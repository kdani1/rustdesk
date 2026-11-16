//! Reference frame invalidation (roadmap M22), the host's state between the connections and the
//! capture loops. See docs/UDP-VIDEO.md, "RFI (M22)".
//!
//! - A live lane whose viewer lost frames of a display asks that display's capture loop to
//!   invalidate everything from the first lost frame's pts on ([`request`]); requests of several
//!   lanes for one display merge to the oldest pts when the loop takes them.
//! - The capture loop takes the request ([`take_request`]) and calls the encoder. Done: the next
//!   frame it encodes references only frames older than that pts; it records the frame here
//!   ([`register`]) before it hands it to the connections. Not possible: it forces a key frame.
//! - Each lane's tap looks the frame up ([`recovery_bound`]) and flags it `RECOVERY_POINT` only
//!   when the bound is at or before the first frame its own viewer lost: one viewer's recovery
//!   frame may reference a frame another viewer lost. A lane whose wait ended (a recovery point
//!   or a key frame) takes back its request if the loop has not taken it yet ([`withdraw`]):
//!   another lane's recovery frame often repairs the same burst, and invalidating again from a
//!   later lost frame would find no valid reference left and force a key frame.
//! - Requests older than [`REQUEST_MAX_AGE`] are dropped (a display whose capture loop stopped
//!   never takes its own), so the capture loops' check goes back to one atomic load.
//!
//! Platform-neutral; only Windows hosts (NVENC through hwcodec's NV SDK driver) ever take a
//! request. Display indexes are the capture loop's (monitors only).

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

/// Recovery frames remembered for the taps (a frame is looked up when the connection takes it
/// off its video queue, a few frames after it was encoded at most).
const RECOVERY_KEPT: usize = 16;

/// A request not taken within this is dropped: its capture loop is not running (the viewer's
/// repeated loss report asks again while it still waits).
pub const REQUEST_MAX_AGE: Duration = Duration::from_secs(1);

/// A request is waiting: the capture loops' check is one atomic load when nothing is asked.
static PENDING: AtomicBool = AtomicBool::new(false);
/// (display, lane, oldest pts to invalidate from, when it was asked first).
static REQUESTS: Mutex<Vec<(usize, u64, i64, Instant)>> = Mutex::new(Vec::new());
/// (display, pts of the recovery frame, bound: it references only frames with a smaller pts).
static RECOVERY: Mutex<VecDeque<(usize, i64, i64)>> = Mutex::new(VecDeque::new());
static NEXT_LANE: AtomicU64 = AtomicU64::new(1);

/// A process-unique id for a lane's requests ([`request`], [`withdraw`]).
pub fn new_lane() -> u64 {
    NEXT_LANE.fetch_add(1, Ordering::Relaxed)
}

/// `lane` lost frames of `display` from the one with `from_pts` on.
pub fn request(display: usize, lane: u64, from_pts: i64) {
    if let Ok(mut r) = REQUESTS.lock() {
        match r.iter_mut().find(|(d, l, _, _)| *d == display && *l == lane) {
            Some((_, _, pts, _)) => *pts = (*pts).min(from_pts),
            None => r.push((display, lane, from_pts, Instant::now())),
        }
        PENDING.store(true, Ordering::Release);
    }
}

/// The wait of `lane` for `display` ended (recovery point or key frame): its request, if the
/// capture loop has not taken it yet, is not needed any more.
pub fn withdraw(display: usize, lane: u64) {
    if !PENDING.load(Ordering::Acquire) {
        return;
    }
    if let Ok(mut r) = REQUESTS.lock() {
        r.retain(|(d, l, _, _)| !(*d == display && *l == lane));
        if r.is_empty() {
            PENDING.store(false, Ordering::Release);
        }
    }
}

/// The capture loop of `display`: the pts to invalidate from (the oldest of every lane's
/// request), if a lane asked since. Drops requests older than [`REQUEST_MAX_AGE`] of any display.
#[inline]
pub fn take_request(display: usize) -> Option<i64> {
    if !PENDING.load(Ordering::Acquire) {
        return None;
    }
    let now = Instant::now();
    let mut r = REQUESTS.lock().ok()?;
    let mut from = None;
    r.retain(|&(d, _, pts, at)| {
        if d == display {
            from = Some(from.map_or(pts, |f: i64| f.min(pts)));
            false
        } else {
            now.saturating_duration_since(at) < REQUEST_MAX_AGE
        }
    });
    if r.is_empty() {
        PENDING.store(false, Ordering::Release);
    }
    from
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
        r.retain(|(d, _, _, _)| *d != display);
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
        request(901, 1, 500);
        request(901, 2, 450);
        request(901, 1, 480);
        request(902, 1, 10);
        assert_eq!(take_request(901), Some(450));
        assert_eq!(take_request(901), None);
        assert_eq!(take_request(902), Some(10));
        assert_eq!(take_request(902), None);
    }

    /// A lane whose wait ended takes back only its own request; a request nobody takes is
    /// dropped after `REQUEST_MAX_AGE`.
    #[test]
    fn latency_rfi_withdraw_and_stale_requests() {
        request(921, 1, 10);
        request(921, 2, 12);
        withdraw(921, 1);
        assert_eq!(take_request(921), Some(12));
        request(922, 1, 7);
        withdraw(922, 1);
        assert_eq!(take_request(922), None);
        // Entries of other tests of the binary are fresh: only this one is old.
        let Some(old) = Instant::now().checked_sub(REQUEST_MAX_AGE + Duration::from_millis(10))
        else {
            return;
        };
        REQUESTS.lock().unwrap().push((923, 1, 5, old));
        PENDING.store(true, Ordering::Release);
        assert_eq!(take_request(924), None);
        assert_eq!(take_request(923), None);
    }

    #[test]
    fn latency_rfi_recovery_lookup_and_reset() {
        register(911, 1000, 900);
        register(912, 1000, 950);
        assert_eq!(recovery_bound(911, 1000), Some(900));
        assert_eq!(recovery_bound(911, 1001), None);
        assert_eq!(recovery_bound(912, 1000), Some(950));
        request(911, 1, 5);
        reset_display(911);
        assert_eq!(recovery_bound(911, 1000), None);
        assert_eq!(take_request(911), None);
        assert_eq!(recovery_bound(912, 1000), Some(950));
    }
}
