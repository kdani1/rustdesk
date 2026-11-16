//! Both ends of the lane without sockets: the sender turns a frame into sealed datagrams, the
//! receiver turns datagrams back into frames, losses and control packets. The live sender
//! thread and receive thread (M19 second half) wrap these around a UdpSocket.

use super::{
    crypto::{LaneCipher, OpenError, Role, SeqExhausted, SECRET_LEN},
    fragment::{
        fragment, Event, FragmentError, FrameMeta, LossStats, Reassembler, ReassemblyConfig,
    },
    packet::{decode_inner, encode_inner, Inner, PathClass},
};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    Fragment(FragmentError),
    SeqExhausted,
}

impl From<SeqExhausted> for SendError {
    fn from(_: SeqExhausted) -> Self {
        Self::SeqExhausted
    }
}

/// Seals frames and control packets for one direction of a lane.
pub struct LaneSender {
    cipher: LaneCipher,
    path: PathClass,
    inner: Vec<u8>,
}

impl LaneSender {
    pub fn new(secret: &[u8; SECRET_LEN], lane_id: u64, role: Role, path: PathClass) -> Self {
        Self {
            cipher: LaneCipher::new(secret, lane_id, role),
            path,
            inner: Vec::with_capacity(path.max_datagram()),
        }
    }

    pub fn path(&self) -> PathClass {
        self.path
    }

    /// The datagrams of one frame, in send order. Each is at most `path.max_datagram()` bytes.
    pub fn frame(&mut self, frame: &[u8], meta: FrameMeta) -> Result<Vec<Vec<u8>>, SendError> {
        let parts =
            fragment(frame, meta, self.path.max_fragment_payload()).map_err(SendError::Fragment)?;
        let mut out = Vec::with_capacity(parts.len());
        for (h, payload) in parts {
            out.push(self.seal(&Inner::Video(h, payload))?);
        }
        Ok(out)
    }

    /// One sealed control or video datagram.
    pub fn seal(&mut self, inner: &Inner) -> Result<Vec<u8>, SendError> {
        self.inner.clear();
        encode_inner(inner, &mut self.inner);
        let mut d = Vec::new();
        self.cipher.seal(&self.inner, &mut d)?;
        Ok(d)
    }
}

/// What one datagram amounted to, besides video fragments (those surface through `poll`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Received {
    Video,
    Hello { lane_id: u64 },
    HelloAck { lane_id: u64 },
    Ping { id: u32, send_us: u32 },
    Pong { id: u32, echo_us: u32 },
}

/// Datagrams dropped before reassembly, by reason.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DropStats {
    /// Bad magic, version, or truncated: not lane traffic.
    pub malformed: u64,
    pub replayed: u64,
    pub unauthenticated: u64,
    /// Authenticated, but the inner part did not parse.
    pub bad_inner: u64,
}

pub struct LaneReceiver {
    cipher: LaneCipher,
    reassembler: Reassembler,
    drops: DropStats,
}

impl LaneReceiver {
    pub fn new(
        secret: &[u8; SECRET_LEN],
        lane_id: u64,
        role: Role,
        config: ReassemblyConfig,
    ) -> Self {
        Self {
            cipher: LaneCipher::new(secret, lane_id, role),
            reassembler: Reassembler::new(config),
            drops: DropStats::default(),
        }
    }

    pub fn reassembler(&mut self) -> &mut Reassembler {
        &mut self.reassembler
    }

    /// Handles one received datagram (decrypted in place). Errors are counted in `drops` and
    /// also returned, so the caller can log the first of a kind.
    pub fn on_datagram(
        &mut self,
        datagram: &mut [u8],
        now: Instant,
    ) -> Result<Received, OpenError> {
        let (seq, inner) = match self.cipher.open(datagram) {
            Ok(v) => v,
            Err(e) => {
                match e {
                    OpenError::Packet(_) => self.drops.malformed += 1,
                    OpenError::Replay => self.drops.replayed += 1,
                    OpenError::Auth => self.drops.unauthenticated += 1,
                }
                return Err(e);
            }
        };
        let inner = match decode_inner(inner) {
            Ok(i) => i,
            Err(e) => {
                self.drops.bad_inner += 1;
                return Err(OpenError::Packet(e));
            }
        };
        self.reassembler.note_seq(seq);
        Ok(match inner {
            Inner::Video(h, payload) => {
                self.reassembler.push(&h, payload, now);
                Received::Video
            }
            Inner::Hello { lane_id } => Received::Hello { lane_id },
            Inner::HelloAck { lane_id } => Received::HelloAck { lane_id },
            Inner::Ping { id, send_us } => Received::Ping { id, send_us },
            Inner::Pong { id, echo_us } => Received::Pong { id, echo_us },
        })
    }

    pub fn poll(&mut self, now: Instant) -> Vec<Event> {
        self.reassembler.poll(now)
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.reassembler.next_deadline()
    }

    /// Loss and drop counters since the last call.
    pub fn take_stats(&mut self) -> (LossStats, DropStats) {
        (
            self.reassembler.take_stats(),
            std::mem::take(&mut self.drops),
        )
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;
    use crate::udp_video::packet::FLAG_KEY;
    use std::time::Duration;

    const SECRET: [u8; SECRET_LEN] = [0x5a; SECRET_LEN];

    fn ends(path: PathClass) -> (LaneSender, LaneReceiver) {
        (
            LaneSender::new(&SECRET, 9, Role::Host, path),
            LaneReceiver::new(&SECRET, 9, Role::Viewer, Default::default()),
        )
    }

    fn frame_bytes(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8) ^ seed).collect()
    }

    fn meta(frame_id: u32, flags: u8) -> FrameMeta {
        FrameMeta {
            frame_id,
            flags,
            ..Default::default()
        }
    }

    fn frames_of(events: &[Event]) -> Vec<(u32, Vec<u8>)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Frame(f) => Some((f.frame_id, f.data.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn round_trip_on_every_path_class() {
        for path in [PathClass::Lan, PathClass::WanV4, PathClass::WanV6] {
            let (mut tx, mut rx) = ends(path);
            let now = Instant::now();
            let sent: Vec<_> = (0..3u32)
                .map(|i| frame_bytes(40_000 + 777 * i as usize, i as u8))
                .collect();
            for (i, f) in sent.iter().enumerate() {
                let dgrams = tx.frame(f, meta(i as u32, FLAG_KEY)).unwrap();
                assert!(dgrams.iter().all(|d| d.len() <= path.max_datagram()));
                assert!(dgrams[..dgrams.len() - 1]
                    .iter()
                    .all(|d| d.len() == path.max_datagram()));
                for mut d in dgrams {
                    assert_eq!(rx.on_datagram(&mut d, now), Ok(Received::Video));
                }
            }
            let got = frames_of(&rx.poll(now));
            assert_eq!(got.len(), 3, "{path:?}");
            for (i, (id, data)) in got.into_iter().enumerate() {
                assert_eq!(id, i as u32);
                assert_eq!(data, sent[i]);
            }
        }
    }

    #[test]
    fn loss_reorder_duplicate_and_replay() {
        let (mut tx, mut rx) = ends(PathClass::Lan);
        let t0 = Instant::now();
        let f0 = frame_bytes(10_000, 1);
        let f1 = frame_bytes(10_000, 2);
        let f2 = frame_bytes(10_000, 3);
        let d0 = tx.frame(&f0, meta(0, FLAG_KEY)).unwrap();
        let d1 = tx.frame(&f1, meta(1, 0)).unwrap();
        let d2 = tx.frame(&f2, meta(2, FLAG_KEY)).unwrap();
        // Frame 0 reversed (reorder) with its first datagram twice (duplicate = replay).
        for d in d0.iter().rev().chain(std::iter::once(&d0[0])) {
            let _ = rx.on_datagram(&mut d.clone(), t0);
        }
        // Frame 1 loses one datagram.
        for (i, d) in d1.iter().enumerate() {
            if i != 3 {
                rx.on_datagram(&mut d.clone(), t0).unwrap();
            }
        }
        for d in &d2 {
            rx.on_datagram(&mut d.clone(), t0 + Duration::from_millis(1))
                .unwrap();
        }
        let ev = rx.poll(t0 + Duration::from_millis(10));
        assert_eq!(
            ev.iter()
                .filter_map(|e| match e {
                    Event::Frame(f) => Some(f.frame_id),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert!(ev.contains(&Event::Lost {
            display: 0,
            from: 1,
            to: 1
        }));
        let (loss, drops) = rx.take_stats();
        assert_eq!(drops.replayed, 1);
        assert_eq!(loss.frames_lost, 1);
        assert_eq!(loss.fragments_missing, 1);
        assert_eq!(loss.reorder_events as usize, d0.len() - 1);
        let total = (d0.len() + d1.len() + d2.len()) as u64;
        assert_eq!(loss.datagrams_expected, total);
        assert_eq!(loss.datagrams_received, total - 1);
        // An old datagram replayed later is still refused.
        assert_eq!(
            rx.on_datagram(&mut d1[0].clone(), t0),
            Err(OpenError::Replay)
        );
    }

    #[test]
    fn control_packets_and_foreign_traffic() {
        let (mut tx, mut rx) = ends(PathClass::Lan);
        let now = Instant::now();
        let mut d = tx.seal(&Inner::Ping { id: 5, send_us: 6 }).unwrap();
        assert_eq!(
            rx.on_datagram(&mut d, now),
            Ok(Received::Ping { id: 5, send_us: 6 })
        );
        // A punch probe or STUN packet on the same socket.
        let mut probe = b"RDP?\x01\x02\x03\x04\x05\x06\x07\x08".to_vec();
        assert!(matches!(
            rx.on_datagram(&mut probe, now),
            Err(OpenError::Packet(_))
        ));
        // Another lane's datagram. Seq 1 is already taken, and the replay check comes before
        // the (dearer) tag check, so use a fresh seq to reach the tag check.
        let mut other = LaneSender::new(&SECRET, 10, Role::Host, PathClass::Lan);
        other.seal(&Inner::Hello { lane_id: 10 }).unwrap();
        let mut d = other.seal(&Inner::Hello { lane_id: 10 }).unwrap();
        assert_eq!(rx.on_datagram(&mut d, now), Err(OpenError::Auth));
        // The viewer's own Hello reflected back to it does not open.
        let mut viewer_tx = LaneSender::new(&SECRET, 9, Role::Viewer, PathClass::Lan);
        for _ in 0..2 {
            viewer_tx.seal(&Inner::Hello { lane_id: 9 }).unwrap();
        }
        let mut d = viewer_tx.seal(&Inner::Hello { lane_id: 9 }).unwrap();
        let mut reflected = d.clone();
        assert_eq!(rx.on_datagram(&mut reflected, now), Err(OpenError::Auth));
        let mut host_rx = LaneReceiver::new(&SECRET, 9, Role::Host, Default::default());
        assert_eq!(
            host_rx.on_datagram(&mut d, now),
            Ok(Received::Hello { lane_id: 9 })
        );
        let (_, drops) = rx.take_stats();
        assert_eq!(drops.malformed, 1);
        assert_eq!(drops.unauthenticated, 2);
        // A forged datagram with a fresh seq fails the tag check and leaves the window alone.
        assert_eq!(rx.cipher.highest_received(), 1);
    }
}
