use hbb_common::ResultType;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, MutexGuard,
};

pub use base::config::keys::OPTION_ALLOW_FORWARDED_MICROPHONE as ALLOW;
pub const TOGGLE: &str = "forward-microphone";
/// Sent to the viewer whose forwarding was stopped because another connection turned it on.
pub const TAKEN_OVER: &str = "Microphone taken over by another device";

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as platform;
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod platform {
    use hbb_common::{bail, ResultType};
    pub struct Route;
    impl Route {
        pub fn open() -> ResultType<Self> {
            bail!("This platform cannot receive a virtual microphone")
        }
        pub fn output(&self) -> &str {
            ""
        }
        pub fn restore(&mut self) {}
    }
}

/// What the shared route needs from a platform route once it is open.
trait Restore: Send {
    fn restore(&mut self);
}
impl Restore for platform::Route {
    fn restore(&mut self) {
        platform::Route::restore(self)
    }
}

// Fork: one virtual microphone route for the whole process (Windows has one default capture
// device and one VB-CABLE). The newest connection that turns forwarding on owns it; an older
// owner's lease goes stale, and only the current owner restores the route when it lets go,
// so a takeover never switches the default capture device back and forth.
struct Shared {
    route: Box<dyn Restore>,
    output: String,
    owner_conn: i32,
}
static ROUTE: Mutex<Option<Shared>> = Mutex::new(None);
// Token of the current owner's lease, 0 when nobody owns the route. Changed under ROUTE.
static CURRENT: AtomicU64 = AtomicU64::new(0);
static NEXT: AtomicU64 = AtomicU64::new(1);

fn route_lock() -> MutexGuard<'static, Option<Shared>> {
    ROUTE.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct RouteLease {
    token: u64,
    output: String,
}
impl RouteLease {
    /// Opens the route, or takes it over from the connection that owns it now (the route stays
    /// switched). Returns the lease and the connection it was taken from. Blocking (COM on
    /// Windows): call it from `spawn_blocking`.
    pub fn open_or_take_over(conn_id: i32) -> ResultType<(Self, Option<i32>)> {
        Self::open_with(conn_id, || {
            let route = platform::Route::open()?;
            let output = route.output().to_owned();
            Ok((Box::new(route) as Box<dyn Restore>, output))
        })
    }
    fn open_with(
        conn_id: i32,
        open: impl FnOnce() -> ResultType<(Box<dyn Restore>, String)>,
    ) -> ResultType<(Self, Option<i32>)> {
        let mut shared = route_lock();
        let previous = match shared.as_mut() {
            Some(s) => Some(std::mem::replace(&mut s.owner_conn, conn_id)),
            None => {
                let (route, output) = open()?;
                *shared = Some(Shared {
                    route,
                    output,
                    owner_conn: conn_id,
                });
                None
            }
        };
        let token = NEXT.fetch_add(1, Ordering::AcqRel);
        CURRENT.store(token, Ordering::Release);
        let output = shared.as_ref().map(|s| s.output.clone()).unwrap_or_default();
        Ok((Self { token, output }, previous.filter(|p| *p != conn_id)))
    }
    /// False once another connection has taken the route over.
    pub fn is_current(&self) -> bool {
        CURRENT.load(Ordering::Acquire) == self.token
    }
    pub fn output(&self) -> &str {
        &self.output
    }
}
impl Drop for RouteLease {
    fn drop(&mut self) {
        let mut shared = route_lock();
        if CURRENT
            .compare_exchange(self.token, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // The route is restored before another connection can open it again.
            if let Some(mut s) = shared.take() {
                s.route.restore();
            }
        }
    }
}

pub fn message(
    request_id: i64,
    enabled: bool,
    response: bool,
    error: &str,
) -> base::message_proto::Message {
    use base::message_proto::{Message, MicrophoneForwarding, Misc};
    let mut misc = Misc::new();
    misc.set_microphone_forwarding(MicrophoneForwarding {
        request_id,
        enabled,
        response,
        error: error.to_owned(),
        ..Default::default()
    });
    let mut message = Message::new();
    message.set_misc(misc);
    message
}

static CAPTURE_USERS: Mutex<usize> = Mutex::new(0);
pub fn capture_in_use() -> bool {
    *CAPTURE_USERS.lock().unwrap() != 0
}
pub struct CaptureLease(());
impl CaptureLease {
    #[cfg(not(target_os = "ios"))]
    pub fn acquire(input: Option<String>) -> Option<Self> {
        let mut users = CAPTURE_USERS.lock().unwrap();
        if *users == 0 {
            if crate::audio_service::get_voice_call_input_device().is_some() {
                return None;
            }
            crate::audio_service::set_voice_call_input_device(input, false);
        }
        *users += 1;
        Some(Self(()))
    }
}
impl Drop for CaptureLease {
    fn drop(&mut self) {
        let mut users = CAPTURE_USERS.lock().unwrap();
        *users -= 1;
        #[cfg(not(target_os = "ios"))]
        if *users == 0 {
            crate::audio_service::set_voice_call_input_device(None, true);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use base::message_proto::{message, misc, Message};
    use hbb_common::protobuf::Message as ProtobufMessage;

    #[test]
    #[cfg(not(target_os = "ios"))]
    fn forwarding_connections_share_capture_until_the_last_disconnect() {
        let first = CaptureLease::acquire(Some("first microphone".to_owned())).unwrap();
        let second = CaptureLease::acquire(Some("other microphone".to_owned())).unwrap();
        assert!(capture_in_use());
        assert_eq!(crate::audio_service::get_voice_call_input_device().as_deref(), Some("first microphone"));
        drop(first);
        assert!(capture_in_use());
        assert_eq!(crate::audio_service::get_voice_call_input_device().as_deref(), Some("first microphone"));
        drop(second);
        assert!(!capture_in_use());
        assert!(crate::audio_service::get_voice_call_input_device().is_none());
        crate::audio_service::set_voice_call_input_device(Some("voice call".to_owned()), false);
        assert!(CaptureLease::acquire(Some("microphone".to_owned())).is_none());
        assert_eq!(crate::audio_service::get_voice_call_input_device().as_deref(), Some("voice call"));
        crate::audio_service::set_voice_call_input_device(None, true);
    }
    #[test]
    fn newest_connection_takes_the_route_over_and_only_the_owner_restores_it() {
        use std::sync::{atomic::AtomicUsize, Arc};
        struct Fake(Arc<AtomicUsize>);
        impl Restore for Fake {
            fn restore(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let restored = Arc::new(AtomicUsize::new(0));
        let opened = Arc::new(AtomicUsize::new(0));
        let open = |restored: &Arc<AtomicUsize>, opened: &Arc<AtomicUsize>| {
            let (restored, opened) = (restored.clone(), opened.clone());
            move || {
                opened.fetch_add(1, Ordering::SeqCst);
                Ok((Box::new(Fake(restored)) as Box<dyn Restore>, "fake".to_owned()))
            }
        };
        let (first, previous) = RouteLease::open_with(1, open(&restored, &opened)).unwrap();
        assert_eq!(previous, None);
        assert!(first.is_current());
        assert_eq!(first.output(), "fake");
        let (second, previous) = RouteLease::open_with(2, open(&restored, &opened)).unwrap();
        assert_eq!(previous, Some(1));
        assert!(!first.is_current());
        assert!(second.is_current());
        assert_eq!(opened.load(Ordering::SeqCst), 1, "a takeover keeps the open route");
        drop(first);
        assert_eq!(restored.load(Ordering::SeqCst), 0, "a stale lease does not restore");
        assert!(second.is_current());
        drop(second);
        assert_eq!(restored.load(Ordering::SeqCst), 1);
        let (third, previous) = RouteLease::open_with(3, open(&restored, &opened)).unwrap();
        assert_eq!(previous, None);
        assert_eq!(opened.load(Ordering::SeqCst), 2);
        let failed = RouteLease::open_with(4, || -> ResultType<(Box<dyn Restore>, String)> {
            hbb_common::bail!("unused while the route is open")
        });
        let (fourth, previous) = failed.unwrap();
        assert_eq!(previous, Some(3));
        drop(third);
        drop(fourth);
        assert_eq!(restored.load(Ordering::SeqCst), 2);
        assert!(RouteLease::open_with(5, || -> ResultType<(Box<dyn Restore>, String)> {
            hbb_common::bail!("no driver")
        })
        .is_err());
    }
    #[test]
    fn forwarding_response_preserves_session_and_error_without_legacy_audio() {
        let wire = message(123, false, true, "Permission revoked")
            .write_to_bytes()
            .unwrap();
        let decoded = Message::parse_from_bytes(&wire).unwrap();
        match decoded.union {
            Some(message::Union::Misc(value)) => match value.union {
                Some(misc::Union::MicrophoneForwarding(response)) => {
                    assert_eq!(response.request_id, 123);
                    assert!(!response.enabled);
                    assert!(response.response);
                    assert!(!response.ready);
                    assert_eq!(response.error, "Permission revoked");
                }
                _ => panic!("Expected isolated forwarding control"),
            },
            _ => panic!("Forwarding control must not be a legacy audio frame"),
        }
    }
}
