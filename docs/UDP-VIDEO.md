# UDP video lane

Status: **M19 first half, staged code.** `src/udp_video/` exists and is unit-tested, but no
session uses it: there is no negotiation, no socket, and no hook in `server/connection.rs` or
`client/io_loop.rs`. Video goes over the session stream (TCP, KCP, WebRTC or relay) exactly as
before. Roadmap: `PLAN-full.hu.md` M19 (shadow test), M20 (live on the LAN), M21 (FEC), M24
(internet).

## What exists

| File | Contents |
|---|---|
| `src/udp_video/packet.rs` | Wire format, version 1, and the per-path datagram budgets |
| `src/udp_video/crypto.rs` | Key derivation, datagram sealing/opening, 1024-seq replay window |
| `src/udp_video/fragment.rs` | Fragmentation, reassembly with timeouts, loss gate, loss statistics |
| `src/udp_video/pacing.rs` | Token-bucket pacer, target rate, `wait_until` |
| `src/udp_video/lane.rs` | `LaneSender` / `LaneReceiver`: the two ends without sockets |

Tests: `cargo test --lib latency` runs them (every test module is named `latency_tests`).
They cover round trips on all three path classes, loss, reorder, duplicates, replay (inside
and beyond the window), tampering, wrong lane/direction, the post-loss gate, wrapping frame
ids, two displays, and pacing.

### Wire format

```
outer (clear, 10 B)   magic 0xD7 | version 1 | seq u64 LE
tag (16 B)            Poly1305 tag of the secretbox
inner (encrypted)     type u8 | fields (little-endian)

VIDEO inner (22 B + payload)
  type=3 | frame_id u32 | display u8 | epoch u8 | flags u8 | frag_index u16 | frag_count u16
  | fec_group u8 | fec_parity u8 | frame_len u32 | send_us u32 | payload
HELLO / HELLO_ACK     type=1/2 | lane_id u64
PING / PONG           type=5/6 | id u32 | send_us/echo_us u32
FEC                   type=4, reserved for M21 (rejected today)
```

- `flags`: `KEY` (IDR), `RECOVERY_POINT` (RFI recovery frame, M22), `SHADOW` (copy of a TCP
  frame, measure only).
- `epoch`: the host bumps it on SwitchDisplay or a video-service restart, so the viewer can
  hold frames of a display change it has not read on TCP yet (the review's fix, used in M20).
- `fec_group` / `fec_parity`: always 0 now. With FEC the last data shard of a block must be
  padded to the shard length; today the last fragment is simply shorter.
- The payload of a frame is the serialized `Message{VideoFrame}`, unchanged, so the viewer's
  existing frame handling and `net+server` measurement keep working on reassembled frames.

Overhead per video datagram: 48 B. Budgets are whole UDP payloads:

| Path | Datagram | Fragment payload | Why |
|---|---|---|---|
| LAN | 1400 | 1352 | 1400 + 8 + 40 (IPv6) = 1448 <= 1500 |
| Internet, IPv4 | 1024 | 976 | Moonlight's value (tunnels, PPPoE, mobile) |
| Internet, IPv6 | 1184 | 1136 | Moonlight's value |

### Encryption

- Cipher: XSalsa20-Poly1305 (`secretbox`), the primitive the TCP stream uses, in detached mode
  (sealed in place).
- Keys: one per direction, derived like the TCP stream's split keys
  (`Encrypt::derive_subkey` in hbb_common): keyed BLAKE2b-256 with the **lane secret** as key,
  over `"rd-udp-video-v1" || direction || lane_id`.
- The lane secret is 32 random bytes the host sends in the lane offer **over the encrypted
  session stream**, so its secrecy rests on the session's key exchange. Deriving directly from
  the TCP session key would need a change in the hbb_common submodule: `Encrypt` keeps its
  receive key private and each side can read only its own send key. If that accessor is added
  later, only the input of `derive_key` changes; the domain label already keeps lane keys apart
  from every TCP key.
- Nonce: `seq u64 LE || direction u8 || 15 zero bytes`. The seq is per direction, starts at 1,
  strictly increases and never wraps (the sender stops at 2^64-2), so no (key, nonce) pair
  repeats. Keys already differ per direction; the direction byte in the nonce is a second
  guard. A new lane (new offer) has a new secret and a new lane id, so seqs may restart.
- The seq is in the clear and authenticated by being the nonce; magic and version are only
  selectors, and the version is bound into the key derivation.
- Replay: 1024-seq sliding window (WireGuard/DTLS style). The window moves only after the tag
  verified, so forged datagrams cannot push it.
- The roadmap line "a new key per frame" is met by a fresh key per lane plus a unique seq per
  datagram; a per-frame rekey would add cost without adding safety.

### Reassembly and loss

- Frames leave in `frame_id` order as soon as complete (no jitter buffer).
- The oldest missing frame is declared lost when a later frame completed `gap_grace` (5 ms)
  ago, or its first fragment (or the oldest later one, if none of it arrived) is older than
  `frame_timeout` (50 ms). A fragment more than `max_ahead` (64) frames ahead declares the
  backlog lost at once. Consecutive losses merge into one `Lost{from, to}` event.
- After a loss, complete frames are dropped (`frames_gated`) until a `KEY` or
  `RECOVERY_POINT` frame, so the decoder never gets a frame whose reference is missing. The
  `Lost` event is the caller's cue to request a keyframe.
- Inconsistent fragments (frame length, count, flags or epoch differing within a frame, or
  bytes not adding up) lose the frame.
- `LossStats` per window: fragments received / duplicate / late / invalid, frames completed /
  lost / gated, fragments missing in lost frames, reorder events and max depth (in seqs),
  datagrams expected (seq span) and received. `DropStats`: malformed, replayed,
  unauthenticated, bad inner.

### Pacing

Token bucket: up to `burst` bytes at once (8 LAN datagrams to start), the rest at
`max(1.5 x bitrate, frame_bytes / (0.5 x frame interval))`, so a frame, an IDR included, is out
within half a frame interval. `wait_until` sleeps and spins the last millisecond; the live
sender should use a high-resolution waitable timer on Windows.

## What the next step wires up (M19 second half: shadow)

1. **Options.** Host `udp-video` (Config: empty = off, `shadow`; later `on`), viewer
   `enable-udp-video` (LocalConfig, `N` = never advertise). Not added yet: nothing reads them.
2. **Negotiation** (fork-only tags, ignored by stock peers): `LoginRequest.udp_video_version =
   1001` from the viewer; Misc `UdpVideoOffer` (1002) from the host with lane id, lane secret,
   host IP and port, max datagram, mode; Misc `UdpVideoControl` (1003) READY / ACTIVE
   {first_frame_id per display} / STOP / REQUEST_STOP. Offer only when the explicit
   `TransportInfo` is direct TCP (`Intranet` or `TcpPunch` on the LAN), the stream
   `is_secured`, and the connection is remote control.
3. **Addresses.** Normalize with `try_into_v4`, skip link-local IPv6 in phase 1, refuse an
   unspecified address; the viewer binds to its TCP local address, not the wildcard.
4. **Sockets and threads.** `UdpVideoHost` / `UdpVideoClient` owning the socket and threads
   and cleaning up in `Drop`. Viewer sends HELLO every 20 ms up to 1 s; host answers HELLO_ACK
   and `connect()`s to the viewer. On Windows `SIO_UDP_CONNRESET` off. Host receive path: a
   receive thread for HELLO and PONG.
5. **Shadow sender.** Every Nth frame is copied to the UDP sender before the TCP send starts,
   flagged `SHADOW`.
6. **Measurement without bias.** The viewer's receive thread stamps `udp_ahead_net`; the
   complete event then goes through the same tokio mpsc and `select!` branch the live path will
   use, which stamps `udp_ahead_app`. Both are compared with the TCP copy by `pts`; shadow
   frames are never decoded. New fields go to docs/LATENCY-METRICS.md with that step.

M20 then adds the live switch (ACTIVE marker, epoch hold, keyframe on loss through
`request_keyframe`, a 2-3 frame send window acknowledged per frame or every 50 ms, a UDP delay
signal for `video_qos`, the watchdog and the fallback to TCP).
