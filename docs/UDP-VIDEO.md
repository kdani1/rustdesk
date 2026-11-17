# UDP video lane

Status: **M22 done: live mode with FEC and RFI.** A fork viewer and a fork host on the same LAN negotiate a
UDP lane after login. In **shadow mode** (M19, host option `enable-udp-video=shadow`) the host
sends a copy of every Nth video frame over it and the viewer only measures it. In **live mode**
(M20, `enable-udp-video=live`) the video frames go over UDP only and the viewer decodes them;
control, input, audio, cursor and clipboard stay on the session stream (TCP), and any trouble
puts the video back on TCP in the same session. Since M21 every frame carries Reed-Solomon
parity (20% by default, `udp-video-fec-percent`) when the viewer speaks lane v4, so scattered
losses are repaired on arrival instead of costing a key frame. Since M23 a fork viewer can send
per-frame feedback (arrival, decode and present times, UDP losses) over the session stream on any
transport; the host measures the one-way queueing delay from it and, when told to, uses it for
QoS, an adaptive FEC level and the lane's fallback (see "Per-frame feedback (M23)"). Since M22
a lost frame FEC could not save can be repaired by reference frame invalidation on NVENC (HEVC)
instead of a key frame (`enable-nvenc-rfi`, see "RFI (M22)"). Since M24 the same lane also runs
over the internet when the session is on a direct path (TCP punch, direct IP, KCP): its own UDP
socket on each end, public ports learned from the rendezvous server and exchanged over the
session stream, a punch from both ends, WAN datagram sizes and no DSCP (`enable-udp-video-wan`,
see "Internet lane (M24)"). Off by default on the host.
Roadmap: `PLAN-full.hu.md` M19 (shadow test), M20 (live on the LAN), M21 (FEC), M22 (RFI), M23
(feedback), M24 (internet).

## What exists

| File | Contents |
|---|---|
| `src/udp_video/packet.rs` | Wire format, version 1, and the per-path datagram budgets |
| `src/udp_video/crypto.rs` | Key derivation, datagram sealing/opening, 1024-seq replay window |
| `src/udp_video/fragment.rs` | Fragmentation, reassembly with timeouts, loss gate, loss statistics; parity shards rebuild lost fragments (M21) |
| `src/udp_video/fec.rs` | Reed-Solomon erasure code over GF(256) (in-tree, AVX2/SSSE3 on x86-64), the frame's FEC block layout, `protect` (M21) |
| `src/udp_video/pacing.rs` | Token-bucket pacer, target rate, `wait_until`, `Waiter` (high-resolution waitable timer) |
| `src/udp_video/lane.rs` | `LaneSender` / `LaneReceiver`: the two ends without sockets |
| `src/udp_video/net.rs` | LAN and internet address gates, socket setup (`SIO_UDP_CONNRESET` off, buffers), the rendezvous NAT query (M24), lane clock |
| `src/udp_video/host.rs` | Offer gate, `UdpVideoHost` (socket, receive and sender threads, send window, QoS signal), `VideoTap` (routing, ACTIVE/STOP markers) |
| `src/udp_video/viewer.rs` | Offer check, `UdpVideoClient` (socket, receive thread, ACKs), `ViewerLane` (io_loop state: hold, epochs, key frame requests, watchdog) |
| `src/udp_video/shadow.rs` | Pairing of UDP and TCP copies by pts, window statistics |
| `src/udp_video/rfi.rs` | RFI requests (connection -> capture loop) and recovery frames (capture loop -> taps) per display (M22) |
| `src/video_feedback/viewer.rs` | Per-frame feedback, viewer: `Collector` (batches), `DoneLog` (the video thread's hook), `UdpCounts` (M23) |
| `src/video_feedback/host.rs` | Per-frame feedback, host: `SendLog`, `Estimator` (q_owd, GCC trendline, jitter, loss), `FeedbackInput` / `FeedbackSink` (the seam to the fast ABR), `LegacyQos`, `FecAdapt`, `ConnFeedback` (M23) |

Hooks: `libs/base/protos/message.proto` (negotiation), `src/client.rs` `create_login_msg`
(advertise), `src/client/io_loop.rs` (offer, control, the lane's `select!` branch, the TCP
copy's timestamp; live: `handle_video_frame` shared by TCP and UDP frames, `feed_udp_video`,
SwitchDisplay epochs, the watchdog tick), `src/server/connection.rs` (offer gate, offer,
control, feedback, handshake and liveness checks, fallback and retry, the tap before both TCP
video sends, the QoS delay sample), `src/session_io.rs` (`WriterHooks::video_dequeued` gets the
shared message and returns a `Route`: markers to write first, and whether the frame itself
goes to TCP), `src/server/latency_stats.rs` (`qos_sig`), `src/platform/windows_qos.rs`
(`mark_udp`).

Tests: `cargo test --lib latency` runs them (every test module is named `latency_tests`).
They cover round trips on all three path classes, loss, reorder, duplicates, replay (inside
and beyond the window), tampering, wrong lane/direction, the post-loss gate, wrapping frame
ids, two displays, pacing, the LAN address gate, the offer gate and offer check, the tap's
every-Nth choice and queue drops, the UDP/TCP pairing, and a loopback handshake with shadow
frames end to end (HELLO, HELLO_ACK, READY, frames byte-identical). Live mode adds: the tap's
routing and markers (ACTIVE before the first UDP frame, SwitchDisplay epochs, the in-band STOP
with the last ids), queue drops reported fetched, the send window held and released by ACKs
and by its time limit, the QoS excess and baseline, paced bursts, the reassembler's start at
frame 0 and `settled`, the viewer's hold / epoch / gate / STOP decisions and watchdog, and a
loopback live lane end to end (frames in order with ids from 0, ACKs, every frame fetched,
the STOP marker after shutdown). FEC (M21) adds: GF(256) arithmetic, the SIMD and scalar
multiply paths agreeing for every coefficient, every erasure pattern of a small block, random
erasures of full 200+40 and 253+2 blocks, the block layout and parity counts for every frame
size up to the 4-block limit, parity headers, a frame rebuilt at once (short last fragment,
one-fragment frame, parity of a frame already delivered counted neither late nor duplicate), too
many losses counted unrecoverable, a two-block frame reordered, FEC off sending the plain
frame, and paced live sending with parity and three lost datagrams.

### Wire format

```
outer (clear, 10 B)   magic 0xD7 | version 1 | seq u64 LE
tag (16 B)            Poly1305 tag of the secretbox
inner (encrypted)     type u8 | fields (little-endian)

VIDEO inner (22 B + payload)
  type=3 | frame_id u32 | display u8 | epoch u8 | flags u8 | frag_index u16 | frag_count u16
  | fec_block u8 | fec_parity u8 | frame_len u32 | send_us u32 | payload
FEC inner (22 B + parity shard, M21)
  type=4 | the VIDEO fields, with frag_index = parity index in its block (< fec_parity) and
  frag_count = the frame's data fragments
HELLO / HELLO_ACK     type=1/2 | lane_id u64
PING / PONG           type=5/6 | id u32 | send_us/echo_us u32
ACK (viewer -> host)  type=7 | display u8 | has_settled u8 | settled u32 | echo_send_us u32
                      | hold_us u32 | lost_total u32
```

ACK (live mode, M20): `settled` is the last frame id of the display the reassembler released
(delivered, dropped behind a loss, or declared lost); `echo_send_us` the `send_us` of the last
complete frame and `hold_us` the viewer time from that frame's first fragment to the ACK
(`u32::MAX`: none yet), so the host's round trip excludes the viewer's hold; `lost_total` the
display's lost frames so far (wrapping). Sent per display after every release and every 50 ms.
A shadow-mode (M19) receiver drops type 7 as an unknown type, and an M19 host never gets one.

- `flags`: `KEY` (IDR), `RECOVERY_POINT` (RFI recovery frame, M22), `SHADOW` (copy of a TCP
  frame, measure only).
- `epoch`: the tap bumps it on every SwitchDisplay that passes it, so the viewer can hold frames
  of a display change it has not read on TCP yet (the review's fix). Live: the ACTIVE marker
  carries the epoch at that point, and an active live tap writes `UdpVideoControl{KindEpoch,
  epoch}` right before each SwitchDisplay that passes it; the viewer takes the marker's epoch
  at the next SwitchDisplay it reads. Not every SwitchDisplay passes the tap: the connection
  sends one directly when it answers a display switch or a message query (Flutter multi-window),
  so counting all SwitchDisplays would leave the viewer an epoch ahead and every frame `stale`
  (the M20 review's finding). A video-service restart without a SwitchDisplay does not bump it
  and needs not: its first frame is a key frame.
- `fec_block` / `fec_parity` (M21): `(block count << 4) | block index` of the Reed-Solomon
  block the datagram belongs to and that block's parity shard count; both 0 for a frame without
  FEC (any frame to a viewer below lane v4, `udp-video-fec-percent=0`, or one too large). Data
  fragments keep their frame-wide `frag_index`; the last one stays short on the wire and counts
  as zero-padded in the code. See "FEC (M21)".
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
  `frame_timeout` (50 ms by default; live 80 ms, shadow 500 ms). A fragment more than
  `max_ahead` (64) frames ahead declares the
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
`max(1.5 x bitrate, frame_bytes / min(0.5 x frame interval, 25 ms))`, so a frame, an IDR
included, is out within half a frame interval and never takes longer than 25 ms (`MAX_SPREAD`;
at 10 fps or on an idle desktop the interval estimate nears 100 ms, and a 50 ms spread would
reach the live reassembly timeout). `wait_until` sleeps and spins the last millisecond; the
live sender uses a high-resolution waitable timer on Windows.

## Shadow probe (M19 second half)

### Options

| Key | Side | Default | Meaning |
|---|---|---|---|
| `enable-udp-video` | host, server option (Config) | off | `live`: live mode (M20) to a viewer of lane v3; `shadow` or `Y` (M19's value): shadow mode to any lane viewer. Anything else: no offer. Read at each login (and each retry). |
| `enable-udp-video` | viewer, local option (LocalConfig) | on | `N` stops advertising and refuses offers: the next connection is plain TCP, without touching the host. |
| `udp-video-shadow-every` | host, server option | `2` | Shadow: one frame in N per display is copied to UDP (1..=60). Read at each offer. |
| `udp-video-burst` | host, server option | `8` | Live: datagrams per paced burst (1..=64; `0` = unpaced, a frame at once). Read at each offer. |
| `udp-video-timeout-ms` | host, server option | `1000` | Live: no datagram from the other end this long ends the lane (200..=10000). Sent in the offer; the viewer uses the same value. Read at each offer. |
| `udp-video-fec-percent` | host, server option | `20` | FEC parity per block in percent of its data shards (0..=100; `0` = no parity; empty or invalid: 20), at least 2 per block. Live and shadow lanes, viewers of lane v4 only. Read at each offer (M21). |
| `enable-udp-video-wan` | host, server option | off | `Y`: also offer the lane (in the `enable-udp-video` mode) on a direct internet path to a viewer of lane v6 (M24). Read at each offer. |
| `enable-udp-video-wan` | viewer, local option | on | `N`: advertise lane v5 (no internet lanes) and refuse an internet offer; LAN lanes unaffected (M24). |

`rustdesk.exe --option enable-udp-video shadow` (or `Y`) on the host (elevated, installed)
turns the probe on, `live` live mode, `N` off; it applies from the next login. **Note:** `Y`
means shadow, as in M19 (the first M20 build took it as live; the review moved live to its own
value, so a host set up for the probe never goes live with a newer build). With the option off
the host costs one relaxed atomic load per video item and the viewer one `Option` check per
frame.

### Negotiation (fork-only tags)

Tag plan of the fork: Misc 1001 `pointer_captured`; the lane uses LoginRequest 1001 and Misc
1002 (`UdpVideoOffer`, field 10 `fec_percent` since M21), 1003 (`UdpVideoControl`) and 1004
(`UdpVideoFeedback`, M20); the per-frame feedback (M23) uses Features 1001 (`video_feedback`)
and Misc 1005 (`VideoFeedback`); the internet lane (M24) adds `UdpVideoOffer.wan` (11),
`UdpVideoControl.Kind.KindCandidate` (6) and `UdpVideoControl.port` (7). Next free: Misc 1006,
Features 1002, LoginRequest 1002, `UdpVideoOffer` 12, `UdpVideoControl` 8. The fast
ABR design's own plan (Features 1001 / Misc 1002 for the feedback) is superseded by this one:
Misc 1002 was taken by the lane.

1. **Viewer -> host:** `LoginRequest.udp_video_version` (1001), set by a Windows viewer for
   remote control unless `enable-udp-video` is `N`: 1 by an M19 viewer (shadow only), 2 by the
   first M20 build (live without EPOCH markers), 3 since the M20 review (live with them), 4
   since M21 (also takes FEC parity datagrams), 5 since M22 (resumes a broken display at a
   `RECOVERY_POINT` frame too), 6 since M24 (also takes internet lanes; only when the session
   is direct and the viewer knows the host's address off the LAN). Stock hosts ignore the field. A host with
   `live` offers live mode to v3 and later only; an older viewer gets a `not offered` line
   instead. Only a v4 viewer gets `fec_percent` > 0. Shadow offers have version 1, live offers version 2 (EPOCH
   markers); the viewer refuses a live offer of version 1 (the first M20 host). The datagram
   wire version stays 1.
2. **Host gate**, checked once a remote-control session is authorized (right after the split,
   when split session I/O is on). A viewer that sent 0 gets nothing and no log line; one that
   advertised gets either an offer or one `not offered` line with the reason:
   - Windows host, `enable-udp-video` = `live` or `shadow`/`Y`, the viewer does not ack frames
     (web client);
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

- `VideoTap::route(&Arc<Message>, queued)` runs in front of both TCP video sends: the connection
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

### Left out of M19, on purpose (most of it done in M20, see "Live mode")

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

## Live mode (M20)

### Switching to UDP: the ACTIVE marker

- The offer says `ModeLive` and `timeout_ms`; the handshake is the shadow one. On READY the
  host puts the tap in live mode. Nothing changes until the next video frame: the tap writes
  `UdpVideoControl{KindActive, epoch}` **to TCP in the order of the video stream** (whole
  stream: the connection loop's video branch; split: the writer task writes it before the item)
  and gives the frame to the lane with frame id 0. From then on every video frame goes to UDP
  only, ids from 0 per display; SwitchDisplay and everything else stays on TCP.
- The viewer holds UDP frames (up to 32) until it read ACTIVE. All TCP frames before ACTIVE
  were handled before it, so the first UDP frame, a P-frame referencing the last TCP frame,
  decodes into a valid chain: no key frame is needed for the switch (the review's hold fix).
  Frames of a newer epoch are held until the viewer read the SwitchDisplay that bumped it (the
  one right after a `KindEpoch` marker); frames of an older epoch are dropped (`stale`).
- UDP frames enter the same `handle_video_frame` as TCP frames: decoder queue or key frame
  path, `VideoCounters`, the M4 present pacer, M5 fps control (queue overflow refreshes) and
  the latency stats (`net+server` uses the frame's own timestamps, whichever way it came). The
  receive thread parses each frame, so the io_loop does no full-frame parse.

### Loss: key frames, never a broken reference

- The receive thread reassembles with the loss gate on and every display starting at frame 0:
  after a loss (`Lost{from,to}`: 80 ms after a frame's first fragment, above the pacer's 25 ms
  longest spread plus Wi-Fi jitter; 5 ms after a later frame completed; a frame id gap; a frame
  that does not parse) nothing of that display reaches the io_loop until a `KEY` (or, later,
  `RECOVERY_POINT`) frame. A loss followed in the same poll by a key frame of its display (the
  key frame overtook the gap) is reported repaired and asks nothing.
- The io_loop marks the display broken (also when the 32-frame hold buffer drops one of its
  frames), drops anything but a key frame for it, and sends `UdpVideoFeedback{request_keyframe,
  display, lost_from, lost_to}` over TCP at once. While the display waits, further losses ask
  nothing (the key frame asked for repairs them too); if no key frame came 250 ms after the
  last request (the key frame or the request was lost), the request is repeated, driven by a
  50 ms tick from the receive thread.
- **Tail loss**: a frame lost whole with no later frame of its display (one small update on an
  otherwise static screen) shows the viewer no gap. The host's sender thread watches each
  display's newest frame (sent, or dropped from the full queue): not settled by the ACKs 200 ms
  (plus the ACK round-trip baseline) after it went out, with nothing newer sent, it asks the
  capture loop for a key frame itself (`tail_kf`), once per newest frame id.
- The host answers through the refresh path of the display: with `enable-refresh-keyframe=Y`
  (M13) a forced key frame on the running encoder (merged with others, at most one per 200 ms,
  3 within 3 s escalate to a restart); otherwise a capture restart (`SWITCH`) for every viewer.
  The offer logs which. Live mode is meant to run with M13 on.
- The host counts the requests and its tail loss key frames: 4 within 5 s end the lane
  (sustained loss).

### Back-pressure: the send window

UDP takes away the TCP write that used to hold the capture loop back. The connection therefore
does not report a live frame as fetched when it takes it off the video channel; the lane does,
once the frame is sent and at most 3 frames of the display are unsettled by the viewer's ACKs
(the roadmap's 2-3 unacknowledged frames), or after 100 ms without an ACK. The capture loop
keeps taking the newest image, as with TCP. A frame dropped anywhere on the way (full queue of
8, lane ended) is reported fetched when it is dropped, so the capture loop never waits for it.

### Pacing

The sender thread sends each frame in bursts of `udp-video-burst` datagrams (8, about 11 KB),
spaced by the token bucket at `max(1.5 x the measured bitrate, frame bytes / min(half the frame
interval, 25 ms))`, so even an IDR is out within half a frame period (the roadmap allows two)
and within 25 ms, well inside the viewer's 80 ms reassembly timeout also at a low frame rate.
Waits
use a high-resolution waitable timer (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`, Windows 10
1803+) for all but the last 0.5 ms, then a spin. The frame interval is an EWMA of the frames'
queue times, the bitrate the bytes sent over the last second. Every datagram is still sealed
and sent under the sender lock, so PONGs keep the seq order. The connected live socket gets a
qWAVE AudioVideo flow (DSCP 40, WMM video) unless `enable-lan-qos` is `N`: the lane is LAN only.

### QoS signal

`video_qos` adapts from TestDelay, which shares the TCP stream with the video; with the video
on UDP that stream is nearly idle and would report no congestion (the review's point). While a
lane is live, the connection replaces each TestDelay sample by: TCP delay + the largest excess
of the lane's ACK round trip over its baseline (the lowest of this and the previous 10 s
window) since the last sample + 150 ms if the viewer lost frames since. QoS keeps its own
thresholds (150 ms, confirmation by consecutive samples). `LATENCY server ... qos_sig=udp`
shows when this is in use.

### Falling back to TCP

The host ends a live lane when: the viewer is silent on UDP for `timeout_ms` (pings every
200 ms and ACKs every 50 ms keep it heard, also on a static screen), 5 datagram sends fail in a
row, sustained loss (above), or the viewer asks (`KindRequestStop`). It then:

1. puts the STOP marker `UdpVideoControl{KindStop, in_band, last_frames}` in the tap: written to
   TCP right before the next video item, which goes to TCP again; `last_frames` has the id of
   the last frame given to UDP per display;
2. asks every display that had UDP frames for a key frame (this also brings the next video item
   and with it the marker soon on a static screen);
3. offers a new lane after 30 s and after 120 s, then stays on TCP for the session.

The viewer asks for the stop when a display stays broken 1 s after a loss, frames wait 1 s for
ACTIVE or for the SwitchDisplay of their epoch, frames arrive for 1 s but only of an ended
epoch (none reaches the decoder), the host is silent for `timeout_ms`, or 5 receive errors come
in a row. It stops
receiving at once but keeps the lane's state until the in-band STOP: a display whose last UDP
frame did not reach the decoder takes TCP frames only from a key frame (`tcp_drop`), the others
go on without a gap. A TCP frame that comes while a lane is active (never, by construction) is
treated the same way. A STOP before ACTIVE (handshake timeout) goes out at once, as in shadow
mode. Kill switches: host `enable-udp-video` empty, `shadow` or `Y` (next login), viewer
`enable-udp-video=N` and a reconnect (needs no access to the host).

### Compatibility

- Stock viewers never advertise, so stock hosts' and stock viewers' sessions are untouched.
- An M19 viewer (v1) gets no live offer; it takes `shadow` offers as before. An M19 host offers
  only shadow mode, which this viewer takes.
- The first M20 build (viewer v2, live offers of version 1, no EPOCH markers): this host offers
  it no live lane, and this viewer refuses that host's live offers (a `KindRequestStop` with
  the reason); shadow works both ways.
- Every new field (`timeout_ms`, `epoch`, `last_frames`, `in_band`, `KindEpoch`, Misc 1004) is
  fork-only and sent only on a negotiated lane.

### Left out of M20

- **FEC** (done in M21, below), **RFI** (M22), **per-frame QoS feedback** beyond the ACK (M23),
  **WAN** (M24).
- **IDR spread over two frame periods per display**: the pacer sends every frame, an IDR
  included, within half a frame period; the bursts already keep it off the wire as one block.
- **Viewer queue-overflow and decode-error refreshes as forced key frames**: they still send
  `RefreshVideo`, which the host serves with a forced key frame when M13 is on, so nothing
  changes on the viewer.
- **Shared encoder**: all viewers of a display share its encoder, so a UDP viewer's key frame
  requests give every viewer a key frame; with M13 off they restart capture for every viewer.
- **A lost STOP marker cannot happen** (TCP), but a host that ends a live lane on a static
  screen with M13 off sends the marker only with the next captured frame; the viewer's lane
  stays quiet until then.

## FEC (M21)

Frame-level Reed-Solomon forward error correction, Sunshine style: a lost datagram is rebuilt
from parity the moment enough shards of its block are there, so a scattered loss costs no
key frame, no request and no wait.

### Code (`src/udp_video/fec.rs`, in-tree)

- Systematic erasure code over GF(2^8), polynomial 0x11D, generator 2. Data shards go out
  unchanged; parity shard `r` of a block is `sum_j C[r][j] * data[j]` with the Cauchy matrix
  `C[r][j] = 1 / ((255 - r) xor j)`. Every square submatrix of a Cauchy matrix is invertible, so
  any `d` of a block's `d + p` shards rebuild it (MDS) while `d + p <= 256`; blocks stay at 255
  or fewer.
- Decoding: the syndromes of `m` received parity rows (parity minus the present data's share)
  times the inverse of their `m x m` Cauchy submatrix (Gauss-Jordan) give the `m` missing
  data shards.
- `mul_add` (`dst ^= c * src`) uses AVX2 (32 B) or SSSE3 (16 B) nibble-table shuffles on x86-64,
  detected at run time, and a 256-byte row table elsewhere; the tests check them against
  each other for every coefficient.
- No crate: the roadmap named `reed-solomon-simd`, but new crates are out for this build
  (Cargo.lock is local-only). The in-tree code needs nothing but `std::arch`.

### Frame layout

- A frame's `n` data fragments (the M20 fragments, 1352 B on the LAN, the last one short) are
  split into `blocks = min(4, ceil(n / max_data))` blocks of as equal size as possible (the
  first `n % blocks` one larger). `max_data` is 200 data shards (roadmap), smaller only above
  27% (`d + parity <= 255`; 127 at 100%).
- Each block gets `max(2, ceil(d * percent / 100))` parity shards (Sunshine: 20%, at least 2),
  capped at `255 - d`. At 20% a frame above 4 x 212 fragments (about 1.15 MB) therefore gets
  less parity than the percentage (at 1000 fragments: 5 per block of 250); above 4 x 254
  fragments (about 1.37 MB) it goes without FEC (`fec_off` on the host).
- Shard length of a block = its first data shard (only the frame's last fragment is short, and
  it is never a block's first unless it is alone in it); the short last fragment is
  zero-padded in the code, not on the wire, so a frame without loss costs only the parity.
- Send order: block 0's data, block 0's parity, block 1's data, ... A block's parity is
  encoded only when the sender reaches it (after its data shards are queued for the socket),
  so the first datagram, and `send_us`, never wait for FEC; a later block's data waits for the
  block before it. The pacer spreads the whole frame, parity included, within half a frame
  interval (the frame bytes it sizes the rate on include the parity, computed without
  encoding).
- Overhead: 20% parity plus one datagram header per parity shard; a 1-fragment frame (cursor
  blink, small update) gets 2 parity datagrams (Sunshine does the same). Host CPU: one shard
  pass (1352 B) per parity shard per data shard of its block: about 155 KB of multiply-add for
  a 30 KB P-frame and 7 MB for a 300 KB IDR, estimated at tens of microseconds and under 1 ms
  with AVX2 (nibble tables for all 256 coefficients are built at compile time, so no
  multiply has setup cost; the scalar path off x86-64 does two table lookups per byte);
  `fec_ms` in the host's window line measures it.

### Receiver

- The reassembler keeps parity shards with their pending frame, per block. When a block holds
  at least as many parity shards as it misses data shards it rebuilds them at once and the frame
  completes (`fec_rec`, counted when it is delivered, so not for a frame gated after a loss);
  its rebuilt last fragment is trimmed back to the frame length. A
  frame complete without loss is delivered before its parity arrives; that parity is counted
  (`parity`) and dropped, neither late nor duplicate.
- Release rule, gate after loss, timeouts and ACKs are unchanged: a frame FEC cannot save is
  lost exactly as in M20 (key frame request, gate), and counted `fec_unrec` as well when any of
  its shards arrived (a frame of which nothing arrived is only `lost`).
- Shards that do not fit their frame (FEC fields differing between shards, a data fragment
  outside its block, parity of another length) make the frame corrupt, as inconsistent
  fragments always did; with authenticated datagrams this only happens on a sender bug.

### Negotiation and compatibility

- Viewer lane version 4 (`LoginRequest.udp_video_version`); offer field `fec_percent` (10).
  The host sets it from `udp-video-fec-percent` (default 20) for a v4 viewer, 0 for v1-v3.
- An M19/M20 viewer never gets parity (it would drop type 4 as an unknown type). An M20 host
  sends none to this viewer (no `fec_percent`), and this viewer takes such a lane as before.
- Stock peers never advertise or offer a lane, so nothing changes for them.
- The datagram wire version stays 1; offer versions stay 1 (shadow) and 2 (live).

### Left out of M21

- **Adaptive FEC** (`clamp(10, 50, 2 x loss% + 5)` from feedback): done in M23
  (`udp-video-fec-adaptive`), see "Per-frame feedback (M23)".
- **Recovered-frame count on the host**: the ACK layout is fixed (an M20 host rejects a longer
  ACK), so `fec_rec` is in the viewer's window line only; the host sees fewer `vlost`.
- **More than 4 blocks per frame**: very large frames get less parity instead (see layout).

## Per-frame feedback (M23)

The viewer tells the host what happened to every video frame, over the session stream (the
reliable channel), on any transport: direct TCP, KCP, relay, WebRTC, and the UDP lane. This is
the feedback channel of the roadmap's [ABR] step 8 and [UDP] step 6: it shows queueing on the
relay-to-viewer leg and inside the viewer, which neither TestDelay nor the host's TCP meter
sees, and it carries the lane's losses to the host's QoS and FEC.

### Negotiation and compatibility

- **Host -> viewer:** `PeerInfo.features.video_feedback` (Features 1001) = 1, set by a Windows
  host with `enable-video-feedback=Y` for remote-control logins (not file transfer, terminal,
  camera or port forward). Stock and older fork viewers ignore the field.
- **Viewer -> host:** `Misc.video_feedback` (1005, `VideoFeedback`), sent only by a Windows
  viewer with `enable-video-feedback` not `N`, only after such a PeerInfo, decided again at every
  login. A stock host never advertises, so it never receives one; an older fork host neither.
- Nothing else changes on the wire. The existing `UdpVideoFeedback` (1004) key frame request
  stays the immediate per-loss NACK (sent at once on the loss, repeated every 250 ms); the batch
  only reports the same ranges for statistics.

### What the viewer sends

`VideoFeedback { seq, sent_us, frames[], done[], lost[], udp, dropped }`, times in microseconds
on the viewer's feedback clock (started at the login; only differences mean anything):

- `frames`: one `VideoFeedbackFrame { display, pts, arrival_us, bytes, queue_len, udp }` per
  frame that entered the decoder path (`handle_video_frame`, TCP and UDP alike; TCP right after
  the parse). `pts` is the largest pts of the frame's encoded frames, the key both ends match by.
- `done`: one `VideoFeedbackDone { display, pts, decode_us, present_us }` per decoded frame, both
  relative to its arrival; `present_us` 0 = present pacing did not hand it to the UI. The video
  thread writes each display's `DoneLog` (one relaxed load per frame while off); a frame seen
  arriving before feedback started is left out.
- `lost`: the lane's loss ranges `{ display, from, to, repaired }` (lane frame ids).
- `udp`: the lane receiver's counters since the previous batch (datagrams expected / received,
  frames completed / lost, FEC recovered / unrecoverable), taken by the receive thread every
  50 ms (`ViewerEvent::Counts`) as deltas of its window statistics.
- Rate limit: a batch at most every 50 ms, only when something is in it, flushed after a message
  or a lane event was handled and on the 1 s status tick; at most 64 frames, 64 done entries and
  16 ranges per batch, the rest counted in `dropped`. At 60 fps about 20 batches of about 60 B
  per second.

### What the host does with it

`SendLog` (shared with the split writer) records `(display, pts, time, bytes, udp)` of every
video item as it leaves the video channel: right before the TCP write, or before the lane's
queue and pacer. `Estimator` (one per connection) takes each batch (at most 40 per second; more
are dropped and counted) and produces a `FeedbackInput`:

| Field | Meaning |
|---|---|
| `q_owd_ms` / `q_owd_max_ms` | per-frame one-way delay (viewer arrival - host send, clock offset included) minus its lowest value over the last 10 s (1 s buckets, dropped by age as well as count); the batch's smallest and largest. The smallest is the queue: a large frame's own transmission time does not count (the fast ABR's `min(Q) over 100 ms` idea). TCP (possibly through hbbr) and the lane (its queue, pacer and the viewer's reassembly hold included) have different fixed delays: a change of transport between two matched frames clears the baseline and the trendline's previous frame, and the value is None for the first 1 s of a baseline |
| `trend`, `threshold`, `usage` | GCC (draft-ietf-rmcat-gcc-02, the defaults of WebRTC's `TrendlineEstimator` and `OveruseDetector`): delay gradient per frame, accumulated, smoothed 0.9, slope over 20 frames x min(n, 60) x 4; adaptive threshold from 12.5 (k_up 0.0087, k_down 0.039, 6..600); overuse after more than 10 ms and two samples above it with a rising trend |
| `jitter_ms` | RFC 3550 style, gain 1/16 |
| `recv_kbps` | the batch's frame bytes over their arrival span (from the previous batch's last arrival) |
| `viewer_ms`, `decode_ms`, `queue_len` | the viewer's own delay (largest of the batch) and decoder queue |
| `udp`, `loss`, `frame_loss` | UDP lane: datagram loss and frame loss after FEC over the last 1 s |
| `fec_recovered`, `lost_frames` | UDP lane, this batch |

Clocks: the viewer's clock is never compared with the host's directly. Drift between the two
(tens of ppm) moves the 10 s minimum by well under a millisecond; the buckets expire by age, so
a sparse screen does not keep a minute-old minimum. The viewer's counters are taken at most
1,000,000 per batch and every sum saturates: a broken or hostile viewer cannot overflow them.

Uses in this build, each behind its own host option (all off by default):

- **QoS** (`video-feedback-qos=Y`, `LegacyQos`): the delay sample video_qos gets from each
  TestDelay (or the live lane's sample, see "QoS signal") becomes `max(sample, q_owd)`, `q_owd`
  being the largest batch value since the previous sample, plus `LOSS_PENALTY_MS` (150) when the
  lane's datagram loss over 1 s went above 10% (the threshold of GCC's loss controller). Frame
  losses after FEC add the penalty through the lane's own sample (`UdpVideoHost::qos_delay`
  says when); then `LegacyQos` adds none, so one sample carries at most one penalty.
- **Adaptive FEC** (`udp-video-fec-adaptive=Y`, `FecAdapt`): for a live lane that has parity
  (viewer lane v4, `udp-video-fec-percent` not 0) the percentage follows
  `clamp(10, 50, 2 x loss% + 5)` of the datagram loss over 1 s: raised at once, lowered after
  2 s below. The sender thread reads it per frame (`Shared::fec_now`); the viewer needs nothing,
  every datagram carries its block's parity count. A new lane starts from the configured value.
- **Fallback** (`video-feedback-qos=Y`): a live lane that loses more than 10% of its frames
  after FEC over 3 s (at least 30 frames) goes back to TCP (`udp_video_stop`, with the same retry
  backoff as the other reasons). Frames the host's tap dropped on a full lane queue took a frame
  id and were never sent, so the viewer counts them as lost too; the lane's running count of
  them (`UdpVideoHost::qdrop_total`) over the same window is taken off first, and the stop reason
  names it. The existing rules (4 key frames in 5 s, silence, socket
  errors) stay.

### Seam to the fast ABR (M15/M16, the `rate` stream)

The fast estimator (`src/server/video_qos/fast.rs` on `feature/b3-rate`) takes TCP meter samples
through `FastEstimator::on_tcp(TcpInput)`. The feedback is its second input, defined here so
the merge only has to connect them:

```rust
// src/video_feedback/host.rs
pub struct FeedbackInput { pub at: Instant, pub frames: u32, pub q_owd_ms: Option<f64>,
    pub q_owd_max_ms: Option<f64>, pub trend: f64, pub threshold: f64, pub usage: Usage,
    pub jitter_ms: f64, pub recv_kbps: Option<f64>, pub viewer_ms: Option<f64>,
    pub decode_ms: Option<f64>, pub queue_len: u32, pub udp: bool, pub loss: Option<f64>,
    pub frame_loss: Option<f64>, pub fec_recovered: u64, pub lost_frames: u64 }
pub trait FeedbackSink { fn on_feedback(&mut self, input: &FeedbackInput); }
// ConnFeedback::sinks: Vec<Box<dyn FeedbackSink + Send>>, called for every batch.
```

At the merge:

1. `impl FeedbackSink` for a small adapter holding the connection's `Arc<ConnFastAbr>`; its
   `on_feedback` forwards to a new `FastEstimator::on_feedback(&FeedbackInput)` under the
   estimator's lock (the Connection task calls it, as it calls `on_dequeue` and `on_sent`).
2. In `Connection`, right after `video_feedback` is created at PeerInfo, push that adapter into
   `ConnFeedback::sinks`.
3. In the estimator: `Q = max(existing terms, q_owd_ms)`; `usage == Overuse` counts as the
   delay-based overuse signal on transports without a meter (relay, KCP, WebRTC); `recv_kbps` is
   a delivered-rate sample (`R_acked`) where the meter has none; `loss > 0.10` cuts by
   `(1 - 0.5 x loss)` and `loss < 0.02` allows the increase (GCC's loss controller); a large
   `viewer_ms` with a small `q_owd` is the RECEIVER_LIMITED case (the viewer, not the network:
   no bitrate cut).
4. With `enable-fast-abr` live, `LegacyQos` steps aside (`ConnFeedback::qos_delay` only while the
   fast loop is off), so video_qos is not fed the same delay twice.

### Left out of M23

- **Feedback-driven RFI**: the per-loss request stays the key frame request of M20; M22 (below)
  answers that same request with RFI where it can.
- **Per-frame NACK with retransmission**: a lost UDP frame is repaired by FEC or a key frame,
  never resent (a resend arrives too late for a 60 fps stream on the LAN).
- **The fast ABR itself on this stream**: only the seam; `LegacyQos` is what this build uses.
- **Feedback over the UDP lane**: the batches ride the session stream, so they still arrive
  when the lane fails, which is what the fallback needs.
- **Pacing or bitrate decisions from `trend` in this build**: `usage` is logged and handed to
  the sinks; `LegacyQos` uses only `q_owd` and loss. The roadmap gate (a throttled
  hbbr-to-viewer leg) decides how much more the signal is worth.

## RFI (M22)

Roadmap M22: after a loss FEC could not repair, the viewer resumes without an IDR spike when
detection plus the round trip stay within about 4 frames. Host option `enable-nvenc-rfi=Y`
(off by default), live lanes of a viewer that advertises lane v5, NVIDIA GPU, HEVC.

### Encoder: hwcodec's NV SDK driver

FFmpeg's `hevc_nvenc` exposes no reference frame invalidation (and an FFmpeg patch would mean
a full vcpkg rebuild, roadmap "dropped"), so RFI runs on hwcodec's own NVENC SDK driver
(`cpp/nv/nv_encode.cpp`, Video Codec SDK 12.1.14 samples with the 12.1 `nvEncodeAPI.h` of
nv-codec-headers n12.1.14.0, which declares `nvEncInvalidateRefFrames` and
`NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION`). Fork changes (docs/patches/README.md):

- `nv_set_rfi(on)` before an encoder is opened (RustDesk sets it at every encoder creation:
  on for HEVC on NVIDIA with the option while a lane wants RFI, else off): the encoder runs with preset P2 and ultra-low-latency tuning (FFmpeg
  nvenc's settings in RustDesk), logs the caps `SUPPORT_REF_PIC_INVALIDATION`,
  `SUPPORT_INTRA_REFRESH`, `SINGLE_SLICE_INTRA_REFRESH` and `SUPPORT_MULTIPLE_REF_FRAMES` of
  both codecs, and offers forced IDRs and `encode_last` (the single input buffer, zero output
  delay, still holds the last image). HEVC with the RFI cap also gets `maxNumRefFramesInDPB=5`
  and `numRefL0=1` (when multiple references are supported at all).
- `inputTimeStamp` is a per-encoder frame counter, so a frame is named unambiguously; a ring of
  the last 16 frames since the last IDR maps the capture pts to it.
- `nv_invalidate_refs(pts)`: invalidates every frame from the oldest one encoded with that pts
  to the newest, if a valid reference is left: a frame before it, after the last IDR, not
  invalidated before, among the last 5 (the DPB). So at most 4 frames from the lost one to the
  newest; beyond that (or for an unknown pts) it returns -2 and the caller forces an IDR. The
  next encoded picture is the recovery point: callback key 2 (1 if it is an IDR); an intra
  picture NVENC may choose instead is also reported as 2, not as a key frame (no SPS/PPS, not
  a start for a new decoder).
- H.264 is left out on purpose: a viewer that skipped invalidated frames sees a `frame_num` gap,
  which H.264 decoders treat as an error (concealment) unless the SPS allows gaps; HEVC names
  its references explicitly (RPS) and a decoder never misses the skipped pictures. H.264
  sessions keep key frames.

RustDesk side (`libs/scrap/src/common/vram.rs`): an HEVC encoder on an NVIDIA adapter is
opened with the NV driver and `nv_set_rfi(true)` (same LUID) while a connection holds
`RfiEncoderWanted` (from the offer of a lane with `rfi` until the lane ends), whether
`HwCodecConfig` picked FFmpeg nvenc or (FFmpeg failed for that adapter) the NV driver itself.
Without such a lane every encoder opens as upstream, `nv_set_rfi(false)`. If the running
HEVC encoder on NVIDIA was opened without RFI when a lane starts wanting it, the capture loop
restarts once (`reason=nv_rfi`). The NV driver is not taken from `HwCodecConfig` (the check process tests it
only when FFmpeg fails for that adapter), so its first open happens in the host process: a
failing open or failing encodes (`disable`) set a process-wide flag and the encoder opens as
before (FFmpeg nvenc, or the NV driver without RFI); hardware encoding is not disabled for it. The encoder stays NV after the lane ends until the
next encoder start. Not applied on the NV path: `nvenc-vbv-frames` and static refine (FFmpeg
only); the M13 forced key frame and its kept-image path work (`request_keyframe` and
`encode_last` are now supported by this driver when `nv_set_rfi` was on).

### Flow

1. The viewer's loss report (`UdpVideoFeedback{request_keyframe, display, lost_from, lost_to}`,
   unchanged since M20; repeated every 250 ms while the display waits) reaches the host.
2. A lane with `rfi` maps `lost_from` (lane frame id) to the frame's pts: the tap remembers
   (display, id, pts) of the last 128 frames and forgets a display's frames before its newest
   key frame. Found: that pts becomes the display's pending first lost pts on this lane (the
   minimum if one is pending) and the lane, under its tap's lock, calls
   `rfi::request(display, lane, pts)`. Not found: a key frame through the refresh path as
   before.
3. The capture loop of that display takes the request (requests of several lanes merge to the
   oldest pts; one atomic load per iteration when there is none; a request not taken within
   1 s is dropped, e.g. when the viewer switched away and that display's capture loop stopped:
   the viewer's repeated report asks again while it still waits) and calls
   `invalidate_refs(pts)`. Done: it remembers the bound (the oldest pts invalidated since the
   last recovery frame). Unsupported or too late: `OPTION_REFRESH`, i.e. the M13 forced key
   frame, or a capture restart without M13, exactly as a key frame request (`rfi_idr`).
4. The next encoded frame (a captured one, or with no new image within one capture wait the
   kept image, `rfi_kept`) carries the encoder's recovery flag. Before it goes to the
   connections the capture loop registers (display, pts, bound) in `rfi`; a key frame instead
   drops the wait.
5. Every lane's tap looks the frame up. It sets `RECOVERY_POINT` only for a display with a
   pending first lost pts F and a bound <= F: the frame references only frames encoded before
   F, all of which this viewer decoded (any frame it skipped before was either invalidated
   when it was repaired, or before an IDR). A recovery frame made for another viewer's later
   loss is not a recovery point here; this viewer waits for its own. A key frame clears the
   pending pts too. A wait that ends this way withdraws the lane's request if the capture loop
   has not taken it yet (`rfi::withdraw`): when two viewers lose the same burst, the first
   one's recovery frame usually repairs the second's later lost frame too, and invalidating
   again from that frame would find no valid reference left and force a key frame for every
   viewer. If the capture loop took the second request first, that key frame still follows. A
   flagged frame dropped from the full queue puts the pending pts and the request back.
6. The viewer's receive thread already lifted its loss gate on `RECOVERY_POINT` (M19 wire
   format); since lane v5 the io_loop also resumes a broken display at such a frame (`rp`,
   `fix_ms`). The frame is a P frame: `EncodedVideoFrame.key` stays false, nothing changes on
   the session stream or for TCP viewers of the same encoder, which simply decode it.

Losses repaired by RFI still count toward sustained loss (4 key frame requests or tail losses
within 5 s end the lane). A lost recovery frame shows as the display still broken: the viewer
repeats its request with the first lost frame, which by then is usually too far back, so a key
frame repairs it.

### Compatibility

- Only a v5 viewer's lanes get `rfi`; older fork viewers and every stock peer get key frames as
  before. The flag value already existed in the M19 wire format, so no datagram change.
- `enable-nvenc-rfi` off (default): no `RfiEncoderWanted`, `nv_set_rfi(false)`, the NV driver
  behaves as upstream (forced key frames and `encode_last` refused, as before this change), and
  the lane never asks for RFI.
- Multiple viewers share the encoder: one viewer's recovery frame is a plain P frame for the
  others; an invalidation costs them nothing but a slightly older reference.

### Left out of M22

- **Gradual intra refresh** (`enableIntraRefresh`, `forceIntraRefreshWithFrameCnt`): never as
  loss recovery (the frames after a loss reference the lost one until the wave completes, which
  breaks the no-broken-reference rule) and never for `RefreshVideo` (roadmap); only the caps are
  logged.
- **RFI for H.264** (see above) and for AMF/QSV (no SDK path in hwcodec).
- **Tail-loss repairs by RFI**: the host's own tail loss check still asks for a key frame; such
  a loss is one small frame on a static screen.
- **Direct NV driver parity measurements** (roadmap step 6): encode p50/p95, kbps, keymax and a
  colour check against FFmpeg nvenc (the NV driver converts ARGB inside NVENC and its VUI
  says SMPTE170M, not verified against the FFmpeg path's output), and the HEVC caps on the RTX 5060 Ti: the
  first run with the option on logs them. The NV driver has no VBV option (`nvenc-vbv-frames`)
  and no static refine.
- **Hardware validation**: Iris Xe and the RTX viewer decoding a stream with skipped
  invalidated frames, recovery time and spike size against IDR (the roadmap gate).

## Internet lane (M24)

The LAN lane of M19-M23 over the internet: the same wire format, crypto, live mode, FEC, RFI,
feedback and fallback, carried by a UDP socket of its own on each end that both ends punch
through their NATs. The roadmap's other carriers (KCP socket mux, WebRTC data channel over
TURN) are left out, see below.

### When

The host offers an internet lane only where the LAN gate refuses (a LAN session keeps its LAN
lane, unchanged) and all of these hold:

- host option `enable-udp-video-wan=Y` (off by default) and `enable-udp-video` = `live` or
  `shadow`, as for a LAN lane;
- the viewer advertised lane **v6**: a Windows viewer with `enable-udp-video-wan` not `N` whose
  session is direct (`connect` won with TCP punch, direct IP access, or KCP over a punched IPv4
  UDP socket) and which knows the host's address there, off the LAN
  (`LoginConfigHandler::udp_video_host`, set by `Client::connect`; None on relay, WebRTC and KCP
  over IPv6), and with no SOCKS proxy or WebSocket set on the viewer (`viewer::wan_allowed`: the
  lane's UDP would bypass the proxy and show the viewer's own address, and the host sees the
  proxy's address as the peer). A viewer that cannot address the host never advertises v6, so
  the host never offers a lane it could not use;
- the host's transport (`TransportInfo`) is direct: intranet, TCP punch, direct or KCP-UDP
  (never relay or WebRTC: the address the connection keys on would not be the viewer's). Not
  KCP-IPv6 either: `udp_nat_listen` keys that connection on the viewer's IPv4 address as hbbs saw
  it, not on the IPv6 peer the KCP stream runs on, so the two ends would never meet
  (`connection::udp_video_gate`);
- the session stream is encrypted (the lane secret travels in it);
- the viewer's address (the session peer, normalized) is a unicast address off the LAN
  (`net::wan_ip`: not loopback, unspecified, link-local, multicast; CGNAT and overlay addresses
  in 100.64.0.0/10 count as internet, but skip the NAT query, see the handshake);
- no SOCKS proxy or WebSocket is set on the host (no UDP to the rendezvous server).

The viewer takes an internet offer only with `enable-udp-video-wan` not `N`, on a round whose
session is direct, encrypted, not WebRTC, with a known host address off the LAN
(`check_wan_offer`: the offer's `max_datagram` at most the WAN budget of that family).

### Handshake

1. **Host:** binds the wildcard of the viewer's family (port 0), and on the lane's receive
   thread asks the rendezvous server (`Config::get_rendezvous_server()`, UDP port 21116) for the
   socket's public port with the `TestNatRequest` the session's own UDP punch uses (every
   150 ms, up to 1 s; only the server's reply counts). The server's DNS lookup is bounded at 1 s
   (`net::DNS_FOR`, on a helper thread). No query when the viewer's address is in
   100.64.0.0/10 (Tailscale and other overlays, an in-carrier CGNAT path): the port the
   rendezvous server sees belongs to another path, the local port is the one this path reaches.
   The offer (`wan=true`, no `ip`, `port` = that public port, or the local port without an
   answer, on IPv6 or toward 100.64/10, `max_datagram` 1024 or 1184) then goes to the
   connection's message channel. The connection never waits for the query: it runs the session.
2. **Viewer:** binds the wildcard of the host's family, asks its own rendezvous server the same
   way on the lane thread (same DNS bound, no query toward a host in 100.64/10), and reports its public port (or local port) as
   `UdpVideoControl{KindCandidate, port}` over the session stream. Then it `connect()`s to the
   host IP it reached the session on, at the offer's port, and sends HELLO every 20 ms for up to
   3 s (LAN: 1 s).
3. **Host:** on the candidate, the receive thread sends a punch every 20 ms to the viewer's
   session IP at that port: a HELLO sealed with the host's keys (the viewer ignores it if it gets
   through). It opens the host side's NAT mapping and filter toward the viewer. The first HELLO
   from the viewer's IP that opens with the lane keys ends the punch; the host `connect()`s to
   its source (which may differ from the candidate) and answers HELLO_ACK, as on the LAN.
4. READY, ACTIVE, video, ACKs and the rest as on the LAN. The host's handshake timeout is 6 s,
   counted from the moment the offer went out (after the host's lookup and query): the viewer's
   lookup and query, the candidate's trip and the HELLOs.

Both ends send first, so a port-preserving or endpoint-independent NAT on either side, and an
address-dependent filter on either side, are crossed. A NAT that maps per destination (symmetric
NAT, many CGNATs) gives the peer a port it will never see traffic from: the handshake then times
out and the session stays on its transport. The rendezvous server must answer UDP
`TestNatRequest` (hbbs does, it is the UDP punch's query); without an answer the local port is
offered, which works only behind a port-preserving NAT.

### On the way

- **Datagram size:** at most 1024 bytes of UDP payload over IPv4 and 1184 over IPv6 (Moonlight's
  values: tunnels, PPPoE, mobile), so 976 / 1136 bytes of video per fragment; the pacer's burst
  is counted in those datagrams. LAN lanes stay at 1400.
- **No DSCP:** the qWAVE AudioVideo flow is added only to a LAN lane's socket; providers bleach
  or misread DSCP.
- **NAT keepalive:** the viewer's PINGs (every 200 ms) and live ACKs (every 50 ms) refresh its
  mapping, the host's PONGs and video refresh the host's. The host's receive thread also sends
  a PING when it has sent nothing for 500 ms (the roadmap's heartbeat), so its mapping survives
  lost PINGs on a static screen.
- **Fallback:** as on the LAN: silence for `udp-video-timeout-ms` (1000 ms by default, both
  ends), send errors, sustained loss or the viewer's request end the lane, the video goes back
  to the session stream with a key frame per display and the in-band STOP. The next offers come
  after **10 s, 30 s and 120 s** (`RETRY_BACKOFF_WAN`; LAN 30 s and 120 s), each with a fresh
  socket, NAT query, secret and lane id, then never again in that session. Unlike a LAN lane, an
  internet lane is also retried when its handshake failed (host timeout, the viewer's handshake
  failure or refusal: `StopInfo::wan_handshake`): a lost punch or NAT reply is often transient.
  The retries share the one budget of three.
- Encryption, replay window, FEC, RFI and the per-frame feedback are the LAN lane's.

### Compatibility

- A stock or older fork viewer never advertises v6: it never gets an internet offer, and the
  host's LAN behaviour toward it is unchanged. A stock host ignores the version.
- A fork viewer on a LAN session advertises v5 (the host address is on the LAN) and gets the
  LAN lane as before; on a relay or WebRTC session it advertises v5 too (the address is cleared
  at the start of every connect and set only by a direct win), and the host's transport gate
  would refuse anyway.
- `KindCandidate` is sent only after a `wan` offer, which only a v6-aware host makes.
- Other platforms compile the code; only Windows hosts offer and only Windows viewers advertise.

### Left out of M24

- **KCP socket mux** (roadmap 4a): the lane on the KCP session's own punched socket, demuxed in
  `kcp_stream.rs` by an outer header byte. The host's connection never sees the KCP socket
  (`udp_nat_listen` keeps it in its own future), and a dedicated punched socket covers KCP
  sessions as well (it costs one more NAT mapping and query). The 1024/1184 budget is used
  instead of KCP's 1214-byte datagrams.
- **WebRTC unreliable data channel and TURN** (roadmap 4c): needs a negotiated `webrtc-rs`
  channel outside the `Stream` enum in hbb_common; relay sessions (most of them caused by a
  symmetric NAT, where a re-punch fails by construction) and WebRTC sessions keep their video on
  the session stream. "Relay via TURN only if configured" therefore means: never, in this build.
- **Several candidates** (LAN addresses next to the public one, both families, local and mapped
  port): one candidate per side; the session's own family decides, and a 100.64/10 peer gets the
  local port only.
- **KCP over IPv6:** the host's connection would need the KCP stream's IPv6 peer (for example in
  `TransportInfo`) as the gate's peer; until then such a session gets no internet lane (the
  viewer advertises v5, the host's gate refuses).
- **A host-key-derived lane key** (HKDF over the session key through a new `FramedStream` API):
  the lane keeps its fresh secret sent over the encrypted session stream (see Encryption).
- **Moving the session itself** between paths (relay to direct): out of scope.
- **Measurements:** the roadmap gate (phone hotspot and another network: lane loss, jitter and
  longest gap per 10 s; no multi-frame stall at 1-3% loss) needs two networks; the window lines
  give the numbers (docs/LATENCY-METRICS.md, "UDP video lane over the internet (M24)").
