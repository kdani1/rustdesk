# UDP video lane

Status: **M19 done: shadow probe.** A fork viewer and a fork host on the same LAN negotiate a
UDP lane after login, and the host sends a copy of every Nth video frame over it. The viewer
reassembles the copies, measures loss, reordering and how much earlier UDP had each frame than
TCP, logs it, and drops them: **the picture still comes from the session stream (TCP) only.**
Off by default on the host. Roadmap: `PLAN-full.hu.md` M19 (shadow test), M20 (live on the
LAN), M21 (FEC), M24 (internet).

## What exists

| File | Contents |
|---|---|
| `src/udp_video/packet.rs` | Wire format, version 1, and the per-path datagram budgets |
| `src/udp_video/crypto.rs` | Key derivation, datagram sealing/opening, 1024-seq replay window |
| `src/udp_video/fragment.rs` | Fragmentation, reassembly with timeouts, loss gate, loss statistics |
| `src/udp_video/pacing.rs` | Token-bucket pacer, target rate, `wait_until` |
| `src/udp_video/lane.rs` | `LaneSender` / `LaneReceiver`: the two ends without sockets |
| `src/udp_video/net.rs` | LAN address gate, socket setup (`SIO_UDP_CONNRESET` off, buffers), lane clock |
| `src/udp_video/host.rs` | Offer gate, `UdpVideoHost` (socket, receive and sender threads), `ShadowTap` |
| `src/udp_video/viewer.rs` | Offer check, `UdpVideoClient` (socket, receive thread), `ViewerLane` (io_loop state) |
| `src/udp_video/shadow.rs` | Pairing of UDP and TCP copies by pts, window statistics |

Hooks: `libs/base/protos/message.proto` (negotiation), `src/client.rs` `create_login_msg`
(advertise), `src/client/io_loop.rs` (offer, control, the lane's `select!` branch, the TCP
copy's timestamp), `src/server/connection.rs` (offer gate, offer, control, handshake timeout,
the tap before both TCP video sends), `src/session_io.rs` (`WriterHooks::video_dequeued` gets
the shared message, so the split writer's hook can hand it to the tap).

Tests: `cargo test --lib latency` runs them (every test module is named `latency_tests`).
They cover round trips on all three path classes, loss, reorder, duplicates, replay (inside
and beyond the window), tampering, wrong lane/direction, the post-loss gate, wrapping frame
ids, two displays, pacing, the LAN address gate, the offer gate and offer check, the tap's
every-Nth choice and queue drops, the UDP/TCP pairing, and a loopback handshake with shadow
frames end to end (HELLO, HELLO_ACK, READY, frames byte-identical).

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
- `epoch`: the host bumps it on SwitchDisplay, so the viewer can hold frames of a display
  change it has not read on TCP yet (the review's fix, used in M20). The shadow tap bumps it on
  every SwitchDisplay it sees; a video-service restart without one does not bump it yet (M20).
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

## Shadow probe (M19 second half)

### Options

| Key | Side | Default | Meaning |
|---|---|---|---|
| `enable-udp-video` | host, server option (Config) | off | `Y` offers a lane (shadow mode) to a viewer that advertised one. Read at each login. |
| `enable-udp-video` | viewer, local option (LocalConfig) | on | `N` stops advertising and refuses offers: the next connection is plain TCP, without touching the host. |
| `udp-video-shadow-every` | host, server option | `2` | One frame in N per display is copied to UDP (1..=60). Read at each offer. |

`rustdesk.exe --option enable-udp-video Y` on the host (elevated, installed) turns it on, `N`
off; it applies from the next login. With the option off the host costs one relaxed atomic
load per video item and the viewer one `Option` check per frame.

### Negotiation (fork-only tags)

Tag plan of the fork: Misc 1001 `pointer_captured`; the lane uses LoginRequest 1001 and Misc
1002-1004 (1004 is reserved for the per-frame feedback of M20); the next free Misc tag is 1005.

1. **Viewer -> host:** `LoginRequest.udp_video_version` (1001), set to 1 by a Windows viewer
   for remote control unless `enable-udp-video` is `N`. Stock hosts ignore the field.
2. **Host gate**, checked once a remote-control session is authorized (right after the split,
   when split session I/O is on). A viewer that sent 0 gets nothing and no log line; one that
   advertised gets either an offer or one `not offered` line with the reason:
   - Windows host, `enable-udp-video` = `Y`, the viewer does not ack frames (web client);
   - transport from `create_tcp_connection`'s explicit `TransportInfo`: `Intranet`, `Direct` or
     `TcpPunch` only (never KCP, WebRTC or relay: the Stream variant is not trusted, a KCP
     session is a `Stream::Tcp` too);
   - the session stream is encrypted (`is_secured`): the lane secret travels in it;
   - the stream's local address and the peer address both pass `net::lane_ip`: normalized with
     `try_into_v4`, not unspecified, not loopback, not link-local IPv6 (its scope id would not
     travel), on the LAN (RFC 1918, link-local IPv4, ULA), same family.
3. **Host -> viewer:** Misc `UdpVideoOffer` (1002): version 1, mode `ModeShadow`, a random lane
   id, a fresh 32-byte lane secret (`secretbox::gen_key`), the host IP (4 or 16 bytes) and UDP
   port, `max_datagram` 1400, `shadow_every`. The host binds the lane socket to the stream's
   local IP (port 0) before it sends the offer.
4. **Viewer check:** version 1, mode shadow, a 32-byte secret, `max_datagram` within its
   2048-byte buffer, a host address that passes `lane_ip`, and a session that is itself direct
   TCP (`direct`, no KCP, no WebRTC, encrypted) with a local address that passes `lane_ip`. The
   viewer binds to that local address (never the wildcard) and `connect()`s to the host's
   address. A refusal goes back as `UdpVideoControl` (1003) `KindRequestStop` with the reason.
5. **Handshake on UDP:** the viewer sends HELLO every 20 ms for up to 1 s. Until the first
   HELLO that opens with the lane keys, the host's receive thread takes datagrams only from the
   session's peer IP; then it `connect()`s to that source address and answers every HELLO with
   HELLO_ACK. The viewer then sends `KindReady` over TCP and pings every 200 ms; the host starts
   copying frames on READY.
6. **Failures:** no HELLO_ACK at the viewer after 1 s: `KindRequestStop` "no HELLO_ACK", the
   lane closes on both ends. No READY at the host 3 s after the offer: `KindStop` to the viewer;
   the log says whether any HELLO came in (none: inbound UDP looks blocked, see Firewall).
   Either way the session goes on over TCP exactly as before. A `KindStop`, the end of the
   io_loop round, or the end of the host connection closes the lane.

### Firewall

The installer (`src/platform/windows.rs`: `netsh advfirewall firewall add rule name="<app>
Service" dir=in action=allow program="<exe>"`) and the MSI (`CustomActions.cpp`
`AddFirewallRuleCmdline`) add program rules with no protocol, port or profile filter, so
inbound UDP to the installed exe on its ephemeral lane port is allowed. The viewer needs no
rule: it sends first, so the stateful firewall lets the host's replies in. What can still block
the lane: a third-party firewall, a portable or not installed host exe (no program rule; the
offer line logs `installed=`), or a network that drops UDP. The host then logs the handshake
timeout as `no HELLO from the viewer in 3 s: inbound UDP to this host looks blocked (...)`; the
viewer logs `handshake failed ... (UDP from this viewer to the host, or back, is blocked)`.

### Host: the shadow copy

- `ShadowTap::offer(&Arc<Message>)` runs in front of both TCP video sends: the connection
  loop's `rx_video` branch (whole stream) and the split writer's `video_dequeued` hook. The UDP
  copy therefore starts before the TCP write. Inactive: one relaxed atomic load.
- Active: per display, items 0, N, 2N, ... are copied (an `Arc` clone into a bounded queue of
  4 with `try_send`; full: `qdrop`, the video path never blocks). A SwitchDisplay bumps the
  epoch byte the copies carry.
- The sender thread serializes the `Message{VideoFrame}` once, gives it the next lane frame id
  of its display (consecutive over what it actually sends, so a `qdrop` is not counted as loss
  by the viewer), sets `KEY` (any key frame in it) and `SHADOW`, stamps `send_us` on the lane
  clock right before the first datagram, and seals and sends the fragments back to back.
  **No pacing** in shadow mode (see "Left out").
- The receive thread answers PING with PONG through the same `LaneSender` (a mutex: the seq
  must stay unique per direction). Both threads seal and `send` one datagram per lock hold, so
  the wire order is the seq order: a PONG sent mid-frame goes between two fragments and never
  overtakes fragments sealed before it (otherwise the viewer would count reorder, and past the
  1024-seq replay window drop the frame's tail as replayed).

### Viewer: measurement without bias

- The receive thread (ABOVE_NORMAL with `enable-viewer-priority-boost`) reassembles with
  `gate_after_loss` off (shadow frames are never decoded, so every complete one counts) and a
  500 ms frame timeout instead of 50 ms (unpaced shadow bursts share the link with the TCP
  copy; a slow but complete frame is paired, not lost), stamps each complete frame, parses it
  for display and pts, and sends it through a tokio mpsc to a `select!` branch of the io_loop:
  the path M20 will deliver live frames on.
- That branch stamps the time at its entry. The TCP copy is stamped when `peer.next()` returns
  it, before `handle_msg_from_peer` parses it, so neither stamp includes a parse and the
  io_loop does no full-frame parse of UDP copies. Copies pair by (display, pts), whichever
  comes first. `ahead_net` = t(TCP) - t(complete on the thread): the upper
  bound, it credits UDP with the io_loop's own scheduling delay. `ahead_app` = t(TCP) - t(UDP
  branch): both copies pay the same loop delay (the review's fix).
- UDP copies are dropped after pairing; nothing reaches the decoder, the video queue or the
  latency stats.

The log lines are in docs/LATENCY-METRICS.md, "UDP video lane (shadow probe)".

### What the shadow numbers are worth

- The copy adds the whole (N=1) or half (N=2) video bitrate to the link, and on Wi-Fi to Wi-Fi
  every frame crosses the AP twice. Loss and delay with the copy on are pessimistic for both
  copies; state N with every result.
- Bursts are unpaced, so loss on big frames (IDR) is higher than a paced live lane would see.
- `ahead_*` compares a UDP copy with a TCP stream that carries the full video; with UDP live,
  TCP would be nearly idle.
- Gate for M20 (roadmap): `ahead_app` p95/max about 0 with loss about 0 means M20-M22 can wait;
  `ahead_app` spikes together with TCP retransmissions (`LATENCY tcp`, pktmon) mean go on.

### Left out of M19, on purpose

- **Pacing** (`pacing.rs` is ready): without a high-resolution waitable timer a spin-paced
  sender costs about half a core at 60 fps; it comes with the live lane together with DSCP /
  qWAVE on the UDP socket (M20, design step 4).
- **ACTIVE, epoch hold, keyframe on loss, send window, liveness from PING on the host, a UDP
  delay signal for `video_qos`, viewer watchdog, fallback and retry** (M20).
- **Per-frame feedback** `UdpVideoFeedback` (Misc 1004, M20/M23), FEC (M21), RFI (M22), WAN
  (M24).
- **Multiple viewers**: each connection has its own lane and tap; nothing is shared.
- The lane secret is fresh per lane, not derived from the session key (that needs a change in
  hbb_common, see Encryption); its secrecy rests on the session's key exchange either way.

## M20 next

The live switch: ACTIVE marker, epoch hold, keyframe on loss through `request_keyframe` (M13),
a 2-3 frame send window acknowledged per frame or every 50 ms, a UDP delay signal for
`video_qos`, pacing, the viewer watchdog and the fallback to TCP.
