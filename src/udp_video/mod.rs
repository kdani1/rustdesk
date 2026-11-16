//! UDP video lane, M19 first half: the parts that need no live session. Nothing calls this yet;
//! the offer/HELLO negotiation, the sender and receive threads and the hooks into
//! `server/connection.rs` and `client/io_loop.rs` come with the M19 second half (shadow mode)
//! and M20. See docs/UDP-VIDEO.md.
//!
//! - `packet`: wire format (frame id, fragment index/count, FEC placeholders, epoch, flags).
//! - `crypto`: per-direction keys derived from the lane secret, seq nonces, replay window.
//! - `fragment`: fragmentation to MTU-safe sizes and reassembly with timeouts, a loss gate and
//!   loss statistics.
//! - `pacing`: token-bucket pacing and the target rate.
//! - `lane`: sender and receiver ends without sockets.
//!
//! Platform-neutral on purpose: other platforms compile it and never use it.
#![allow(dead_code)] // Staged: wired in the M19 second half.

pub mod crypto;
pub mod fragment;
pub mod lane;
pub mod pacing;
pub mod packet;
