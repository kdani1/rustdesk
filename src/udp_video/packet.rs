//! Wire format of one UDP video datagram (version 1). See docs/UDP-VIDEO.md.
//!
//! ```text
//! outer, in the clear (10 B):   magic u8 | version u8 | seq u64 LE
//! sealed (16 B tag + inner):    tag[16] | inner (encrypted)
//! inner:                        type u8 | type-specific fields, little-endian
//! ```
//!
//! The seq is the nonce, so it is authenticated even though it travels in the clear; magic and
//! version only select how the rest is read, and the version is also bound into the key
//! derivation (`crypto::KDF_CONTEXT`), so a datagram of another version cannot open.

use std::fmt;

/// First byte of every datagram. Arbitrary, chosen to differ from a STUN header (0x00/0x01), a
/// DTLS record (20..=63) and the `punch_udp` probe bytes, so a stray packet on the same socket
/// is dropped before any crypto work.
pub const MAGIC: u8 = 0xD7;
pub const VERSION: u8 = 1;
/// magic + version + seq.
pub const OUTER_LEN: usize = 10;
/// Poly1305 tag of the secretbox (XSalsa20-Poly1305).
pub const TAG_LEN: usize = 16;
/// type + VideoHeader fields.
pub const VIDEO_HEADER_LEN: usize = 22;
/// Bytes a video datagram adds around its fragment payload.
pub const VIDEO_OVERHEAD: usize = OUTER_LEN + TAG_LEN + VIDEO_HEADER_LEN;

/// `VideoHeader::flags`: the frame is a keyframe (IDR).
pub const FLAG_KEY: u8 = 0x01;
/// The frame references only frames the viewer is known to have (RFI recovery, M22).
pub const FLAG_RECOVERY_POINT: u8 = 0x02;
/// Shadow copy (M19): the same frame also went over TCP; measure, never decode.
pub const FLAG_SHADOW: u8 = 0x04;

/// Datagram size budgets. The numbers are whole UDP payloads, so the IP packet is the budget
/// plus 8 (UDP) and 20 or 40 (IP) bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathClass {
    /// 1400 + 8 + 40 = 1448 <= 1500 even over IPv6; no PPPoE or VPN on a home LAN.
    Lan,
    /// Internet over IPv4: 1024, Moonlight's conservative value (tunnels, PPPoE, mobile).
    WanV4,
    /// Internet over IPv6: 1184 (1232 minimum-MTU-safe minus headers), Moonlight's value.
    WanV6,
}

impl PathClass {
    pub fn max_datagram(self) -> usize {
        match self {
            Self::Lan => 1400,
            Self::WanV4 => 1024,
            Self::WanV6 => 1184,
        }
    }

    /// The `path=` field of the lane log lines.
    pub fn label(self) -> &'static str {
        match self {
            Self::Lan => "lan",
            Self::WanV4 => "wan4",
            Self::WanV6 => "wan6",
        }
    }

    /// Fragment payload per video datagram.
    pub fn max_fragment_payload(self) -> usize {
        self.max_datagram() - VIDEO_OVERHEAD
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    /// Viewer -> host, first datagram on the lane: opens the viewer's firewall/NAT state and
    /// tells the host where to send. Carries the lane id from the (TCP) offer.
    Hello = 1,
    HelloAck = 2,
    Video = 3,
    /// A Reed-Solomon parity shard of a frame (M21): `VideoHeader` fields as for `Video`, with
    /// `frag_index` the parity index within its block and `frag_count` the frame's data
    /// fragments. Sent only on a lane whose offer had `fec_percent` > 0.
    Fec = 4,
    Ping = 5,
    Pong = 6,
    /// Viewer -> host, live mode (M20): how far a display's frames are settled, and the
    /// timing of its last complete frame.
    Ack = 7,
}

impl PacketType {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Hello,
            2 => Self::HelloAck,
            3 => Self::Video,
            4 => Self::Fec,
            5 => Self::Ping,
            6 => Self::Pong,
            7 => Self::Ack,
            _ => return None,
        })
    }
}

/// Per-fragment header of a video frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct VideoHeader {
    /// Per display, +1 per encoded frame; compared with wrapping arithmetic.
    pub frame_id: u32,
    pub display: u8,
    /// Bumped by the host on every SwitchDisplay or video-service restart, so the viewer can
    /// hold frames that belong to a display change it has not read on TCP yet (M20).
    pub epoch: u8,
    /// FLAG_*.
    pub flags: u8,
    pub frag_index: u16,
    pub frag_count: u16,
    /// FEC (M21): `(block count << 4) | block index` of the Reed-Solomon block this shard
    /// belongs to (`fec::block_byte`); 0 = the frame has no FEC.
    pub fec_block: u8,
    /// FEC (M21): parity shards of this block; 0 without FEC.
    pub fec_parity: u8,
    /// Bytes of the whole frame (the serialized protobuf Message).
    pub frame_len: u32,
    /// Host monotonic microseconds (mod 2^32) when the frame was handed to the sender, for
    /// one-way-delay variation and the `udp_ahead` measurement.
    pub send_us: u32,
}

/// Viewer -> host, live mode: the state of one display's reassembly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Ack {
    pub display: u8,
    /// Every frame up to this id (wrapping) was delivered, dropped behind a loss, or declared
    /// lost. Only valid with `has_settled`.
    pub settled: u32,
    pub has_settled: bool,
    /// `send_us` of the last complete frame (0 with `hold_us` u32::MAX: none yet).
    pub echo_send_us: u32,
    /// Viewer time from that frame's first fragment to this ack, so the host can take it out of
    /// its round trip.
    pub hold_us: u32,
    /// Frames of this display declared lost since the lane started (wrapping).
    pub lost_total: u32,
}

/// `Ack::hold_us` when no frame completed yet.
pub const ACK_NO_FRAME: u32 = u32::MAX;

/// The decrypted inner part of a datagram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inner<'a> {
    Hello { lane_id: u64 },
    HelloAck { lane_id: u64 },
    Video(VideoHeader, &'a [u8]),
    /// A parity shard (M21); the header as for `Video`, see `PacketType::Fec`.
    Fec(VideoHeader, &'a [u8]),
    Ping { id: u32, send_us: u32 },
    Pong { id: u32, echo_us: u32 },
    Ack(Ack),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketError {
    TooShort,
    BadMagic,
    BadVersion(u8),
    UnknownType(u8),
    /// Fields that cannot be valid: frag_index >= frag_count, frag_count 0, trailing bytes, a
    /// parity shard without FEC fields or with frag_index >= fec_parity.
    BadHeader,
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for PacketError {}

/// Writes the clear outer header.
pub fn write_outer(seq: u64, out: &mut Vec<u8>) {
    out.push(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&seq.to_le_bytes());
}

/// Reads the clear outer header: the seq, and the sealed rest (tag + ciphertext).
pub fn read_outer(datagram: &[u8]) -> Result<(u64, &[u8]), PacketError> {
    if datagram.len() < OUTER_LEN + TAG_LEN + 1 {
        return Err(PacketError::TooShort);
    }
    if datagram[0] != MAGIC {
        return Err(PacketError::BadMagic);
    }
    if datagram[1] != VERSION {
        return Err(PacketError::BadVersion(datagram[1]));
    }
    let mut seq = [0u8; 8];
    seq.copy_from_slice(&datagram[2..10]);
    Ok((u64::from_le_bytes(seq), &datagram[OUTER_LEN..]))
}

/// Appends the inner (plaintext) encoding of `inner` to `out`.
pub fn encode_inner(inner: &Inner, out: &mut Vec<u8>) {
    match inner {
        Inner::Hello { lane_id } => {
            out.push(PacketType::Hello as u8);
            out.extend_from_slice(&lane_id.to_le_bytes());
        }
        Inner::HelloAck { lane_id } => {
            out.push(PacketType::HelloAck as u8);
            out.extend_from_slice(&lane_id.to_le_bytes());
        }
        Inner::Video(h, payload) | Inner::Fec(h, payload) => {
            out.push(if matches!(inner, Inner::Fec(..)) {
                PacketType::Fec as u8
            } else {
                PacketType::Video as u8
            });
            out.extend_from_slice(&h.frame_id.to_le_bytes());
            out.push(h.display);
            out.push(h.epoch);
            out.push(h.flags);
            out.extend_from_slice(&h.frag_index.to_le_bytes());
            out.extend_from_slice(&h.frag_count.to_le_bytes());
            out.push(h.fec_block);
            out.push(h.fec_parity);
            out.extend_from_slice(&h.frame_len.to_le_bytes());
            out.extend_from_slice(&h.send_us.to_le_bytes());
            out.extend_from_slice(payload);
        }
        Inner::Ping { id, send_us } => {
            out.push(PacketType::Ping as u8);
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&send_us.to_le_bytes());
        }
        Inner::Pong { id, echo_us } => {
            out.push(PacketType::Pong as u8);
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&echo_us.to_le_bytes());
        }
        Inner::Ack(a) => {
            out.push(PacketType::Ack as u8);
            out.push(a.display);
            out.push(a.has_settled as u8);
            out.extend_from_slice(&a.settled.to_le_bytes());
            out.extend_from_slice(&a.echo_send_us.to_le_bytes());
            out.extend_from_slice(&a.hold_us.to_le_bytes());
            out.extend_from_slice(&a.lost_total.to_le_bytes());
        }
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], PacketError> {
        if self.0.len() < n {
            return Err(PacketError::TooShort);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, PacketError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, PacketError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, PacketError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, PacketError> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(a))
    }
    fn end(&self) -> Result<(), PacketError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(PacketError::BadHeader)
        }
    }
}

/// Parses a decrypted inner part.
pub fn decode_inner(bytes: &[u8]) -> Result<Inner<'_>, PacketError> {
    let mut r = Reader(bytes);
    let t = r.u8()?;
    let t = PacketType::from_u8(t).ok_or(PacketError::UnknownType(t))?;
    let inner = match t {
        PacketType::Hello | PacketType::HelloAck => {
            let lane_id = r.u64()?;
            r.end()?;
            if t == PacketType::Hello {
                Inner::Hello { lane_id }
            } else {
                Inner::HelloAck { lane_id }
            }
        }
        PacketType::Video | PacketType::Fec => {
            let h = VideoHeader {
                frame_id: r.u32()?,
                display: r.u8()?,
                epoch: r.u8()?,
                flags: r.u8()?,
                frag_index: r.u16()?,
                frag_count: r.u16()?,
                fec_block: r.u8()?,
                fec_parity: r.u8()?,
                frame_len: r.u32()?,
                send_us: r.u32()?,
            };
            if t == PacketType::Fec {
                if h.frag_count == 0 || h.fec_block == 0 || h.frag_index >= h.fec_parity as u16 {
                    return Err(PacketError::BadHeader);
                }
                Inner::Fec(h, r.0)
            } else {
                if h.frag_count == 0 || h.frag_index >= h.frag_count {
                    return Err(PacketError::BadHeader);
                }
                Inner::Video(h, r.0)
            }
        }
        PacketType::Ping | PacketType::Pong => {
            let id = r.u32()?;
            let us = r.u32()?;
            r.end()?;
            if t == PacketType::Ping {
                Inner::Ping { id, send_us: us }
            } else {
                Inner::Pong { id, echo_us: us }
            }
        }
        PacketType::Ack => {
            let display = r.u8()?;
            let has_settled = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(PacketError::BadHeader),
            };
            let a = Ack {
                display,
                has_settled,
                settled: r.u32()?,
                echo_send_us: r.u32()?,
                hold_us: r.u32()?,
                lost_total: r.u32()?,
            };
            r.end()?;
            Inner::Ack(a)
        }
    };
    Ok(inner)
}

#[cfg(test)]
mod latency_tests {
    use super::*;

    #[test]
    fn video_header_round_trip_and_size() {
        let h = VideoHeader {
            frame_id: 0xDEAD_BEEF,
            display: 2,
            epoch: 7,
            flags: FLAG_KEY | FLAG_SHADOW,
            frag_index: 3,
            frag_count: 9,
            fec_block: 0x21,
            fec_parity: 4,
            frame_len: 123_456,
            send_us: 42,
        };
        let payload = [1u8, 2, 3, 4, 5];
        let mut out = Vec::new();
        encode_inner(&Inner::Video(h, &payload), &mut out);
        assert_eq!(out.len(), VIDEO_HEADER_LEN + payload.len());
        assert_eq!(decode_inner(&out).unwrap(), Inner::Video(h, &payload[..]));
        // The same header as a parity shard (index 3 of 4 in block 1 of 2).
        let mut out = Vec::new();
        encode_inner(&Inner::Fec(h, &payload), &mut out);
        assert_eq!(out[0], PacketType::Fec as u8);
        assert_eq!(decode_inner(&out).unwrap(), Inner::Fec(h, &payload[..]));
    }

    #[test]
    fn control_round_trip() {
        for inner in [
            Inner::Hello { lane_id: 77 },
            Inner::HelloAck { lane_id: u64::MAX },
            Inner::Ping { id: 1, send_us: 2 },
            Inner::Pong { id: 3, echo_us: 4 },
            Inner::Ack(Ack {
                display: 1,
                settled: 0xFFFF_FFFE,
                has_settled: true,
                echo_send_us: 5,
                hold_us: ACK_NO_FRAME,
                lost_total: 6,
            }),
        ] {
            let mut out = Vec::new();
            encode_inner(&inner, &mut out);
            assert_eq!(decode_inner(&out).unwrap(), inner);
        }
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(decode_inner(&[]), Err(PacketError::TooShort));
        assert_eq!(decode_inner(&[99]), Err(PacketError::UnknownType(99)));
        assert_eq!(
            decode_inner(&[PacketType::Fec as u8]),
            Err(PacketError::TooShort)
        );
        // Parity without FEC fields, or with an index beyond the block's parity count.
        for (fec_block, fec_parity, frag_index) in [(0u8, 2u8, 0u16), (0x10, 2, 2), (0x10, 0, 0)] {
            let h = VideoHeader {
                frag_count: 3,
                fec_block,
                fec_parity,
                frag_index,
                ..Default::default()
            };
            let mut out = Vec::new();
            encode_inner(&Inner::Fec(h, &[1, 2]), &mut out);
            assert_eq!(decode_inner(&out), Err(PacketError::BadHeader));
        }
        // Trailing byte after a Hello.
        let mut out = Vec::new();
        encode_inner(&Inner::Hello { lane_id: 1 }, &mut out);
        out.push(0);
        assert_eq!(decode_inner(&out), Err(PacketError::BadHeader));
        // frag_index == frag_count.
        let h = VideoHeader {
            frag_index: 2,
            frag_count: 2,
            ..Default::default()
        };
        let mut out = Vec::new();
        encode_inner(&Inner::Video(h, &[]), &mut out);
        assert_eq!(decode_inner(&out), Err(PacketError::BadHeader));
        // Truncated video header.
        let h = VideoHeader {
            frag_count: 1,
            ..Default::default()
        };
        let mut out = Vec::new();
        encode_inner(&Inner::Video(h, &[]), &mut out);
        assert_eq!(
            decode_inner(&out[..VIDEO_HEADER_LEN - 1]),
            Err(PacketError::TooShort)
        );
    }

    #[test]
    fn outer_header() {
        let mut out = Vec::new();
        write_outer(0x0102_0304_0506_0708, &mut out);
        assert_eq!(out.len(), OUTER_LEN);
        out.extend_from_slice(&[0u8; TAG_LEN + 1]);
        let (seq, rest) = read_outer(&out).unwrap();
        assert_eq!(seq, 0x0102_0304_0506_0708);
        assert_eq!(rest.len(), TAG_LEN + 1);
        out[0] = 0;
        assert_eq!(read_outer(&out), Err(PacketError::BadMagic));
        out[0] = MAGIC;
        out[1] = 9;
        assert_eq!(read_outer(&out), Err(PacketError::BadVersion(9)));
        assert_eq!(read_outer(&out[..OUTER_LEN]), Err(PacketError::TooShort));
    }

    #[test]
    fn budgets_fit_the_mtu() {
        assert_eq!(PathClass::Lan.max_fragment_payload(), 1400 - 48);
        for p in [PathClass::Lan, PathClass::WanV4, PathClass::WanV6] {
            assert_eq!(p.max_fragment_payload() + VIDEO_OVERHEAD, p.max_datagram());
        }
        // IPv6 + UDP headers on a 1500 B Ethernet MTU.
        assert!(PathClass::Lan.max_datagram() + 48 <= 1500);
    }
}
