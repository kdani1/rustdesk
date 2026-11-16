//! UDP video lane. M19 first half: the parts that need no live session; M19 second half: the
//! shadow probe (negotiation over the session stream, sockets and threads on both ends, the
//! host's copy of every Nth frame, the viewer's pairing with the TCP copy). M20: live mode,
//! video frames over UDP with paced bursts, a send window, key frames on loss and a fallback
//! to TCP video in the same session. M21: Reed-Solomon FEC per frame. See docs/UDP-VIDEO.md.
//!
//! - `packet`: wire format (frame id, fragment index/count, FEC block fields and parity
//!   datagrams, epoch, flags).
//! - `crypto`: per-direction keys derived from the lane secret, seq nonces, replay window.
//! - `fragment`: fragmentation to MTU-safe sizes and reassembly with timeouts, a loss gate and
//!   loss statistics; parity shards rebuild lost fragments at once (M21).
//! - `fec`: Reed-Solomon erasure code over GF(256) (in-tree, SIMD on x86-64) and the frame's
//!   block layout (M21).
//! - `pacing`: token-bucket pacing, the target rate and a high-resolution waiter (live mode).
//! - `lane`: sender and receiver ends without sockets.
//! - `net`: LAN address gate, socket setup, lane clock.
//! - `host`: offer gate, `UdpVideoHost` (socket, receive and sender threads), `VideoTap`.
//! - `viewer`: offer check, `UdpVideoClient` (socket, receive thread), `ViewerLane`.
//! - `shadow`: UDP/TCP pairing and window statistics.
//!
//! Platform-neutral code; only socket options are Windows-specific. Hosts offer and viewers
//! advertise only on Windows, so other platforms compile it and never use it.

// Parts of these are staged for M21-M24 (WAN path classes, FEC fields, accessors); the shadow
// probe and the live lane use the rest.
#[allow(dead_code)]
pub mod crypto;
pub mod fec;
#[allow(dead_code)]
pub mod fragment;
pub mod host;
#[allow(dead_code)]
pub mod lane;
pub mod net;
#[allow(dead_code)]
pub mod pacing;
#[allow(dead_code)]
pub mod packet;
pub mod shadow;
pub mod viewer;
