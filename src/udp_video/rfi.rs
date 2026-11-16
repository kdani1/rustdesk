//! Reference frame invalidation (roadmap M22), the host's state between the connections and the
//! capture loops. See docs/UDP-VIDEO.md, "RFI (M22)".
//!
//! - A connection whose live lane lost frames of a display asks that display's capture loop to
//!   invalidate everything from the first lost frame's pts on ([`request`]); requests of several
//!   lanes for one display merge to the oldest pts.
//! - The capture loop takes the request ([`take_request`]) and calls the encoder. Done: the next
//!   frame it encodes references only frames older than that pts; it records the frame here
//!   ([`register`]) before it hands it to the connections. Not possible: it forces a key frame.
//! - Each lane's tap looks the frame up ([`recovery_bound`]) and flags it `RECOVERY_POINT` only
//!   when the bound is at or before the first frame its own viewer lost: one viewer's recovery
//!   frame may reference a frame another viewer lost.
//!
//! Platform-neutral; only Windows hosts (NVENC through hwcodec's NV SDK driver) ever take a
//! request. Display indexes are the capture loop's (monitors only).

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

/// Recovery frames remembered for the taps (a frame is looked up when the connection takes it
/// off its video queue, a few frames after it was encoded at most).
const RECOVERY_KEPT: usize = 16;

/// A request is waiting: the capture loops' check is one atomic load when nothing is asked.
static PENDING: AtomicBool = AtomicBool::new(false);
/// (display, oldest pts to invalidate from).
static REQUESTS: Mutex<Vec<(usize, i64)>> = Mutex::new(Vec::new());
/// (display, pts of the recovery frame, bound: it references only frames with a smaller pts).
static RECOVERY: Mutex<VecDeque<(usize, i64, i64)>> = Mutex::new(VecDeque::new());

/// A lane lost frames of `display` from the one with `from_pts` on.
pub fn request(display: usize, from_pts: i64) {
    if let Ok(mut r) = REQUESTS.lock() {
        match r.iter_mut().find(|(d, _)| *d == display) {
            Some((_, pts)) => *pts = (*pts).min(from_pts),
            None => r.push((display, from_pts)),
        }
        PENDING.store(true, Ordering::Release);
    }
}

/// The capture loop of `display`: the pts to invalidate from, if a lane asked since.
#[inline]
pub fn take_request(display: usize) -> Option<i64> {
    if !PENDING.load(Ordering::Acquire) {
        return None;
    }
    let mut r = REQUESTS.lock().ok()?;
    let i = r.iter().position(|(d, _)| *d == display)?;
    let (_, pts) = r.swap_remove(i);
    if r.is_empty() {
        PENDING.store(false, Ordering::Release);
    }
    Some(pts)
}

/// The capture loop encoded the recovery frame `pts` of `display` after invalidating from
/// `bound` on; called before the frame goes to the connections.
pub fn register(display: usize, pts: i64, bound: i64) {
    if let Ok(mut r) = RECOVERY.lock() {
        r.retain(|(d, p, _)| !(*d == display && *p == pts));
        r.push_back((display, pts, bound));
        while r.len() > RECOVERY_KEPT {
            r.pop_front();
        }
    }
}

/// The bound of the recovery frame `pts` of `display`, if it is one.
pub fn recovery_bound(display: usize, pts: i64) -> Option<i64> {
    let r = RECOVERY.lock().ok()?;
    r.iter()
        .rev()
        .find(|(d, p, _)| *d == display && *p == pts)
        .map(|(_, _, b)| *b)
}

/// A capture loop of `display` (re)starts: its pts start again from 0, so earlier requests and
/// recovery frames no longer name its frames.
pub fn reset_display(display: usize) {
    if let Ok(mut r) = REQUESTS.lock() {
        r.retain(|(d, _)| *d != display);
        if r.is_empty() {
            PENDING.store(false, Ordering::Release);
        }
    }
    if let Ok(mut r) = RECOVERY.lock() {
        r.retain(|(d, _, _)| *d != display);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The statics are shared by every test of the binary: each test uses its own displays.

    #[test]
    fn latency_rfi_requests_merge_to_the_oldest() {
        request(901, 500);
        request(901, 450);
        request(901, 480);
        request(902, 10);
        assert_eq!(take_request(901), Some(450));
        assert_eq!(take_request(901), None);
        assert_eq!(take_request(902), Some(10));
        assert_eq!(take_request(902), None);
    }

    #[test]
    fn latency_rfi_recovery_lookup_and_reset() {
        register(911, 1000, 900);
        register(912, 1000, 950);
        assert_eq!(recovery_bound(911, 1000), Some(900));
        assert_eq!(recovery_bound(911, 1001), None);
        assert_eq!(recovery_bound(912, 1000), Some(950));
        request(911, 5);
        reset_display(911);
        assert_eq!(recovery_bound(911, 1000), None);
        assert_eq!(take_request(911), None);
        assert_eq!(recovery_bound(912, 1000), Some(950));
    }
}
