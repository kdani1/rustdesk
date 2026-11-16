//! UDP video lane. M19 first half: the parts that need no live session; M19 second half: the
//! shadow probe (negotiation over the session stream, sockets and threads on both ends, the
//! host's copy of every Nth frame, the viewer's pairing with the TCP copy). Video is still shown
//! from TCP only. M20 adds the live switch. See docs/UDP-VIDEO.md.
//!
//! - `packet`: wire format (frame id, fragment index/count, FEC placeholders, epoch, flags).
//! - `crypto`: per-direction keys derived from the lane secret, seq nonces, replay window.
//! - `fragment`: fragmentation to MTU-safe sizes and reassembly with timeouts, a loss gate and
//!   loss statistics.
//! - `pacing`: token-bucket pacing and the target rate (not used by shadow mode).
//! - `lane`: sender and receiver ends without sockets.
//! - `net`: LAN address gate, socket setup, lane clock.
//! - `host`: offer gate, `UdpVideoHost` (socket, receive and sender threads), `ShadowTap`.
//! - `viewer`: offer check, `UdpVideoClient` (socket, receive thread), `ViewerLane`.
//! - `shadow`: UDP/TCP pairing and window statistics.
//!
//! Platform-neutral code; only socket options are Windows-specific. Hosts offer and viewers
//! advertise only on Windows, so other platforms compile it and never use it.

// Parts of these are staged for M20-M24 (WAN path classes, pacing, accessors for the live
// lane); the shadow probe uses the rest.
#[allow(dead_code)]
pub mod crypto;
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
