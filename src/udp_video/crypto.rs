//! Datagram encryption for the UDP video lane.
//!
//! Cipher: XSalsa20-Poly1305 (sodiumoxide `secretbox`), the same primitive the TCP secure stream
//! uses, in detached mode so the datagram is sealed in place.
//!
//! Keys: one per direction, derived the way the TCP stream derives its split keys
//! (`hbb_common::tcp::Encrypt::derive_subkey`): keyed BLAKE2b (`generichash`, 32-byte output)
//! keyed with the lane secret, over a domain label, the direction and the lane id:
//!
//! ```text
//! key(dir) = BLAKE2b-256(key = lane_secret, "rd-udp-video-v1" || dir || lane_id LE)
//! ```
//!
//! The lane secret is 32 random bytes the host sends in the lane offer over the already
//! encrypted TCP session (M19 second half), so its secrecy rests on the session key exchange.
//! Deriving from the TCP session key itself is not possible without a change in the
//! hbb_common submodule: `Encrypt` keeps its receive key private, and each side can only read
//! its own send key. The domain label keeps these keys apart from every TCP key even if the
//! lane secret were ever taken from TCP key material later.
//!
//! Nonce: `seq u64 LE || dir u8 || 15 zero bytes`. The seq is per direction, starts at 1 and
//! strictly increases; the sender refuses to wrap, so a (key, nonce) pair is never reused. The
//! keys already differ per direction; the direction byte in the nonce is a second, independent
//! guard. The seq travels in the clear and is authenticated by being the nonce.
//!
//! Replay: the receiver keeps a 1024-seq sliding window (WireGuard and DTLS use the same
//! scheme). A seq is recorded only after its tag verified, so forged datagrams cannot move the
//! window.

use super::packet::{read_outer, write_outer, PacketError, OUTER_LEN, TAG_LEN};
use hbb_common::sodiumoxide::crypto::{
    generichash,
    secretbox::{self, Key, Nonce, Tag},
};
use std::fmt;

/// Domain label of the key derivation. The version suffix ties keys to the wire version.
pub const KDF_CONTEXT: &[u8] = b"rd-udp-video-v1";
pub const SECRET_LEN: usize = 32;
/// Seqs remembered behind the highest one.
pub const REPLAY_WINDOW: u64 = 1024;
const WINDOW_WORDS: usize = (REPLAY_WINDOW / 64) as usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction {
    HostToViewer = 0,
    ViewerToHost = 1,
}

impl Direction {
    pub fn reverse(self) -> Self {
        match self {
            Self::HostToViewer => Self::ViewerToHost,
            Self::ViewerToHost => Self::HostToViewer,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Host,
    Viewer,
}

impl Role {
    fn send_direction(self) -> Direction {
        match self {
            Self::Host => Direction::HostToViewer,
            Self::Viewer => Direction::ViewerToHost,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenError {
    Packet(PacketError),
    /// Seq 0, already seen, or more than `REPLAY_WINDOW` behind the highest seq.
    Replay,
    /// Tag did not verify: wrong key, wrong direction, tampered, or not ours.
    Auth,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for OpenError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeqExhausted;

pub fn derive_key(secret: &[u8; SECRET_LEN], dir: Direction, lane_id: u64) -> Key {
    // Fixed, valid sizes (32-byte output and key are within BLAKE2b's 16..=64 and 0..=64), so
    // libsodium cannot fail here.
    let mut h = generichash::State::new(Some(secretbox::KEYBYTES), Some(secret))
        .expect("generichash parameters are valid");
    for part in [KDF_CONTEXT, &[dir as u8], &lane_id.to_le_bytes()] {
        h.update(part).expect("generichash update");
    }
    let digest = h.finalize().expect("generichash finalize");
    let mut key = [0u8; secretbox::KEYBYTES];
    key.copy_from_slice(digest.as_ref());
    Key(key)
}

fn nonce(seq: u64, dir: Direction) -> Nonce {
    let mut n = [0u8; secretbox::NONCEBYTES];
    n[..8].copy_from_slice(&seq.to_le_bytes());
    n[8] = dir as u8;
    Nonce(n)
}

/// 1024-seq sliding replay window. Bit i of `bits` stands for seq `top - i`.
#[derive(Clone)]
pub struct ReplayWindow {
    top: u64,
    bits: [u64; WINDOW_WORDS],
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self {
            top: 0,
            bits: [0; WINDOW_WORDS],
        }
    }
}

impl ReplayWindow {
    fn bit(&self, i: u64) -> bool {
        self.bits[(i / 64) as usize] & (1u64 << (i % 64)) != 0
    }

    /// Whether `seq` would be accepted; changes nothing.
    pub fn check(&self, seq: u64) -> bool {
        if seq == 0 {
            return false;
        }
        if seq > self.top {
            return true;
        }
        let back = self.top - seq;
        back < REPLAY_WINDOW && !self.bit(back)
    }

    /// Records `seq`. Call only after `check` passed and the datagram authenticated.
    pub fn commit(&mut self, seq: u64) {
        if seq > self.top {
            let shift = seq - self.top;
            self.shift_older(shift);
            self.top = seq;
            self.bits[0] |= 1;
        } else {
            let back = self.top - seq;
            if back < REPLAY_WINDOW {
                self.bits[(back / 64) as usize] |= 1u64 << (back % 64);
            }
        }
    }

    /// Moves every recorded seq `n` positions further from the top.
    fn shift_older(&mut self, n: u64) {
        if n >= REPLAY_WINDOW {
            self.bits = [0; WINDOW_WORDS];
            return;
        }
        let words = (n / 64) as usize;
        let shift = (n % 64) as u32;
        for w in (0..WINDOW_WORDS).rev() {
            let src = w.checked_sub(words);
            let lo = src.map_or(0, |s| self.bits[s]);
            let carry = match src.and_then(|s| s.checked_sub(1)) {
                Some(s) if shift > 0 => self.bits[s] >> (64 - shift),
                _ => 0,
            };
            self.bits[w] = (if shift > 0 { lo << shift } else { lo }) | carry;
        }
    }

    pub fn highest(&self) -> u64 {
        self.top
    }
}

/// Seals datagrams in one direction and opens the other.
pub struct LaneCipher {
    send_key: Key,
    recv_key: Key,
    send_dir: Direction,
    send_seq: u64,
    window: ReplayWindow,
}

impl LaneCipher {
    pub fn new(secret: &[u8; SECRET_LEN], lane_id: u64, role: Role) -> Self {
        let send_dir = role.send_direction();
        Self {
            send_key: derive_key(secret, send_dir, lane_id),
            recv_key: derive_key(secret, send_dir.reverse(), lane_id),
            send_dir,
            send_seq: 0,
            window: ReplayWindow::default(),
        }
    }

    /// Seals `inner` into `out` (cleared first): outer header, tag, ciphertext. Returns the seq.
    pub fn seal(&mut self, inner: &[u8], out: &mut Vec<u8>) -> Result<u64, SeqExhausted> {
        let seq = self.send_seq.checked_add(1).ok_or(SeqExhausted)?;
        if seq == u64::MAX {
            // Never hand out the last value either, so the window arithmetic stays simple.
            return Err(SeqExhausted);
        }
        self.send_seq = seq;
        out.clear();
        out.reserve(OUTER_LEN + TAG_LEN + inner.len());
        write_outer(seq, out);
        out.extend_from_slice(&[0u8; TAG_LEN]);
        out.extend_from_slice(inner);
        let tag = secretbox::seal_detached(
            &mut out[OUTER_LEN + TAG_LEN..],
            &nonce(seq, self.send_dir),
            &self.send_key,
        );
        out[OUTER_LEN..OUTER_LEN + TAG_LEN].copy_from_slice(&tag.0);
        Ok(seq)
    }

    /// Verifies and decrypts `datagram` in place. On success returns the seq and the inner
    /// plaintext, a slice of `datagram`.
    pub fn open<'a>(&mut self, datagram: &'a mut [u8]) -> Result<(u64, &'a [u8]), OpenError> {
        let (seq, _) = read_outer(datagram).map_err(OpenError::Packet)?;
        if !self.window.check(seq) {
            return Err(OpenError::Replay);
        }
        let (head, body) = datagram.split_at_mut(OUTER_LEN + TAG_LEN);
        let tag = Tag::from_slice(&head[OUTER_LEN..]).ok_or(OpenError::Auth)?;
        secretbox::open_detached(
            body,
            &tag,
            &nonce(seq, self.send_dir.reverse()),
            &self.recv_key,
        )
        .map_err(|_| OpenError::Auth)?;
        self.window.commit(seq);
        Ok((seq, body))
    }

    /// Highest seq accepted so far (0 before the first).
    pub fn highest_received(&self) -> u64 {
        self.window.highest()
    }

    #[cfg(test)]
    fn set_send_seq(&mut self, seq: u64) {
        self.send_seq = seq;
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;

    const SECRET: [u8; SECRET_LEN] = [7u8; SECRET_LEN];

    fn pair() -> (LaneCipher, LaneCipher) {
        (
            LaneCipher::new(&SECRET, 42, Role::Host),
            LaneCipher::new(&SECRET, 42, Role::Viewer),
        )
    }

    #[test]
    fn round_trip_both_directions() {
        let (mut host, mut viewer) = pair();
        let mut d = Vec::new();
        assert_eq!(host.seal(b"frame", &mut d).unwrap(), 1);
        assert_eq!(d.len(), OUTER_LEN + TAG_LEN + 5);
        assert_eq!(viewer.open(&mut d).unwrap(), (1, &b"frame"[..]));
        viewer.seal(b"ack", &mut d).unwrap();
        assert_eq!(host.open(&mut d).unwrap().1, b"ack");
    }

    #[test]
    fn keys_differ_per_direction_lane_and_secret() {
        let a = derive_key(&SECRET, Direction::HostToViewer, 1);
        assert_ne!(a, derive_key(&SECRET, Direction::ViewerToHost, 1));
        assert_ne!(a, derive_key(&SECRET, Direction::HostToViewer, 2));
        assert_ne!(
            a,
            derive_key(&[8u8; SECRET_LEN], Direction::HostToViewer, 1)
        );
        assert_eq!(a, derive_key(&SECRET, Direction::HostToViewer, 1));
    }

    #[test]
    fn own_datagrams_and_other_lanes_do_not_open() {
        let (mut host, _) = pair();
        let mut d = Vec::new();
        host.seal(b"x", &mut d).unwrap();
        // Reflected back to the sender: its receive key is the other direction's.
        let mut reflected = d.clone();
        assert_eq!(host.open(&mut reflected), Err(OpenError::Auth));
        let mut other = LaneCipher::new(&SECRET, 43, Role::Viewer);
        assert_eq!(other.open(&mut d), Err(OpenError::Auth));
    }

    #[test]
    fn tampering_is_detected_and_does_not_move_the_window() {
        let (mut host, mut viewer) = pair();
        let mut d = Vec::new();
        host.seal(b"payload", &mut d).unwrap();
        // Byte 3 changes the seq to 257: in the window, but the nonce no longer matches.
        for i in [3usize, OUTER_LEN, d.len() - 1] {
            let mut t = d.clone();
            t[i] ^= 1;
            assert_eq!(viewer.open(&mut t), Err(OpenError::Auth), "byte {i}");
        }
        assert_eq!(viewer.highest_received(), 0);
        assert!(viewer.open(&mut d).is_ok());
    }

    #[test]
    fn duplicate_and_replay_rejected() {
        let (mut host, mut viewer) = pair();
        let mut sealed = Vec::new();
        for _ in 0..3 {
            let mut d = Vec::new();
            host.seal(b"v", &mut d).unwrap();
            sealed.push(d);
        }
        let mut first = sealed[0].clone();
        assert!(viewer.open(&mut first).is_ok());
        let mut dup = sealed[0].clone();
        assert_eq!(viewer.open(&mut dup), Err(OpenError::Replay));
        // Reordered but inside the window: accepted once.
        let mut third = sealed[2].clone();
        assert!(viewer.open(&mut third).is_ok());
        let mut second = sealed[1].clone();
        assert!(viewer.open(&mut second).is_ok());
        let mut second = sealed[1].clone();
        assert_eq!(viewer.open(&mut second), Err(OpenError::Replay));
    }

    #[test]
    fn too_old_is_rejected() {
        let (mut host, mut viewer) = pair();
        let mut old = Vec::new();
        host.seal(b"old", &mut old).unwrap();
        host.set_send_seq(REPLAY_WINDOW + 5);
        let mut d = Vec::new();
        host.seal(b"new", &mut d).unwrap();
        assert!(viewer.open(&mut d).is_ok());
        assert_eq!(viewer.open(&mut old), Err(OpenError::Replay));
    }

    #[test]
    fn seq_never_wraps() {
        let (mut host, _) = pair();
        host.set_send_seq(u64::MAX - 2);
        let mut d = Vec::new();
        assert!(host.seal(b"a", &mut d).is_ok());
        assert_eq!(host.seal(b"a", &mut d), Err(SeqExhausted));
        assert_eq!(host.seal(b"a", &mut d), Err(SeqExhausted));
    }

    #[test]
    fn window_matches_a_reference_set() {
        // Pseudo-random arrival order over a range wider than the window, checked against a
        // plain set of seen seqs plus the "within REPLAY_WINDOW of the top" rule.
        let mut w = ReplayWindow::default();
        let mut seen = std::collections::HashSet::new();
        let mut top = 0u64;
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let seq = 1 + (x % 3_000) + top.saturating_sub(1_500);
            let expect =
                seq != 0 && !seen.contains(&seq) && (seq > top || top - seq < REPLAY_WINDOW);
            assert_eq!(w.check(seq), expect, "seq {seq} top {top}");
            if expect {
                w.commit(seq);
                seen.insert(seq);
                top = top.max(seq);
            }
        }
        // Shifts by exact word multiples and by more than the window.
        let mut w = ReplayWindow::default();
        w.commit(1);
        w.commit(65);
        assert!(!w.check(1));
        w.commit(129);
        assert!(!w.check(1) && !w.check(65));
        w.commit(129 + REPLAY_WINDOW);
        assert!(!w.check(129));
        assert!(w.check(130));
    }
}
