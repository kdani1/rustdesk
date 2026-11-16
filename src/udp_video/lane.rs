//! Both ends of the lane without sockets: the sender turns a frame into sealed datagrams, the
//! receiver turns datagrams back into frames, losses and control packets. The live sender
//! thread and receive thread (M19 second half) wrap these around a UdpSocket.

use super::{
    crypto::{LaneCipher, OpenError, Role, SeqExhausted, SECRET_LEN},
    fec,
    fragment::{
        fragment, Event, FragmentError, FrameMeta, LossStats, Reassembler, ReassemblyConfig,
    },
    packet::{decode_inner, encode_inner, Ack, Inner, PathClass},
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

    /// The datagrams of one frame with FEC at `percent` (0: none): each block's data shards,
    /// then its parity shards.
    pub fn frame_fec(
        &mut self,
        frame: &[u8],
        meta: FrameMeta,
        percent: u32,
    ) -> Result<Vec<Vec<u8>>, SendError> {
        let parts =
            fragment(frame, meta, self.path.max_fragment_payload()).map_err(SendError::Fragment)?;
        let (shards, _) = fec::protect(parts, percent);
        let mut out = Vec::with_capacity(shards.len());
        for s in &shards {
            out.push(self.seal(&s.inner())?);
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
    Ack(Ack),
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
            Inner::Fec(h, payload) => {
                self.reassembler.push_parity(&h, payload, now);
                Received::Video
            }
            Inner::Hello { lane_id } => Received::Hello { lane_id },
            Inner::HelloAck { lane_id } => Received::HelloAck { lane_id },
            Inner::Ping { id, send_us } => Received::Ping { id, send_us },
            Inner::Pong { id, echo_us } => Received::Pong { id, echo_us },
            Inner::Ack(a) => Received::Ack(a),
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

    /// Positions of the data and parity datagrams of a frame of `n` fragments sent by
    /// `frame_fec` at `percent`: per block, (data positions, parity positions).
    fn fec_positions(n: usize, percent: u32) -> Vec<(Vec<usize>, Vec<usize>)> {
        let blocks = fec::plan_blocks(n, percent).unwrap();
        let mut at = 0;
        (0..blocks)
            .map(|b| {
                let d = fec::block_range(n, blocks, b).len();
                let p = fec::parity_for(d, percent);
                let data = (at..at + d).collect();
                let parity = (at + d..at + d + p).collect();
                at += d + p;
                (data, parity)
            })
            .collect()
    }

    fn send_except(rx: &mut LaneReceiver, dgrams: &[Vec<u8>], drop: &[usize], now: Instant) {
        for (i, d) in dgrams.iter().enumerate() {
            if !drop.contains(&i) {
                assert_eq!(rx.on_datagram(&mut d.clone(), now), Ok(Received::Video));
            }
        }
    }

    #[test]
    fn latency_fec_rebuilds_lost_fragments_at_once() {
        let (mut tx, mut rx) = ends(PathClass::Lan);
        let t0 = Instant::now();
        // 30 fragments, the last one short: one block with 6 parity shards.
        let f0 = frame_bytes(40_000, 3);
        let d0 = tx.frame_fec(&f0, meta(0, FLAG_KEY), 20).unwrap();
        let pos = fec_positions(30, 20);
        assert_eq!(pos.len(), 1);
        assert_eq!((pos[0].0.len(), pos[0].1.len()), (30, 6));
        assert_eq!(d0.len(), 36);
        // Six data fragments lost, the short last one among them, and no parity.
        send_except(&mut rx, &d0, &[0, 7, 8, 15, 22, 29], t0);
        // Frame 1 loses nothing; its parity comes after it is delivered.
        let f1 = frame_bytes(3_000, 4);
        let d1 = tx.frame_fec(&f1, meta(1, 0), 20).unwrap();
        assert_eq!(d1.len(), 3 + 2);
        send_except(&mut rx, &d1, &[], t0);
        // Frame 2: one fragment, lost, rebuilt from a parity shard.
        let f2 = frame_bytes(100, 5);
        let d2 = tx.frame_fec(&f2, meta(2, 0), 20).unwrap();
        assert_eq!(d2.len(), 1 + 2);
        send_except(&mut rx, &d2, &[0, 1], t0);
        // No timeout needed: everything is there at t0.
        let got = frames_of(&rx.poll(t0));
        assert_eq!(got, vec![(0, f0), (1, f1), (2, f2)]);
        let (loss, _) = rx.take_stats();
        assert_eq!(loss.frames_lost, 0);
        assert_eq!(loss.fec_recovered, 2);
        assert_eq!(loss.fec_shards, 7);
        assert_eq!(loss.fec_unrecoverable, 0);
        assert_eq!(loss.parity_received, 6 + 2 + 1);
        // Parity of a delivered frame is neither late nor a duplicate.
        assert_eq!(
            (loss.fragments_late, loss.fragments_duplicate, loss.fragments_invalid),
            (0, 0, 0)
        );
        assert_eq!(rx.next_deadline(), None);
    }

    #[test]
    fn latency_fec_rebuilt_but_gated_is_not_recovered() {
        let (mut tx, mut rx) = ends(PathClass::Lan);
        let t0 = Instant::now();
        // Frame 0 arrives whole; frame 1 never arrives; frame 2 (a P-frame) loses one fragment
        // that parity rebuilds, but it is gated behind the loss of frame 1: FEC saved nothing
        // the decoder can use.
        let f0 = frame_bytes(5_000, 1);
        let d0 = tx.frame_fec(&f0, meta(0, FLAG_KEY), 20).unwrap();
        send_except(&mut rx, &d0, &[], t0);
        let _ = tx.frame_fec(&frame_bytes(5_000, 2), meta(1, 0), 20).unwrap();
        let d2 = tx.frame_fec(&frame_bytes(5_000, 3), meta(2, 0), 20).unwrap();
        send_except(&mut rx, &d2, &[1], t0);
        let ev = rx.poll(t0 + Duration::from_millis(60));
        assert_eq!(frames_of(&ev), vec![(0, f0)]);
        let (loss, _) = rx.take_stats();
        assert_eq!((loss.frames_lost, loss.frames_gated), (1, 1));
        assert_eq!(loss.fec_shards, 1);
        assert_eq!(loss.fec_recovered, 0);
    }

    #[test]
    fn latency_fec_too_many_lost_is_unrecoverable() {
        let (mut tx, mut rx) = ends(PathClass::Lan);
        let t0 = Instant::now();
        let f0 = frame_bytes(40_000, 6);
        let d0 = tx.frame_fec(&f0, meta(0, FLAG_KEY), 20).unwrap();
        // Seven missing (five data, two parity) against six parity shards: four parity arrive
        // for five holes.
        send_except(&mut rx, &d0, &[1, 2, 3, 4, 5, 30, 31], t0);
        assert!(frames_of(&rx.poll(t0)).is_empty());
        let ev = rx.poll(t0 + Duration::from_millis(60));
        assert!(ev.contains(&Event::Lost {
            display: 0,
            from: 0,
            to: 0
        }));
        let (loss, _) = rx.take_stats();
        assert_eq!(loss.frames_lost, 1);
        assert_eq!(loss.fec_unrecoverable, 1);
        assert_eq!(loss.fec_recovered, 0);
        assert_eq!(loss.fragments_missing, 5);
    }

    #[test]
    fn latency_fec_multi_block_and_reorder() {
        let (mut tx, mut rx) = ends(PathClass::Lan);
        let t0 = Instant::now();
        let n_bytes: usize = 400_000;
        let n = n_bytes.div_ceil(PathClass::Lan.max_fragment_payload());
        let f0 = frame_bytes(n_bytes, 7);
        let d0 = tx.frame_fec(&f0, meta(0, FLAG_KEY), 20).unwrap();
        let pos = fec_positions(n, 20);
        assert_eq!(pos.len(), 2);
        assert_eq!(d0.len(), n + pos[0].1.len() + pos[1].1.len());
        // Block 0 loses as many data shards as it has parity; block 1 loses its last data
        // shard (the short end of the frame) and some parity.
        let mut drop: Vec<usize> = pos[0].0.iter().step_by(3).take(pos[0].1.len()).copied().collect();
        drop.push(*pos[1].0.last().unwrap());
        drop.extend(pos[1].1.iter().take(5));
        // Data in reverse order, then the parity, also reversed. (Parity before data would
        // rebuild a block as soon as the holes left match the parity that came, and the data
        // arriving after that would count as duplicates.)
        let is_parity = |i: &usize| pos.iter().any(|(_, par)| par.contains(i));
        let mut order: Vec<usize> = (0..d0.len()).rev().filter(|i| !is_parity(i)).collect();
        order.extend((0..d0.len()).rev().filter(|i| is_parity(i)));
        order.retain(|i| !drop.contains(i));
        for i in order {
            rx.on_datagram(&mut d0[i].clone(), t0).unwrap();
        }
        let got = frames_of(&rx.poll(t0));
        assert_eq!(got, vec![(0, f0)]);
        let (loss, _) = rx.take_stats();
        assert_eq!(loss.fec_recovered, 1);
        assert_eq!(loss.fec_shards as usize, pos[0].1.len() + 1);
        assert_eq!(loss.frames_lost, 0);
    }

    #[test]
    fn latency_fec_off_is_the_plain_frame() {
        let (mut tx, mut rx) = ends(PathClass::Lan);
        let t0 = Instant::now();
        let f = frame_bytes(10_000, 8);
        let plain = tx.frame(&f, meta(0, FLAG_KEY)).unwrap();
        let off = tx.frame_fec(&f, meta(1, 0), 0).unwrap();
        assert_eq!(plain.len(), off.len());
        send_except(&mut rx, &plain, &[], t0);
        send_except(&mut rx, &off, &[], t0);
        assert_eq!(frames_of(&rx.poll(t0)).len(), 2);
        let (loss, _) = rx.take_stats();
        assert_eq!((loss.parity_received, loss.fec_recovered), (0, 0));
    }
}
