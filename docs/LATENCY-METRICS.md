# Video latency metrics ("step 0")

Per-stage latency numbers for the video path. They are measurement only: no
frame rate, bitrate, queue or transport behaviour changes, and there is no wire
protocol change, so the build stays compatible with stock RustDesk peers. The one
viewer behaviour they report on, present pacing, is described in "Viewer: present
pacing" below.

All numbers are wall-clock durations on one machine's monotonic clock, except
the two marked **(est)**, which need the host's clock and are estimated (see
"Clock estimate" below).

Code:

| Side | File | Hooks |
|---|---|---|
| Host | `src/server/latency_stats.rs` | `src/server/video_service.rs` `run()` and its restart wrapper in `new()`, `src/server/connection.rs` (select loop, `rx_video` video frame write, `refresh_video_display`, split session), `src/server.rs` `create_tcp_connection` (`TransportInfo`), `libs/scrap/src/dxgi/mod.rs` (`last_present_age`), `src/platform/windows_qos.rs` (TCP meter, `HostSocket`), `src/session_io.rs` (split writer), `src/udp_video/host.rs` (UDP lane, `UDPVIDEO host`) |
| Viewer | `src/client/latency.rs` | `src/client/io_loop.rs` (receive, TestDelay, 1 s status timer, `fps_control`, writes of UI messages), `src/client.rs` `start_video_thread` (`VideoCounters`, `PresentPacer`), `AudioBuffer` / `start_audio_thread`, `src/client/audio_target.rs` (adaptive audio target), `src/platform/windows.rs` (`viewer_thread_boost`, `viewer_process_boost`), `RaceLog`, `VideoHandler`, `src/client/audio_playback.rs` (underruns), `src/ui_session_interface.rs` `Session::send`, `src/flutter.rs` (soft-render handoff, drop and replace, `next_rgba`, quality status), `src/udp_video/viewer.rs` and `shadow.rs` (UDP lane, `UDPVIDEO viewer`) |
| Flutter | `flutter/lib/models/model.dart` (`ImageModel.onRgba`, `_PaintLatencyStats`, `QualityMonitorModel`) | `flutter/lib/common/widgets/overlay.dart` (`QualityMonitor`) |

## Turning it on

- **Viewer (Quest / Android, Windows client):** open the session toolbar and
  turn on **Show quality monitor**. While the overlay is on, the viewer measures
  and logs. Alternatively set the local option `enable-latency-log` to `Y`
  (`RustDesk_local.toml`, `[options]`), which logs without the overlay.
- **Host (Windows):** set the server option `enable-latency-log` to `Y`, for
  example from an elevated prompt on an installed host:
  `rustdesk.exe --option enable-latency-log Y` (`N` turns it off again). The
  capture loop re-reads it every 5 s.

The option is compared with `"Y"` directly: `option2bool()` would treat any
`enable-*` key as on by default.

When logging is off the hooks still run, but return after one atomic load
(viewer: the number of measured streams, so the shared lock is not taken) or
one `bool` check (host capture loop; the connection's video-write hook is two
clock reads around the write plus one atomic load). The Flutter side checks the overlay flag and re-reads
the option at most every 5 s.

## Report format

Every 5 s per display, one line. A triplet `a/b/c` is **p50/p95/max in ms**
over the frames of that 5 s window (at most 1024 samples per window; nearest-rank
percentiles). `n/a` means no sample in the window.

### Host

```
LATENCY server display=0 fps=29.8 spf=33ms grab=… encode=… wait=… send=… sleep=… sent=149 skipped=0 idle=12 send_slow=0 age=… kbps=8130 set_kbps=4096 maxf=61234 key=1 keymax=61234 qchg=1 qchg_key=0 gap=n/a pacing=deadline reanchor=0 rkey=0 rkept=0 rmerge=0 refine=0 qos_sig=tcp
```

| Field | Meaning |
|---|---|
| `fps` | frames handed to at least one connection, per second |
| `spf` | current frame period from QoS (1000 / target fps) |
| `grab` | capturer call until it returned the image. **Includes the wait for the desktop to change**: DXGI `AcquireNextFrame` blocks up to `spf` truncated to ms, or ceil(`spf`) + 2 ms with `pacing=deadline` (in-process DXGI only; the portable service takes the timeout as its own frame period and keeps `spf`). A large `grab` p50 on an active screen means the capture is waiting, not slow. The once-per-second display check, which runs after the frame's `pts` is taken and before the capturer call, is not in `grab` (it is in the viewer's `net+server(est)`). |
| `encode` | after grab until the encoded frame was put on every connection's queue: BGRA→YUV conversion (RAM path), encode, protobuf, `send_video_frame` |
| `wait` | after that until every connection had **taken the frame off its queue to start sending** ("fetched", `connection.rs` `rx_video`). It is the time the frame waits behind the previous frame's socket write. It is not the time until the peer received it: bytes can still sit in the kernel send buffer. The previous frame's write overlaps this frame's sleep, grab and encode, so only the part of `send` that outlasts them (about one `spf`) shows up here; the rest of `wait` is the connection's select loop busy elsewhere (input, audio, file transfer), or, for a web client, the viewer's late ack. |
| `send` | one connection's `stream.send` of one video frame. Over TCP: protobuf serialization, encryption and the write into the socket, including any wait for send-buffer space (backpressure). Over WebRTC it is the data-channel queueing, over a relay the write to hbbr. One sample per connection per frame, all connections of the display merged. Not the time until the peer received the frame. A capture-loop restart (`SWITCH`) drops that display's partial window. A camera and a monitor with the same index share one `send` store, so either line can hold the other's writes and only part of its window. |
| `send_slow` | how many of those writes took more than a fixed 16 ms (counted even beyond the 1024-sample cap) |
| `sleep` | the sleep the loop asked for after `wait` (only iterations that slept): the rest of the frame period (`pacing=relative`), or the time left until the next absolute deadline (`pacing=deadline`). It is the requested sleep, not a measured one. |
| `sent` / `skipped` | captured frames that were / were not handed to a connection (encoder produced nothing, e.g. libvpx frame dropping, or no subscriber) |
| `idle` | iterations where the capturer had no new image within its wait (`spf`, or for DXGI with `pacing=deadline` ceil(`spf`) + 2 ms): static screen. On moving content it must stay about 0. |
| `age` | Windows DXGI capture only: how old the image was when `AcquireNextFrame` returned it, QPC now minus the frame's `LastPresentTime` (the time DWM presented it), p50/p95/max ms over the frames in `sent`/`skipped`. A sawtooth (p50 low, p95/max close to `spf`) means the loop samples the desktop out of phase with presents. `n/a` with GDI, magnifier, portable-service and camera capture, and on other platforms (no present time). |
| `kbps` | encoder output: bytes of all encoded frames of the window × 8 / window length. Repeat encodes and frames no connection took (`skipped`) are counted too: it is what the encoder produced, compare it with `set_kbps`. |
| `set_kbps` | the bitrate the encoder was last set to (`encoder.bitrate()`, kbps; at capture start and after each `set_quality`). With NVENC opened at 30 fps and run at 60 fps, `kbps` ≈ 2 × `set_kbps` is expected (`vram.rs`, framerate 30). |
| `maxf` | largest encoded frame of the window, bytes |
| `key` | key frames encoded in the window |
| `keymax` | largest key frame of the window, bytes (0 when none) |
| `qchg` | QoS quality (bitrate) changes applied with `set_quality` in the window |
| `qchg_key` | how many of those were followed by a key frame as the next encoded frame. 0 with `qchg` > 0 confirms the FFmpeg patch 0009 (no IDR on a bitrate change) is in the binary. |
| `pacing` | Windows host: `deadline` (`enable-capture-deadline-pacing`, on by default): each tick is due one `spf` after the previous deadline, so oversleep and loop overhead do not add up; when the loop is more than one `spf` past its deadline it starts a new schedule from now (no catch-up burst). Within one `spf` of lateness the next iteration starts at once. `relative` (option `N`, and on other platforms): the loop sleeps `spf` minus this iteration's time, so each iteration's oversleep adds up (measured 58.6 fps at a 60 fps target). Read when the capture loop starts. |
| `reanchor` | `pacing=deadline`: how often the loop was more than one `spf` late and started a new schedule (long `wait`, a stall, a camera that delivers slower than the target). An iteration without a new image (`idle`; the DXGI timeout is `ceil(spf)` + 2 ms) that ends past its deadline also starts a new schedule from now, but is not counted. Always 0 with `relative`. |
| `gap` | capture restarts completed in the window (see `LATENCY switch` below): from the refresh request or the restart until the restarted capture loop sent its first frame to a connection, p50/p95/max ms. With `enable-refresh-keyframe` it also holds the refreshes served by a key frame: from the request until that key frame reached a connection. |
| `rkey` | Windows host, `enable-refresh-keyframe=Y`: refreshes served by a forced key frame on the running encoder (no restart), counted when the key frame reached at least one connection. Their gaps are in `gap` and in `LATENCY switch` lines with `reason=refresh_keyframe restarts=0`. |
| `rkept` | how many of `rkey` were encoded from the kept last image because the screen was static (no new capture within min(2 × `spf`, 100 ms) of the request): VRAM re-encodes its NV12 surface (`encode_last`), YUV encoders the kept YUV buffer |
| `rmerge` | refreshes that did not get a key frame of their own: merged into a key frame that was already requested, or deferred because a key frame went out less than 200 ms before (they get the next one; never dropped) |
| `qos_sig` | the delay signal `video_qos` adapts fps and bitrate from: `tcp`, the TestDelay round trip on the session stream (always, unless a live UDP lane exists); `udp` while at least one connection of this host has an active live UDP video lane (M20): that connection's TestDelay samples are replaced by TCP delay + the largest queueing excess of the lane's ACK round trips + 150 ms if the viewer lost frames since the last sample (`UDPVIDEO host` `ack_rtt_ms`, `base_ms`, `vlost`). Process-wide, not per display |
| `refine` | static refine (on with `nvenc-vbv-frames` > 0 when the running encoder is FFmpeg nvenc, the one that gets the VBV): re-encodes of the kept last image after a key frame or a large frame (> 2 × the running average size) once the screen stayed unchanged for 150 ms, at most 3 per such frame, one per loop iteration. They are in `kbps`, `maxf` and `key`, not in `sent`, and they give no `wait` sample (neither do `rkept` key frames), so `wait` compares the same captured frames whether refine is on or off. |

`age`, `kbps`, `maxf`, `key`, `keymax`, `qchg`, `qchg_key`, `reanchor`, `rkey`, `rkept`,
`rmerge` and `refine` are only counted while logging is on, like the other fields; `set_kbps` is
kept always.

**With a live UDP video lane (M20) `wait` and `send` mean something else** for that
connection's frames, so compare TCP and UDP runs field by field with care:
- `send` is the lane sender thread's time for one frame: serialize, fragment, seal and `send`
  every datagram, **including the pacer's waits between bursts** (a big frame is spread over up
  to half a frame period on purpose). It no longer contains any socket back-pressure.
- `wait` ends when the lane reports the frame fetched: once it is sent and at most 3 frames of
  that display are not yet settled by the viewer's ACKs (or after 100 ms). It is the send
  window's back-pressure, the UDP replacement for the TCP write blocking.
- `send_slow` counts paced sends over 16 ms (an IDR of several hundred kB at a low bitrate).

Not covered: the re-encode of the last image that non-latency-free encoders
(mediacodec, videotoolbox) do on idle iterations.

### Host: key frame refresh, VBV and static refine (roadmap M13/M14)

Windows host, server options (`rustdesk.exe --option <key> <value>` on an installed host). All
three are off by default in this build.

- **`enable-refresh-keyframe`** (`Y` turns it on; read on every refresh). A viewer's
  `RefreshVideo` (its fps control, decode errors, queue overflow, the session menu, `PeerInfo` at
  login) then asks the running encoder for a key frame instead of restarting capturer and encoder
  (`SWITCH`). Stock viewers recover on any key-flagged frame, so nothing changes on the wire.
  Encoders: FFmpeg `hevc_nvenc`/`h264_nvenc` (VRAM and RAM): `pict_type I` with `forced-idr=1`;
  VP8/VP9/AV1: `FORCE_KF`. AMF, QSV, MFX and the direct NVENC SDK VRAM driver (which has no
  `encode_last` for a static screen) keep the restart (`reason=refresh_unsupported`). Still a restart: a real display change
  (`display_changed`), a pending screenshot (`refresh_screenshot`), and the escalations:
  - `refresh_escalated`: a refresh after 3 refreshes were served by key frames within 3 s (log
    line `refresh escalated to SWITCH`; should stay about 0),
  - `refresh_kept_again`: a refresh 200 ms to 3 s after the previous one was served from the
    kept image, with no new captured image since (same log line). The kept image cannot show a
    stalled capturer, so a second Refresh in the session menu restarts it as before,
  - `refresh_no_key`: the encode after the request had no key frame,
  - `refresh_kept_failed`: static screen, and the kept image could not be encoded (the driver
    keeps none; before the run encoded its first frame the request waits for that frame, a key
    frame anyway, up to the 1 s timeout),
  - `refresh_key_timeout`: no key frame 1 s after the request.

  Refreshes while a key frame is pending merge into it; one that comes less than 200 ms after a
  key frame went out waits for the 200 ms (`rmerge`), once the encoder took a key frame request in
  this run (before that the refresh asks at once, so an encoder without forced key frames restarts
  without the wait). While a key frame is requested or deferred, the capturer's wait and the
  loop's sleep end at its deadline (kept image, end of the 200 ms), so at a low frame rate the
  kept-image key frame still comes about 100 ms after the request, not after a whole frame
  period. Forced key frame and kept-image encode
  failures never count as encode failures and never turn the hardware encoder off.
- **`nvenc-vbv-frames`** (number, empty or `0` = FFmpeg's default VBV; read when an encoder is
  created). nvenc `rc_buffer_size` = N × bitrate / encoder frame rate, at open and after every
  bitrate change (FFmpeg's nvenc reconfigure re-applies it; patch 0009 keeps that free of an
  IDR). The encoder frame rate is the one NVENC was opened with: 30 on the VRAM and RAM paths
  today, while the loop runs at up to 60, so N = 1 is one NVENC frame budget, about two real
  frames at 60 fps (until the encoder is opened at the real frame rate). `ldkfs` is left at
  NVENC's default (1 with `tune=ull`). Turns static refine on for FFmpeg nvenc encoders: the two
  only go on together (roadmap M14); another encoder (VP9/AV1, AMF, QSV, the NVENC SDK driver)
  gets neither.
- **`enable-static-refine`** (`N` turns static refine and the VBV off together, so
  `nvenc-vbv-frames` is then ignored; any other value changes nothing, refine is on exactly when
  the running encoder got the VBV of `nvenc-vbv-frames` > 0; read when an encoder is created):
  see `refine` above.

Log lines (always, not gated by `enable-latency-log`; hwcodec lines carry `[UTIL]`, `[FFMPEG_VRAM_ENC]` or `[FFMPEG_RAM_ENC]`):

| Line | Meaning |
|---|---|
| `[UTIL] hevc_nvenc open: bit_rate=… fps=… rc_buffer_size=… vbv_frames=… forced_idr=…` | once per nvenc encoder: what FFmpeg settled on after `avcodec_open2`. With `vbv_frames=0` `rc_buffer_size` is FFmpeg's own default VBV (expected 2 × `bit_rate`), with N > 0 the value set. `forced_idr=0` means FFmpeg refused `forced-idr`, or an earlier encoder of that name gave a forced IDR without the key flag in this process; the encoder then reports forced key frames as unsupported (refresh then restarts at once). |
| `forced IDR verified: key frame of N bytes` | once per encoder: the first forced key frame came out key-flagged |
| `forced IDR gave no key frame, forced keyframes off` | a forced key frame came out without the key flag (`refresh_no_key` follows): forced key frames are reported unsupported for encoders of that name until the host process restarts, so this line comes at most once per process and name, and later refreshes restart at once (`refresh_unsupported`) |
| `[FFMPEG] avg bitrate change: a -> b` / `[FFMPEG] vbv buffer size change: a -> b` | with `nvenc-vbv-frames` > 0, FFmpeg's own nvenc reconfigure lines for the first 8 such messages in the process: the proof that a bitrate change re-applies the VBV |
| `static refine on` | the capture loop starts with static refine |
| `refresh: this encoder has no forced key frame, switch` | `refresh_unsupported` |

### Host: capture restarts, connection loop, transport

```
LATENCY switch source=monitor display=0 reason=refresh restarts=1 gap_ms=187 not_logged=0
LATENCY conn id=3 via=TCP-punch busy=0.0/0.4/45.0 busy_max_in=video busy_slow=2 in_gap=… in_slow=4 in_n=812 send=… send_slow=1 frames=298 io=whole ctrlq=n/a qmax_kb=0 adrop=0 bulk_n=0
TRANSPORT host conn=3 type=TCP-punch peer=192.0.2.10:52311 secure=true detail=accepted
```

| Line / field | Meaning |
|---|---|
| `LATENCY switch` | one line per completed capture restart (`gap`), with logging on, at most one per source and display per second. A capture restart is any error return of the capture loop (`run()` in `video_service.rs`), which the service restarts: a `SWITCH` (refresh, codec change, new subscriber, …) or an error. |
| `source` / `display` | `monitor` or `camera`, and the display index. Gaps are kept per source and display. |
| `reason` | why the first restart of the gap happened. For a `SWITCH`: `refresh` (a viewer's `RefreshVideo`; also when a requested refresh was served by a fresh capture start), the `enable-refresh-keyframe` reasons below, `codec`, `portable_service`, `i444`, `gdi_vram`, `screenshot`, `dxgi_access_lost`, `scale` (Android), `privacy_mode`, `new_subscriber`, `encode_fail`, `encoder_need_switch`, `display_changed`, `quality` (encoder that cannot change bitrate), `record`. For any other error: its text, spaces as `_`, at most 60 characters. |
| `restarts` | restarts within this gap (a restart that failed again before sending a frame adds one). `0` with `reason=refresh_keyframe`: the refresh was served by a key frame without a restart. |
| `gap_ms` | the gap: from the viewer's refresh request (`RefreshVideo` for one display, recorded in `refresh_video_display`) or, without one, the restart, until the restarted loop handed its first frame to at least one connection. It includes the service's error back-off sleep and the capturer/encoder re-creation. A refresh request for all displays (login, `PeerInfo`) has no request time: its gap starts at the restart. Gaps older than 60 s when the loop starts are dropped (the session that caused it is most likely gone). |
| `not_logged` | completed gaps of this source and display not logged since the previous line (rate limit); they are still in the `gap` field |
| `LATENCY conn` | every 5 s per authorized connection, with logging on, when the connection wrote video frames or received MouseEvents in the window. It covers the connection's own `tokio::select!` loop in `Connection::start` (`connection.rs`). |
| `id` | the connection id (`conn` of the `TRANSPORT host` line) |
| `via` | the connection's transport, the `type` of its `TRANSPORT host` line |
| `busy` | how long one branch body of the select loop ran, p50/p95/max ms, over all branches. While a body runs the loop polls nothing else: no reading (input), no video write. A video write blocked on a full socket shows here as `video`. p50/p95 cover the first 1024 bodies of the window; the max covers all. |
| `busy_max_in` | the branch of the longest body: `read` (one received message: parse plus `on_message`, e.g. input injection, file operations), `video` (one video frame write), `rx` (other outgoing messages: audio, cursor, clipboard; and the PeerInfo refresh), `cm` (connection manager), `file` (file transfer read jobs), `hbbs`, `authed` (printer), `second` (1 s timer), `test_delay`, `clip`. `-` without any body. |
| `busy_slow` | bodies longer than 16 ms (all of them, not capped) |
| `in_gap` | arrival gap between two MouseEvents at the reader (parsed message to parsed message), p50/p95/max ms. Gaps over 1 s are user pauses and left out. It mixes the viewer's send gaps, the network and host read stalls: a stall the host caused shows as an `in_gap` spike together with a `busy` spike; one with low `busy` came from the network or the viewer (read it with a pktmon capture). p50/p95 cover the first 1024 gaps; the max all of them. |
| `in_slow` | MouseEvent gaps over 16 ms (up to the 1 s limit) |
| `in_n` | MouseEvents received in the window |
| `send` | this connection's video frame writes (`stream.send`), p50/p95/max ms. Unlike `send` of `LATENCY server`, which pools all connections of a display, this is one viewer. p50/p95 cover the first 1024 writes; the max all of them. |
| `send_slow` | this connection's video frame writes over 16 ms |
| `frames` | video frames this connection wrote in the window |
| `io` | `whole`: the select loop writes everything itself (today's path). `split`: the session stream is split (`enable-split-session-io`, see "Host: split session I/O" below). Split, the loop has no `video` branch: `busy` no longer contains video writes, and `send`/`send_slow`/`frames` come from the writer task (same start and end points: serialization, encryption, the write). |
| `ctrlq` | split only: how long control messages (replies, TestDelay, audio, cursor, clipboard, PeerInfo) waited in the writer's control queue before their write started (from entering that queue, not from the rx service channel), p50/p95/max ms. This is the head-of-line wait behind an in-flight frame that remains on the host. `n/a` when whole. |
| `qmax_kb` | split only: the most bytes control and bulk held at once in the window |
| `adrop` | split only: audio frames the writer dropped because they were older than 1 s when it took them (whole: the loop drops them, uncounted) |
| `bulk_n` | split only: bulk writes in the window (CM raw messages, file-clipboard messages, file read-job blocks, rx messages over 256 KB) |
| `TRANSPORT host` | once per incoming connection, when it is admitted (before the identity handshake), whether logging is on or not |
| `conn` | the connection id |
| `type` | `Intranet` (the controller connected to our listener after `FetchLocalAddr`), `TCP-punch` (a TCP hole punch), `Direct` (direct IP access to the listening port), `KCP-UDP` / `KCP-IPv6` (KCP over a punched UDP socket), `WebRTC`, `Relay` (through hbbr) |
| `peer` | the peer address the connection keys on (for `Relay` the controller's address hbbs reported, not hbbr's) |
| `secure` | whether the identity handshake runs the key exchange |
| `detail` | `TCP-punch`: `accepted` (the controller's connect reached our listener) or `dialed` (our own punch connected); `Relay`: the relay server; `-` otherwise |

### Host: TCP meter and SO_SNDBUF (Windows)

Off by default. Server options (`rustdesk.exe --option <key> <value>`), read when a
connection is accepted:

- `enable-tcp-meter`: `Y` meters the session socket of LAN peers, `all` of every peer,
  `N` or empty: off. Covered paths are the ones where the host accepts the session socket
  itself: `Intranet`, `Direct` and `TCP-punch` with `detail=accepted`. Not covered:
  `Relay`, `TCP-punch` `dialed`, KCP, WebRTC (their socket is made inside hbb_common).
- `video-sndbuf-kb`: a number of KB (16..65536) sets `SO_SNDBUF` of the session socket once
  a LAN peer has logged in to remote control or camera view. Never for file transfer,
  terminal or port forward, never for non-LAN peers. Empty (default) keeps Windows' dynamic
  send buffering. Setting it turns dynamic send buffering off for that socket; it is an
  experiment (A/B 256 and 512 against empty).

With either option set, the socket gets a pass-through wrapper (`HostTcpStream`): reads go
straight through; a write is counted only after it returned `Ready(Ok(n))`; `SIO_TCP_INFO`
(version 1, falling back to 0) is queried after a write at most every 100 ms, after a write
that returned Pending at most every 10 ms, and once per report window. The first query, when
the wrapper is made, is the baseline for `BytesOut`/`BytesRetrans`. Any query error turns
sampling off for that socket and never touches the connection. The wrapper marks the socket
closed before it closes, and nothing queries a closed socket. Sampling runs only while
`enable-latency-log` is `Y`. Both options off: the socket is exactly what it was (plain, or
qWAVE-marked).

```
TCPMETER host conn=3 meter=yes ver=1 lan=true
TCPMETER host conn=4 meter=no why=path via=Relay
TCPMETER host conn=3 sndbuf_kb=256 was_kb=64 now_kb=256
LATENCY tcp conn=3 via=Intranet ver=1 n=61 pend=4 app_kb=10240 out_kb=10260 rtt=0.9/2.1/31.0 minrtt=0.4 unsent_max_kb=310 unsent_last_kb=0 inflight_max_kb=180 cwnd_kb=640 sndwnd_kb=2048 retrans_kb=0 fastre=0 rto=0 dupack=0 lim_rwin_ms=0 lim_cwnd_ms=12 lim_snd_ms=4980
```

| Line / field | Meaning |
|---|---|
| `TCPMETER host` | once per connection when either option is set, whether logging is on or not: whether this connection has a meter. `why`: `option_off`, `not_lan` (`enable-tcp-meter=Y` and a non-LAN peer), `path` (not an accepted path, see above), `closed`. `meter=yes` with `ver=-` cannot happen; `meter=no` with a socket handle means `SIO_TCP_INFO` failed at the baseline. The `sndbuf_kb` form is logged when `video-sndbuf-kb` was applied: the value asked for and what Windows reported before and after. |
| `LATENCY tcp` | every 5 s per metered, authorized connection, with logging on, when the window had a sample or a write |
| `ver` | `SIO_TCP_INFO` version in use: 1, or 0 (no `lim_*` fields, `n/a`) |
| `n` | samples in the window (including the one taken when the window is reported, so a write blocked for the whole window still gives one) |
| `pend` | writes into the socket that returned Pending: the send buffer was full |
| `app_kb` | bytes the session wrote into the socket in the window |
| `out_kb` | `BytesOut` delta in the window, as Windows counts it. Over a long idle-ended window it should be about `app_kb` + `retrans_kb`. On loopback it is not: 430683 bytes out for 300000 written with no retransmits (test, Windows 11 26300), so `unsent_*` below must be checked on the real link first. |
| `rtt` | `RttUs` over the samples, p50/p95/max ms |
| `minrtt` | `MinRttUs` of the last sample, ms |
| `unsent_max_kb` / `unsent_last_kb` | bytes written by the session that TCP has not sent yet: written − (`BytesOut` − `BytesRetrans`), both from the baseline. Max over the samples and the last sample. This is what `SO_SNDBUF` can cap. Idle, `unsent_last_kb` must be about 0; a value well below 0 means `BytesOut` does not count what the formula assumes. **Unverified**: on loopback it is far below 0 (see `out_kb`); confirm on the LAN/Wi-Fi link (idle window → about 0) before deciding `video-sndbuf-kb` from it. |
| `inflight_max_kb` | `BytesInFlight` max: sent, not acknowledged |
| `cwnd_kb` / `sndwnd_kb` | congestion window and the peer's advertised receive window, last sample |
| `retrans_kb` / `fastre` / `rto` / `dupack` | in the window: retransmitted bytes, fast retransmits, RTO episodes (`TimeoutEpisodes`), duplicate ACKs received |
| `lim_rwin_ms` / `lim_cwnd_ms` / `lim_snd_ms` | v1, in the window: ms the sender was limited by the peer's receive window (the viewer does not read: an overloaded Dell), by the congestion window, by the sender itself (nothing to send) |

Classifying a `send` spike (`LATENCY conn` `send` max / `send_slow` in the same window):
`rto` ≥ 1: TCP timeout; `fastre` > 0 with `rto` 0: loss recovered by duplicate ACKs;
`lim_rwin_ms` large: the viewer stopped reading; none of these with a large `send` max and
`pend` > 0: a pause of the link itself (Wi-Fi airtime), confirm once with `pktmon`.
`SO_SNDBUF` only helps when `unsent_max_kb` during spikes is well above the cap.

Rollback: `enable-tcp-meter N`, `video-sndbuf-kb` empty.

### Host: split session I/O

Off by default: server option `enable-split-session-io` `Y` (Windows hosts only; read when
a connection is authorized). An authorized remote-control or camera-view session over TCP
(intranet, direct, punch, relay) or KCP is split once it is authorized: the read half stays
in the connection's select loop, and one writer task owns the write half. Input is read
while a video write waits on a full socket. The wire does not change: same framing, same
encryption and nonce counters (the writer alone encrypts, the reader alone decrypts), same
messages, each source in order. WebSocket and WebRTC sessions, file transfer, terminal and
port forward always stay whole.

The writer takes, in this order: the control queue (every reply, the whole rx service channel:
audio, cursor, clipboard, PeerInfo; TestDelay; the close reason), the video channel (frames and
SwitchDisplay, "fetched" at dequeue as before, unless the web client acks frames), then the
bulk queue (CM raw messages, file-clipboard messages, file read-job blocks, rx messages over
256 KB; the rx messages after one follow it to bulk while it is queued, so rx order holds). Bulk gets a write at least after every 4 video writes. Audio older than 1 s when the
writer takes it is dropped (from control or bulk). File reads pause while bulk holds 512 KB, and the reader pauses
while control and bulk hold 16 MB (the backpressure the inline write gave); the writer wakes
the loop when it takes something off. The send timeout is the writer's. When the writer ends
for any reason (write error, send timeout, panic) the session closes, and the capture loop is
told not to wait for this connection's fetch. A writer with work and no progress for the
send timeout + 5 s is closed by a watchdog. A close reason is on the wire before `on_close`
(flush, at most twice the send timeout: the frame in flight, then the reason; it stops early
when the writer ends), and at the end the writer drains control (1.5 s, then aborted).

```
SPLIT host conn=5 via=Intranet carried=1
SPLIT host conn=5 stays whole: transport
SPLIT host conn=5 ended lived=812.4s reason=Peer close failure=false aborted=false counted=false
SPLIT host: 2 split sessions ended early, enable-split-session-io is off until the service restarts
```

| Line / field | Meaning |
|---|---|
| `carried` | complete messages that were already in the read buffer at the split; they are delivered first |
| `stays whole` | why a session that should split did not: `transport` (WebSocket, WebRTC), `unflushed write` |
| `ended` | `lived`: time since the split; `reason`: the first close reason; `failure`: writer panic or watchdog; `aborted`: the writer did not drain in 1.5 s; `counted`: held against split |

Sticky fallback: a split session counts when it ends in under 60 s for any reason other than
the user or the CM closing it (`Peer close`, `switch sides`, `connection manager`,
`web console`, `auto disconnect`, `stop service`, or an empty reason: the viewer picked another
Windows session), and always on a writer panic or a watchdog trip. Two counts
turn split off until the service restarts; a split session that lasted over 5 minutes clears
the count. The way back without a working remote session: a terminal or file-transfer session
(never split) running `rustdesk.exe --option enable-split-session-io N`.

What changes in the numbers when split: `wait` (`LATENCY server`) no longer contains the loop
being busy in other branches, only the writer's queue; `busy` has no `video`; `in_gap` spikes
during a `send` spike should disappear when the stall is one-way (host to viewer).
With enable-refresh-keyframe or static refine on, `wait` still counts captured frames only
(kept-image and refine key frames add no sample), in both modes. The writer never drops video,
so a forced refresh key frame always reaches the socket; its `gap` (`refresh_keyframe`) ends
when the capture loop hands the frame to the connections, before the writer's queue, as the
whole stream's gap ends before the loop's video branch.

### UDP video lane (shadow probe)

Roadmap M19, design and negotiation in docs/UDP-VIDEO.md. Off by default: host server option
`enable-udp-video` `shadow` (M19 used `Y`, which is live mode since M20, see the next section);
the viewer option `enable-udp-video` is on by default and only acts when a host offers. The
video still goes over TCP; every Nth frame (`udp-video-shadow-every`, default 2) is also sent
over UDP, and the viewer only measures it. These lines are logged whenever a lane exists,
independent of `enable-latency-log` (the lane exists only to measure). Since M20 the window
lines carry `mode=shadow` after the lane id.

```
UDPVIDEO host conn=5 not offered (viewer v1): host option enable-udp-video is not Y
UDPVIDEO host conn=5 offer lane=3f2a… mode=shadow addr=192.168.1.5:61234 every=2 viewer=192.168.1.9 installed=true
UDPVIDEO host conn=5 HELLO from 192.168.1.9:50511 lane=3f2a… at_us=41250
UDPVIDEO host conn=5 active lane=3f2a… mode=shadow peer=192.168.1.9:50511 every=2 after_ms=47
UDPVIDEO host conn=5 lane=3f2a… win_s=5.0 frames=150 keys=1 skipped=150 qdrop=0 dgrams=2210 kB=2950 send_ms=0.2/0.6/2.1 lag_ms=0.1/0.3/1.2 err=0 pings=25 refused=0
UDPVIDEO host conn=5 stop lane=3f2a…: no HELLO from the viewer in 3 s: inbound UDP to this host looks blocked (…)
UDPVIDEO host conn=5 closed lane=3f2a… state=Active
UDPVIDEO viewer peer=123456789 accepted lane=3f2a… mode=shadow host=192.168.1.5:61234 local=192.168.1.9:50511 every=2
UDPVIDEO viewer peer=123456789 ready lane=3f2a… after_ms=2 hellos=1
UDPVIDEO viewer peer=123456789 lane=3f2a… win_s=5.0 frames=150 lost=0 frag_miss=0 dgrams=2235/2235 loss_pct=0.00 reorder=0/0 dup=0 late=0 bad=0 drop=0/0/0/0 matched=150 tcp_first=3 unpaired=0 ahead_net_ms=1.8/6.5/24.0/-0.4 ahead_app_ms=1.1/5.9/22.7/-1.0 owd_var_ms=0.4/2.2/9.8 asm_ms=0.3/1.1/3.0 rtt_ms=2.1/4.0/7.5 pings=25/25 rx_err=0
UDPVIDEO viewer peer=123456789 refused lane=3f2a…: session is not direct encrypted TCP
UDPVIDEO viewer peer=123456789 handshake failed lane=3f2a…: no HELLO_ACK in 1000 ms after 50 HELLO (…)
UDPVIDEO viewer peer=123456789 closed lane=3f2a…: host stopped: …
```

Host window line (sender thread, every 5 s while the lane is active, and once when it ends):

| Field | Meaning |
|---|---|
| `win_s` | length of the window |
| `frames`, `keys` | shadow copies sent, of them containing a key frame |
| `skipped` | video frames the every-Nth rule did not copy |
| `qdrop` | copies dropped because the sender's queue (4) was full: the sender thread fell behind |
| `dgrams`, `kB` | datagrams and kB the socket took (UDP payload, headers not counted) |
| `send_ms` | p50/p95/max (nearest rank) of one copy's serialize + seal + `send` of all its datagrams |
| `lag_ms` | p50/p95/max from the tap to the sender thread taking the copy |
| `err` | failed serializations, fragmentations and `send` calls; `first_err` is the first one |
| `pings` | viewer pings answered; `refused`: datagrams dropped before or by the crypto (other source, bad tag, replay) |

Viewer window line (every 5 s from the receive thread's window, combined in the io_loop with
the pairing of the same 5 s):

| Field | Meaning |
|---|---|
| `frames` | shadow frames completed |
| `lost`, `frag_miss` | frames declared lost (a later frame completed 5 ms earlier, or 500 ms since its first fragment: shadow frames are unpaced and share the link with the TCP copy, so a slow but complete frame is delivered and paired, not lost), and their missing fragments |
| `dgrams` | datagrams received / expected from the lane seq span (pongs and HELLO_ACK share the seq, so a lost pong counts too; the host sends each datagram under the seal lock, so a pong never overtakes the frame it interrupts) |
| `loss_pct` | `1 - received/expected`, in % |
| `reorder` | datagrams that arrived after a higher seq / largest seq distance |
| `dup`, `late`, `bad` | duplicate fragments, fragments of a frame already complete or lost, inconsistent fragments |
| `drop` | datagrams refused: malformed / replayed / failed authentication / bad inner part |
| `matched` | frames whose UDP and TCP copies met (by display and pts) |
| `tcp_first` | of those, how many times TCP handled its copy before UDP completed its own |
| `unpaired` | UDP copies that found no TCP copy within 2 s |
| `ahead_net_ms` | p50/p95/max/min of t(TCP copy returned by `peer.next()` in the io_loop, before its parse) - t(UDP copy complete on the receive thread). Positive: UDP first. Upper bound: it credits UDP with the io_loop's scheduling delay |
| `ahead_app_ms` | the same against the entry of the io_loop's UDP branch for the copy: both copies pay the loop's delay and neither stamp includes a parse (the UDP copy is parsed for display and pts on the receive thread, so the io_loop does no full-frame parse for it). The number the M20 decision uses |
| `owd_var_ms` | p50/p95/max of the one-way delay above the window's minimum: arrival of a frame's first fragment on the viewer's lane clock minus `send_us`, which the host stamps after serializing the frame, right before it seals and sends the first datagram (host lane clock; the clock offset cancels). One datagram's delay, independent of frame size; the burst of a big frame shows in `asm_ms` |
| `asm_ms` | p50/p95/max first to last fragment of a frame |
| `rtt_ms` | p50/p95/max of PING to PONG over the lane (200 ms interval) |
| `pings` | sent / answered |
| `rx_err` | receive errors other than the timeout |

Reading them: `loss_pct`/`lost` near 0 and `ahead_app_ms` p95/max near 0 mean TCP loses
nothing to retransmission on this link, and M20-M22 can wait. `ahead_app_ms` max spikes at the
moments `LATENCY tcp` shows retransmissions (or `wait`/`send` spikes on the host) mean UDP would
have delivered those frames earlier. Always note `every`: the copy itself loads the link.

### UDP video lane (live, M20)

Host server option `enable-udp-video` `Y` (off by default) and a viewer of lane v2: video frames
go over UDP, everything else stays on TCP; any trouble falls back to TCP video in the same
session (docs/UDP-VIDEO.md, "Live mode"). Logged whenever a lane exists, like the shadow lines.

```
UDPVIDEO host conn=5 offer lane=3f2a… mode=live addr=192.168.1.5:61234 every=2 burst=8 timeout_ms=1000 viewer=192.168.1.9 installed=true
UDPVIDEO host conn=5 live: a loss is repaired by a forced key frame (enable-refresh-keyframe=Y)
UDPVIDEO host conn=5 lane=3f2a… pacing burst=8 hires_timer=true
UDPVIDEO host conn=5 qWAVE AudioVideo flow added
UDPVIDEO host conn=5 active lane=3f2a… mode=live peer=192.168.1.9:50511 every=2 after_ms=41
UDPVIDEO host conn=5 lane=3f2a… mode=live win_s=5.0 frames=300 keys=1 skipped=0 qdrop=0 dgrams=4410 kB=5900 send_ms=0.9/3.1/8.0 lag_ms=0.1/0.2/0.6 err=0 pings=25 refused=0 paced_ms=0.4/2.6/7.6 win_wait_ms=2.0/9.5/31.0 win_to=0 inflight_max=4 acks=410 ack_rtt_ms=2.2/4.9/12.1 base_ms=1.6 vlost=0 kf_req=0
UDPVIDEO host conn=5 stop lane=3f2a… mode=live: sustained loss: 4 key frame requests in 5 s (…) (in_band=true displays=[0])
UDPVIDEO host conn=5 video back on TCP, next offer in 30 s
UDPVIDEO host conn=5 retry 1 of 2
UDPVIDEO viewer peer=123456789 accepted lane=3f2a… mode=live host=192.168.1.5:61234 local=192.168.1.9:50511 every=2 timeout_ms=1000
UDPVIDEO viewer peer=123456789 live lane=3f2a… epoch=0 held=1 closed=false
UDPVIDEO viewer peer=123456789 lane=3f2a… mode=live win_s=5.0 frames=300 lost=0 gated=0 frag_miss=0 dgrams=4535/4535 loss_pct=0.00 reorder=0/0 dup=0 late=0 bad=0 drop=0/0/0/0 shown=300 held=1/1 stale=0 gated_app=0 kf_req=0 tcp_drop=0 owd_var_ms=0.4/2.0/8.1 asm_ms=0.6/2.9/7.9 rtt_ms=2.0/3.6/6.6 pings=25/25 acks=410 rx_err=0
UDPVIDEO viewer peer=123456789 request stop lane=3f2a…: display 0 without a key frame 1004 ms after a loss
UDPVIDEO viewer peer=123456789 live lane=3f2a… back on TCP: key frame awaited on displays [0]
```

Host: `offer` adds `burst` (`udp-video-burst`, datagrams per paced burst, 0 = unpaced) and
`timeout_ms` (`udp-video-timeout-ms`); the `live:` line says how a loss is repaired (M13 forced
key frame, or a capture restart for every viewer when `enable-refresh-keyframe` is not `Y`);
`pacing` says whether the high-resolution waitable timer is in use (`false`: Windows before 10
1803, waits are slept and spun); the qWAVE line appears unless `enable-lan-qos` is `N` (a
failure is logged as a warning, the lane goes on unmarked). The window line has the shadow
fields (`frames`: frames sent over the lane; `qdrop`: frames dropped from a full queue of 8,
each a loss on the viewer; `send_ms`: as `send` above) plus:

| Field | Meaning |
|---|---|
| `paced_ms` | p50/p95/max of the time one frame waited for the pacer (part of `send_ms`) |
| `win_wait_ms` | p50/p95/max, frames the send window held: from sent until reported fetched |
| `win_to` | held frames released by the 100 ms limit instead of an ACK |
| `inflight_max` | most frames of one display sent and not yet settled by the viewer |
| `acks` | viewer ACK datagrams (each display on progress, and every 50 ms) |
| `ack_rtt_ms` | p50/p95/max of the ACK round trip: host time at the ACK minus the acked frame's `send_us`, minus the viewer's hold from that frame's first fragment to the ACK. The first fragment's trip plus the ACK's |
| `base_ms` | the QoS baseline: the lowest ACK round trip of this and the previous 10 s window; QoS gets the excess over it (`qos_sig=udp`) |
| `vlost` | frames the viewer declared lost (from the ACKs' running count) |
| `kf_req` | key frame requests (`UdpVideoFeedback`) the viewer sent over TCP; 4 within 5 s end the lane (sustained loss) |

The `stop` line gives the reason (handshake timeout, `no datagram from the viewer in N ms`,
`N datagram sends failed in a row`, `sustained loss`, `viewer asked: …`), `in_band` (the STOP
marker waits for the next video item, so it comes right before the first TCP frame) and the
displays that get a key frame for their restart on TCP. After a live lane ends the host offers
again after 30 s and 120 s, then stays on TCP.

Viewer window line, live (`mode=live`): the receive-side fields as in the shadow line (`lost`
here: 50 ms after a frame's first fragment, or 5 ms after a later frame completed), plus:

| Field | Meaning |
|---|---|
| `gated` | complete frames the receive thread dropped after a loss because they were not key frames (their reference is missing) |
| `shown` | frames handed to the decoder path (decoder queue, pacer, fps control, latency stats, as TCP frames are) |
| `held` | frames held for the ACTIVE marker or a SwitchDisplay of their epoch / most held at once |
| `stale` | frames of an epoch before a SwitchDisplay already read, dropped |
| `gated_app` | frames the io_loop dropped while the display waited for a key frame (a held frame lost to the 32-frame hold limit, or a loss it had already seen) |
| `kf_req` | key frame requests sent (at most one per display per 100 ms) |
| `tcp_drop` | TCP frames dropped after the lane ended until a key frame, for displays whose last UDP frames did not all arrive |
| `acks` | ACK datagrams sent |

The viewer asks the host to stop the lane (`request stop`) when a display has no key frame 1 s
after a loss, frames wait 1 s for ACTIVE, the host is silent for `timeout_ms`, or 5 receive
errors come in a row. With a live lane the viewer's `LATENCY client` fields keep their meaning:
UDP frames enter the same handling as TCP frames; the session `kB/s` counts both.

### Viewer (Rust)

```
LATENCY client display=0 codec=H265 hw=false pacing=on fps=29.9 net+server(est)=… queue=… decode=… to_ui=… ui_take=… total_to_ui(est)=… offset_err<=1.5ms queue_len=2 ui_dropped=3 ui_replaced=0 rtt_min=3ms rx_fps=30.0 dec_fps=29.9 shown_fps=29.9 skip=0 decode_skip=n/a decode_skip_conv=n/a noout=0 shown_gap=… discard=0 dec_fail=0 via=TCP
```

| Field | Meaning |
|---|---|
| `codec` | negotiated codec of the decoded frames |
| `hw` | `true` only when the frame was decoded to a GPU texture (VRAM decoder, Windows). `false` covers both software decoding and the FFmpeg "hwram" decoders, which can be hardware on desktop. On the Quest build (no `mediacodec` feature) `false` means software. The decoder actually created is in the log ("create … decoder"). |
| `pacing` | present pacing (`enable-viewer-present-pacing`) of this display: `on` / `off` |
| `fps` | decoded frames per second, shown or skipped |
| `net+server(est)` | host loop tick (`pts`) → message parsed in `io_loop` on the viewer. Includes host grab, encode, host queueing, socket buffers, network, retransmits. See "Clock estimate". |
| `queue` | parsed in `io_loop` → taken by the decoder thread (key frames: the decoder channel; other frames: the 120-deep `ArrayQueue`). Shown frames only (those handed to the UI callback); `net+server(est)` covers skipped frames too. |
| `decode` | `VideoHandler::handle_frame`: decode plus YUV→RGBA conversion. Shown frames only; the frames present pacing skipped are in `decode_skip`, so `decode` (and the overlay's `dec`) stays comparable with pacing off. |
| `to_ui` | decode done → the UI callback returned. Soft render (Android/Quest): buffer swap and posting `EventToUI::Rgba`. Texture render: copy into the texture. |
| `ui_take` | soft render only: buffer marked ready for Flutter (just before `EventToUI::Rgba` is posted) → Flutter released it (`next_rgba`). Covers the wait in the Dart UI isolate's event queue, `getRgbaSize`/`getRgba`, and `decodeImageFromPixels` + `update` in `ImageModel.onRgba` (the Flutter line's `image` part). `ui_take − image` ≈ the port-queue wait, which grows when the UI isolate is busy. Measured on one clock (Rust), sampled per frame Flutter took; frames counted in `ui_dropped` have none. It overlaps `to_ui` only by the post call itself (microseconds). `n/a` on the texture-render path. |
| `total_to_ui(est)` | `net+server(est)` + receive → `to_ui` done, per frame (does not include `ui_take`) |
| `offset_err<=` | the clock estimate's network error bound, `rtt_min/2` |
| `queue_len` | the largest video queue backlog seen after a decode in the window |
| `ui_dropped` | decoded frames the soft renderer threw away because Flutter had not taken the previous one yet. They have `queue` and `decode` samples but no `to_ui`/`total`. With present pacing on, only frames that came after Flutter started reading the pending buffer (or of a different size) are dropped; the others replace it (`ui_replaced`). |
| `ui_replaced` | soft render with present pacing: frames that replaced a pending buffer Flutter had not started reading (neither `session_get_rgba_size` nor `get_rgba` called yet), so Flutter shows the newer frame. Counted with the replacing frame, which has normal `to_ui`/`total` samples and whose handoff starts `ui_take`; the replaced frame keeps the samples it got when it was handed over, but is never shown. |
| `rtt_min` | smallest non-zero round trip the host reported in its last 10 TestDelay probes (`last_delay`) |
| `rx_fps` | frames parsed in `io_loop` per second (from the always-on `VideoCounters`, summed over the 1 s status ticks of the window; `n/a` before the first tick) |
| `dec_fps` | frames `handle_frame` decoded to a picture per second |
| `shown_fps` | decoded frames that reached the screen per second: handed to the UI callback, minus soft-render drops (`ui_dropped`) and frames a later one replaced before Flutter fetched them (`ui_replaced`). The overlay's `shown` is the same. |
| `skip` | decoded frames not handed to the UI callback (`decoded` − handed), count in the window: the frames present pacing skipped. Always 0 with pacing off. Soft-render drops and replacements are not in it (they are left out of `shown_fps` instead). |
| `decode_skip` | `decode` of the skipped frames that were decoded without output (`DecodeOutput::DecodedNoOutput`, see "Decode without output" below), p50/p95/max ms, `n/a` when there was none. `decode_skip` p50 against `decode` p50 is what a skipped frame saves. |
| `decode_skip_conv` | `decode` of the skipped frames that were converted anyway, p50/p95/max ms, `n/a` when there was none: a frame that turned stale only during its decode, every skipped frame with `enable-viewer-decode-no-output` off, and every skipped frame of a decoder that always outputs (AMF/MFX GPU decoders). Kept apart so `decode_skip` stays a pure no-output time. |
| `noout` | of the decoded frames, those decoded without output (`DecodeOutput::DecodedNoOutput`), count in the window. Equal to `skip` when every skipped frame was decided before its decode; lower when a frame turned stale only during its decode (the callback is skipped then, the conversion was already done) or the decoder always outputs (AMF/MFX GPU decoders) |
| `shown_gap` | time between two consecutive frames that reached the screen (`to_ui` done → next `to_ui` done; frames counted in `ui_dropped`, and frames a later one replaced (`ui_replaced`), are left out: the gap runs from the frame shown before the replaced one to the replacing one), p50/p95/max ms. A gap is added once the next frame did not replace its frame, so the newest one lands in the next window. Gaps over 1 s (static screen) are left out. Its p95 shows stutter: with pacing on it must not grow in calm use. |
| `discard` | received frames dropped undecoded, count in the window: `discard_queue` after a refresh request, or evicted by a full 120-deep queue |
| `dec_fail` | `handle_frame` calls that returned no picture or an error (a frame decoded without output is not one) |
| `via` | the session's transport, the `type` of the `TRANSPORT viewer` line (`TCP`, `UDP`, `IPv6`, `WebRTC`, `WebRTC/IPv6`, `Relay`, `WebSocket`) |

The `rx_fps` / `dec_fps` / `shown_fps` / `skip` / `discard` / `dec_fail` counters come
from `VideoCounters` (`src/client.rs`), which count always, whether or not latency is
measured; the line only reports them. `rx_fps > dec_fps` with a growing `queue_len` means
the decoder cannot keep up; `rx_fps ≈ dec_fps` with `queue` spikes means frames arrived in
bursts (network).

### Viewer: decoder, transport, races

Logged once per event, not tied to the latency option:

```
LATENCY client decoder display=0 codec=H265 path=vram/FFMPEG hw=true use_texture_render=true
LATENCY client pacing display=0 present_pacing=on
LATENCY client priority thread=io_loop prio=above_normal
LATENCY client priority process=above_normal ecoqos=opt_out
LATENCY client priority thread=decoder display=0 prio=above_normal
LATENCY client mmcss display=0 task=Playback state=on
LATENCY client priority process=restored
LATENCY client audio_policy=adaptive floor_ms=60
audio trim 412ms to target 60ms, generation=3
TRANSPORT viewer peer=123456789 type=TCP direct=true secured=true kcp=false rendezvous=rs.example.com:21116 connect_ms=412
RACE site=connect tcp=ok@180 udp=err@3010 v6=pending webrtc=pending winner=TCP ms=180
RACE site=start rule=relay_hold preferred=ok@640 tcp_punch_round=ok@95 winner=UDP ms=640
```

| Line / field | Meaning |
|---|---|
| `decoder` | every decoder creation (`VideoHandler::new` / `reset`, e.g. a codec switch) |
| `pacing` | once per video thread (display) start: whether present pacing is on for it (the option is read then) |
| `priority thread=` | viewer priority boost (`enable-viewer-priority-boost`, Windows), once per `io_loop` round of a remote-control or camera session and once per decoder thread (`display=`) start. `prio`: `above_normal` (raised to THREAD_PRIORITY_ABOVE_NORMAL, restored when the round or thread ends), `off` (option `N`), `kept:<n>` (the thread already ran at that priority or higher, unchanged), `failed (<error>)` |
| `priority process=` | the counted process boost: logged when the first such session raises it and when the last one ends (`restored`, or `kept_server` when the in-process server's host priority boost holds the class). `process`: `above_normal` (raised from NORMAL or lower), `kept:<class>` (the class, hex, was already higher: unchanged and not restored), `failed(<error>)`. `ecoqos`: `opt_out` (power throttling of execution speed off; the earlier state is put back on release), `failed(<error>)` |
| `mmcss` | MMCSS "Playback" on a display's decoder thread (`enable-viewer-mmcss`, Windows): `state=on` when the decoder outputs GPU textures (vram) and the registration succeeded, `failed` (a `MMCSS Playback registration failed` warning has the error), `reverted` when the decoder stops being a GPU one (codec switch to software or RAM decoding), `off` once at the thread start with option `N`. Nothing is logged while the decoder never was a GPU one. |
| `audio_policy` | once per audio format of a session (`AudioBuffer` creation): `adaptive` (the `audio-target-ms` target, `floor_ms` its lower bound) or `legacy` (`try_shrink`) |
| `audio trim` | each trim of the adaptive policy: how much buffered audio was dropped, the target it trimmed to, and the playback discontinuity generation (crossfade). Not tied to the latency option; at most one every 2 s. The legacy rule logs `skip <samples>, based …` instead. |
| `path` | `vram/<driver>` (hwcodec GPU decoder, D3D11 texture output; driver `FFMPEG`, `AMF`, `MFX` or `NV`), `hwram/<FFmpeg decoder>/<hw device>` (FFmpeg decoder with CPU output, converted to RGBA), `vpx`, `aom` (software), `other` (MediaCodec), `none` (creation failed) |
| `hw` | `true` for the `vram` path: frames reach the UI as GPU textures (the `hw` of the LATENCY line) |
| `use_texture_render` | the viewer's texture-render setting. With `vram` and this off nothing is displayed. On desktop the RGBA path uses the texture renderer when this is on or the session shows more than one display, otherwise the soft renderer (`ui_take`, `ui_dropped`). |
| `TRANSPORT viewer` | once per connection round, right after the connection is established: `type` the transport label, `direct` whether it is peer to peer, `secured` the encrypted handshake, `kcp` KCP over UDP, `rendezvous` the rendezvous server used, `connect_ms` from the start of `Client::start` (rendezvous, punching, race, secure handshake) |
| `RACE` | one line per connection race. `site`: `start` (the preferred punch request against the TCP-punch-only request, `preferred` / `tcp_punch_round`), `connect` (the direct attempts after a punch: `tcp`, `udp`, `v6`, `webrtc`, then `relay` when the relay was requested afterwards), `relay_response` (`v6`, `relay`, `webrtc` after a relay answer). Each candidate: `ok@ms` / `err@ms`, milliseconds since the race started, or `pending` when the race ended before it finished (it was dropped). `winner` is the transport label of the result, `none` if all failed; `ms` the race's total time. `rule` (sites `start` and `relay_response` only): which race decided. `first`: the first success wins (the stock rule; at `start` with `enable-relay-hold` = `N`, at `relay_response` without a v6 attempt or with relay-only WebRTC). `relay_hold`: a relay result is held for `relay-fallback-delay` (default 2.5 s) while a direct attempt still runs (M8 2a; with WebRTC as before, now also without it). `prefer_udp` / `prefer_tcp`: `direct-transport-preference` is set (M8 2b); a direct result of the other kind is also held, for 300 ms, while the preferred kind still runs. A held candidate that loses shows as `ok@ms` before the winner's time. |

### Viewer: fps control

```
FPSCTL tick_ms=1001 direct=true custom=60 last_auto=54 limited=54 min_decode_fps=60 max_queue_len=0 decision=keep sent=- | d0 rx=60 dec=60 shown=60 skip=0 discard=0 fail=0 queue_len=0 queue_max=1 decode_fps=62 dec_ms=8.1 present_ms=4.2 other_ms=3.8 idle=4 inactive=0 trend=- net+server=3.1/9.0;
Set fps to 27 (reason=decrease last_auto=Some(54) custom=60 limited=27 min_decode_fps=30 max_queue_len=3 direct=true | d0 …;)
```

`FPSCTL` is logged every 1 s status tick while `enable-latency-log` = `Y` (not for the
quality monitor overlay alone). The `Set fps to N` line (always logged, as before) now
carries the same inputs after the number. These two lines describe the legacy
controller (`io_loop.rs` `fps_control`, unchanged), which runs when
`enable-gentle-fps-control` is off (see "Gentle fps control" below for the lines of
the default controller):

| Field | Meaning |
|---|---|
| `tick_ms` | length of this status tick |
| `custom` | the user's custom fps (30 when unset or outside 5..120) |
| `last_auto` | the fps last requested from the host (`-`: none yet) |
| `limited` | `min_decode_fps` × 0.9 (direct) or × 0.8 (relay), capped at `custom` |
| `min_decode_fps` | the smallest `decode_fps` over displays with frames in the last 15 ticks |
| `max_queue_len` | the largest video queue length now, over all displays |
| `decision` | `keep` (no trend), `init` (first request), `increase`, `decrease` (request `limited`), `halve` (decrease while `limited < max_queue_len`: request `limited/2`, floor 1), or `none` before any `decode_fps` exists |
| `sent` | the fps sent to the host this tick, `-` if unchanged |
| `d<N> rx/dec/shown/skip/discard/fail` | this display's `VideoCounters` for the tick (see the LATENCY line). `shown` here is every frame handed to the UI callback, `skip` = `dec` − `shown` |
| `noout` | of `dec`, the frames decoded without output (see the LATENCY line) |
| `ui_lost` | of the `shown` frames, those the soft renderer dropped or that a later frame replaced before Flutter fetched them (`ui_dropped` + `ui_replaced`): they never reached the screen. The gentle controller and `shown_fps` use `shown` − `ui_lost`. |
| `queue_len` | `video_queue.len()` as the trend check saw it (the controller's input) |
| `queue_max` | the largest queue length right after a decode during the tick |
| `decode_fps` | the legacy controller's input: frames / (dequeue → after the UI callback and latency hook), averaged over up to 30 frames, first 3 frames after a codec change skipped. Frames skipped by present pacing count too, with their time up to the skip hook (no UI callback), so under backlog `decode_fps` shows how fast the backlog drains. |
| `dec_ms` / `present_ms` / `other_ms` | the split of that time per frame over the tick: `handle_frame`, the UI callback, and the rest (queue pop, latency hook) |
| `idle` / `inactive` | the controller's counters: ticks with a short queue, ticks without shown frames |
| `trend` | `up` / `down` / `-` (none) as the trend check returned it, `?` if it did not run for this display |
| `net+server` | `net+server(est)` of the last frame / p95 of the current window in ms, `?` while not measured |

A refresh request from the controller logs `Refresh display N to reduce delay
(queue_len=… tolerable=… refresh_times=…)`.

#### Gentle fps control

`io_loop.rs` `GentleFpsControl`, local option `enable-gentle-fps-control`: on by
default on Windows (`N` brings back the legacy controller above), off by default on
other platforms (`Y` turns it on). Read when a connection starts. It sends the same
message as the legacy controller (`Misc` `OptionMessage{custom_fps}`), with values in
10..custom, so a stock host accepts it; no wire change. It works from the decoder
thread's load, not from the queue peak, which Wi-Fi bursts make large:

- **Overloaded tick** (a display with frames to tell, at least 2 decoded): the
  decoder thread was busy (`busy_us`, dequeue to after the UI callback) more than
  90 % of the tick, or fewer frames left the queue (decoded + failed + discarded)
  than arrived for 3 ticks in a row while the median queue length (`queue_p50`, not
  the maximum) did not shrink and ended higher than before the streak.
- **Decrease:** 2 overloaded ticks in a row and at least 2 s since the last change:
  × 0.85, not below the floor `min(custom, 24)`. Below it, down to `min(custom, 10)`,
  only once the capacity has been under the floor for 10 ticks in a row, and then
  not below 0.9 × capacity.
- **Capacity** (`cap`): steady frames (not key frames, not the first 3 after a
  decoder start, reset or codec change) per busy second, an EWMA (α 0.3) over ticks
  with at least 5 of them. The minimum over displays.
- **Lasting present overload:** frames arrive (received ≥ half the current fps) but
  shown < 0.7 × min(current fps, received) for 10 ticks in a row (frames that present
  pacing skips): request `max(floor, 1.2 × the average shown fps)`. A shorter present
  overload requests nothing; the pacer absorbs it.
- **Increase:** 5 calm ticks in a row and at least 3 s since the last change, while
  below the custom fps: `+ max(3, 10 %)`, at most custom. Calm: no overload, every
  display that decoded at least half the current fps was busy < 75 %, had a median
  queue ≤ 1, showed ≥ 90 % of what it received and has capacity ≥ 1.2 × the current
  fps, and there is at least one such display.
- **Start-up:** nothing is sent before 3 ticks and 30 steady frames on a display (the
  host starts at the custom fps sent at login). A display without decoded frames for
  15 ticks is ignored, and one with few frames (static screen) neither raises nor
  lowers anything.
- **Custom fps changed by the user:** the host was told directly, so the controller
  starts again from it.
- **Refresh** (restarts the host's capturer and encoder for every viewer) only as a
  last resort: a display's `queue_max` above `max(30, VIDEO_QUEUE_SIZE / 4)` for 3
  ticks in a row, within the legacy limit (20 per display, 10 s apart). The refresh
  after a queue overflow (a frame evicted from the full queue) is spaced at least
  2 s from the previous refresh but not capped while this option is on; with the
  option off it stays unlimited, as before. Inside the 2 s, the frames after the
  evicted one cannot be decoded correctly (no periodic key frames): the display's
  queue is discarded until a key frame arrives (the picture freezes instead of
  smearing), and the refresh is sent on the first status tick once 2 s have passed
  (`Queue overflow on display N: refresh deferred …`, then `Refresh display N after
  queue overflow (deferred, …)`). A key frame that arrives meanwhile cancels it.
- **Custom fps sent by the user** (fps dialog, image quality), even unchanged: the
  host runs at it again, so the controller drops its lowered target and starts again
  from the custom fps.

```
FPSCTL mode=gentle tick_ms=1000 direct=true custom=60 target=43 floor=24 cap=47.9 overload=1/2 calm=0/5 below_floor=0/10 present_low=0/10 since_change_ms=1000 decision=keep sent=- | d0 rx=43 dec=43 shown=41 … net+server=3.1/9.0;| d0 ready=1 busy=0.93 cap=47.9 queue_p50=0 grow=0 big_queue=0 overload=1 present_low=0;
Set fps to 37 (gentle custom=60 target=43 floor=24 cap=46.0 overload=2/2 … decision=decrease sent=37 direct=true | d0 …;| d0 ready=1 …;)
Refresh display 0 to reduce delay (gentle: queue_max above 30 for 3 ticks, queue_len=41 refresh_times=0)
Refresh display 0 after queue overflow (refresh_times=1)
```

`FPSCTL mode=gentle` is logged every tick while `enable-latency-log` = `Y`, `Set fps to N
(gentle …)` whenever a request is sent, the two `Refresh` lines whenever a refresh is
sent. After the first `|` come the same per-display counters as on the legacy line
(`idle`, `inactive`, `trend` and `decode_fps` there are the legacy controller's and
are not used); after the second `|` the gentle state per display:

| Field | Meaning |
|---|---|
| `custom` | the user's custom fps (30 when unset or outside 5..120) |
| `target` | the fps the host is running at as far as this controller knows: the last request, or `custom` |
| `floor` | `min(custom, 24)` |
| `cap` | the smallest capacity (fps) over ready displays, `-` while unknown |
| `overload` | overloaded ticks in a row / needed for a decrease |
| `calm` | calm ticks in a row / needed for an increase |
| `below_floor` | ticks in a row with `cap` under `floor` / needed to go below it |
| `present_low` | ticks in a row with a present overload / needed for a request |
| `since_change_ms` | time since the last request (or custom change) |
| `decision` | `warmup` (start-up, nothing sent), `keep`, `decrease`, `decrease-below-floor`, `at-floor` (overloaded, already at the lowest allowed), `present` (lasting present overload), `present-at-floor`, `increase` |
| `sent` | the fps sent this tick, `-` if none |
| `d<N> ready` | 1 once the display has 3 ticks and 30 steady frames and decoded something in the last 15 ticks |
| `busy` | decoder thread busy fraction of the tick |
| `cap` | this display's capacity EWMA, `-` while unknown |
| `queue_p50` | median queue length right after a decode during the tick (bucket lower bound: 0, 1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96) |
| `grow` | ticks in a row with a growing backlog (3 make an overload) |
| `big_queue` | ticks in a row with `queue_max` above the refresh threshold |
| `overload` / `present_low` | this display's verdict for the tick |

### Viewer: present pacing

"Always show the newest frame" (`src/client.rs` `PresentPacer`), local option
`enable-viewer-present-pacing`: on by default on Windows (`N` turns it off), off
by default on other platforms (`Y` turns it on). Read when a display's video thread
starts, so a change applies from the next connection (or display switch). No wire
change; the host is not involved.

Every frame is still decoded (later frames reference it). After the decode, the
frame is handed to the UI callback unless a newer frame is already waiting
(`video_queue` not empty, or a key frame in the decoder channel) **and** either the
backlog has lasted 1.5 frame intervals, or the newest received frame is at least
1.5 frame intervals newer (by `pts`) than this one. So two frames arriving
together (Wi-Fi aggregation) are both shown, while a burst after a network stall
is drained at decode speed and only its newest frame is shown. The frame interval
is an average of the `pts` steps (16.7 ms until known). Always shown: key frames,
the first frame after the thread starts, a decoder reset or a codec change. While
the decoder stays behind, a frame is shown anyway once the backlog has lasted
50 ms, counted from when the backlog began (not from the last shown frame), and the
50 ms window starts again: at most about 50 ms between shown frames while frames
keep coming. Each display has its own pacer and counters.

The soft renderer (Android/Quest, desktop with texture render off) follows the
same option on that thread: a newer frame replaces the pending buffer while
Flutter has not started reading it and the size is the same (`ui_replaced`),
instead of being dropped (`ui_dropped`).

Measured by `pacing`, `skip`, `decode_skip`, `shown_gap`, `ui_replaced` on the
LATENCY line, `skip` on the `FPSCTL` line (always on), and the overlay's
`fps dec D / shown S (skip K)`.

#### Decode without output

Local option `enable-viewer-decode-no-output`, on by default (`N` turns it off),
read when a display's video thread starts; it acts only while present pacing is on.
The pacer then decides **before** the decode, with the same rule as above, whether
the frame will be shown. A frame that will not be shown is still decoded, but:

- VP8/VP9/AV1 (libvpx/libaom): no YUV→RGBA conversion (`to(rgb)`).
- Hardware decoding to RAM (FFmpeg, `hwram`): no conversion. With output, only the
  last image of each packet's `decode()` call is converted (was: every image).
- GPU decoding (`vram`), FFmpeg D3D11 driver: `ffmpeg_vram_decode2(output=0)` in
  hwcodec skips the NV12→BGRA shader, the texture pool rotation and the texture
  hand-off; one GPU event query still waits for the decode, so `decode_skip` stays an
  honest decode time and the GPU queue cannot run ahead of the decoder. AMF/MFX
  decoders ignore the flag and always output (counted in `skip`, not in `noout`).
- Android MediaCodec: the output buffer is released without conversion (only with
  pacing turned on there, `Y`).

`handle_frame` returns `DecodeOutput::DecodedNoOutput` for such a frame, a success:
it resets the decoder's fail counter like a shown frame, so skipped frames never get
a codec (H.265) marked unsupported. A frame the pacer accepted because nothing newer
was waiting yet is checked again after its decode; if a newer frame arrived
meanwhile, only the UI callback is skipped (as without this option). With the
option off, the decision is taken after the decode, as before. The recording
(`record_screen`) writes the encoded frames and is not affected.

Measured by `noout`, `decode_skip` vs `decode` (p50; converted skips are in
`decode_skip_conv`, not in `decode_skip`) and `dec_fail` on the LATENCY
line, `noout` on the `FPSCTL` line, and the viewer's CPU and GPU 3D/Copy load with
pacing on and off. `decode`'s meaning is unchanged (shown frames, with conversion).
The gentle fps control's busy fraction is wall time, so it still sees the real
decoder load; its per-frame capacity (`cap`) rises by what skipped frames save.

The hwcodec `NativeDevice::Query` wait (used after the NV12→BGRA shader on every
shown GPU frame, and by the no-output path above) keeps its old shape, a short
poll phase and then 1 ms sleeps, but the poll phase is now bounded by time
(0.2 ms, was 100 polls, about 0.1-0.3 ms) and each failed poll calls `Sleep(0)`
instead of polling again at once. `Sleep(0)` returns at once unless a thread of
equal or higher priority is ready, so the decoder never hands its time slice to a
lower-priority thread (`SwitchToThread` could, for a whole quantum). It still
polls for up to 0.2 ms per wait; it does not wait on a GPU event. No option
(C++ in hwcodec); it changes no output.

### Viewer: priority boost

Local option `enable-viewer-priority-boost` (Windows only, on by default, `N` turns
it off). While a remote-control or camera session is open, its `io_loop` thread (set
inside `io_loop` itself, so a reconnect gets it too) and each display's decoder
thread run at THREAD_PRIORITY_ABOVE_NORMAL, and the process at
ABOVE_NORMAL_PRIORITY_CLASS with EcoQoS / power throttling (execution speed) turned
off. The process boost is counted over the open sessions: the first raises it (only
from NORMAL or lower), the last restores the class (only if it is still
ABOVE_NORMAL) and the earlier throttling state. File-transfer and terminal sessions
are not boosted. Read when a session round or a video thread starts. The aim is a
CPU-loaded viewer that keeps decoding, reading the socket and answering TestDelay;
compare with the option on and off under CPU load: decode p95, `queue`, the `Set
fps` / `FPSCTL` events, `ui_take` / `to_ui` and the Flutter `paint` p50. The process
class raises every thread of the viewer, the Flutter UI and raster threads included
(base priority 8 → 10); the thread boosts put `io_loop` and the decoders one level
above that. The A/B therefore measures both together, and the raster thread is not
an unchanged control. In a portable or not-installed instance the in-process server
shares the class: the last viewer session does not restore it while the server's
`enable-host-priority-boost` holds it at ABOVE_NORMAL (`process=kept_server`), and
with that option `N` the server leaves a class the viewer raised. Logged by
the `LATENCY client priority` lines above. MMCSS "Playback" on the decoder thread is
not part of it (next section).

#### Decoder thread MMCSS "Playback"

Local option `enable-viewer-mmcss` (Windows only, on by default, `N` turns it off),
read when a display's video thread starts. While that display's decoder outputs GPU
textures (`path=vram/…` on the `decoder` line), its decoder thread is registered
with the Multimedia Class Scheduler as a "Playback" task (`mmcss_register`), so it
keeps its time slices under CPU load. Only the thread: the process priority class
is not touched (unlike the host's `mmcss_capture`, which also applies
`enable-host-priority-boost`). Software and RAM decoding are not registered: a
CPU-heavy decode at MMCSS priority could starve the Flutter UI and raster threads
that present. The registration follows the decoder (codec switch, reset) and is
reverted when the thread ends. With the `Query` change above the poll before the
first sleep is capped at 0.2 ms, about what it was before, so the raised priority
adds no longer spin. Logged by the `LATENCY client mmcss` line. A/B it with the option
on and off: decode p95, `queue`, `shown_gap` p95 and the Flutter `paint` p50.

### Viewer: input and audio

Every 5 s while latency is measured:

```
LATENCY client input via=TCP in=0.1/0.4/2.3 n=812 backlog_max=1
LATENCY client audio level=38.0/61.2/95.4 trim=0 trimmed=0ms underrun=1 frames=250 target=60 late95=10 boost=0
```

| Field | Meaning |
|---|---|
| `in` | `Session::send` of a mouse, key or pointer-device message → `io_loop` finished writing it to the peer stream (`peer.send` returned). Includes the wait in the UI → `io_loop` channel behind whatever `io_loop` is busy with (video frames, file transfer) and the socket write. p50/p95/max ms. Messages are matched in FIFO order; the first ones after measurement starts can be off. |
| `n` | input messages measured in the window |
| `backlog_max` | most input messages queued at one write, the written one included |
| `level` | playback buffer level just before each received audio frame is appended: **min/p50/max** in ms. A level that stays high after a network stall is audio lag the trim policy did not remove. |
| `trim` / `trimmed` | how many times the trim policy dropped buffered audio, and how much in total: the adaptive target (at most every 2 s) or the legacy shrink (`try_shrink`, at most every 12 s) |
| `underrun` | output callbacks that found less audio than they needed after one that had enough (each audible gap counts once; includes a callback that lost the buffer lock to the decoder thread). Not counted: the underrun that ends the audio before a pause of 1 s or more (the host stops sending after silence), and underruns while nothing is measured (the counter is reset on every audio frame). |
| `frames` | audio frames in the window |
| `target` | the adaptive playback buffer target at the end of the window, ms; `legacy` when `audio-target-ms` selects the old rule (then `late95` and `boost` are absent) |
| `late95` | p95 of the arrival lateness over the last 20 s, ms (upper edge of a 10 ms bin): how much later than its schedule an audio packet arrived. The schedule is the earliest arrival seen plus the audio received since, re-based every second by the slot's smallest lateness (clock drift) and restarted after a 1 s pause in the audio. |
| `boost` | what output underruns during a network stall added to the target, ms (+20 per underrun, −10 per 15 s without one) |

The audio line exists only on Windows/macOS/Android (not the Linux PulseAudio path), and
only while any video stream of the viewer process is measured; with several sessions the
lines do not say which session they belong to. Capacity overwrites past 3 s of audio are
not counted in `trim` (they have their own debug log line).

Adaptive audio target (`src/client/audio_target.rs`), local option `audio-target-ms`:
empty is adaptive on Windows and the legacy rule elsewhere, `0` is the legacy rule
everywhere, a number N > 0 is adaptive with N ms as the lower bound (at most 2000).
Read when the audio format of a session starts. Only the viewer's received audio
(`start_audio_thread`) follows it; the microphone audio a host plays into its virtual
microphone (`start_microphone_audio_thread`) keeps the legacy rule and logs no
`audio_policy` line. Target =
min(max(`late95` + 20 ms, floor) + `boost`, max(300 ms, floor)), floor 60 ms by default.
The buffer level is sampled just before each push; when the lowest level of a 1 s slot
stays above target + 20 ms for 2 slots in a row, the excess is trimmed down to the
target (oldest audio first, whole channel frames, with the playback crossfade). It only
trims, never pads. An underrun counts towards `boost` only when the buffer was read
(not lock contention) and audio resumed within 1 s (the host stops sending after about
8 s of silence; that end is not a stall). Expected after a Wi-Fi stall: `level` back to
about `target` within 2–3 s, instead of the old rule's lasting extra lag below 600 ms;
`underrun` about 0 between stalls.

### Viewer (Flutter)

```
LATENCY flutter image=p50/p95/max paint=p50/p95/max frames=…
```

- `image`: start of `ImageModel.onRgba` → the new `ui.Image` set
  (`decodeImageFromPixels`: copy + image creation, then `update`). Part of the
  Rust `ui_take`, logged for the breakdown only.
- `paint`: the new `ui.Image` set (right before `nextRgba`, where `ui_take`
  ends) → the post-frame callback of the next frame: wait for vsync,
  build/layout/paint on the UI thread. `frames` counts these samples.

`ui_take` and `paint` follow each other with no gap and no overlap.

At most one paint measurement is pending. When several images are set before a
frame, only the last one (the one actually painted) is counted. Nothing is
measured while frames are disabled (app paused or hidden, Quest headset off), and
samples over 1 s are discarded (a frame that waited for the app to resume).
`addPostFrameCallback` does not request a frame, so when nothing redraws (remote
page not on screen) `paint` reads high.

**Not included:** raster thread work, GPU upload/composition, the Android/Quest
compositor and display scan-out (typically one or two more refresh periods).
Measured only for the soft-render path (Quest/Android, and desktop with texture
render off).

### Quality monitor overlay

Below the existing rows:

```
Lat: net+srv ~X | queue Y | dec Z | ui W | take U | paint V | total ~T ms | fps dec D / shown S (skip K)
```

`D`, `S` and `K` are the window's `dec_fps`, `shown_fps` and `skip` per second (JSON keys
`dfps`, `sfps`, `skip`; `rfps` is `rx_fps`). `?` with an older Rust core that sends no such key.

All values are **p50** of the last 5 s window of the shown display. `total` is
`total_to_ui(est)` p50 + `ui_take` p50 + `paint` p50 (a sum of medians, not the
median of the sum). `?` means not available yet: the first report comes 5 s
after the overlay is turned on or after a display switch, and the row is
cleared in both cases rather than showing older numbers.

## Clock estimate

The plan was to take the host's clock from `TestDelay.time`. That does not
work without a protocol change:

- The host sends `TestDelay` with `time = 0` (`connection.rs`, `..Default::default()`);
  only `last_delay` (the previous round trip) and `target_bitrate` are filled.
- The frame timestamp `EncodedVideoFrame.pts` is not wall-clock time: it is
  milliseconds since the host's capture loop started (`video_service.rs`,
  `ms = now - start`), and it restarts at 0 whenever the capture loop restarts
  (codec switch, display change, new subscriber, …).

So the viewer estimates the mapping from the frames themselves:

1. For every received frame: `transit = local_receive_time - pts`. This equals
   the true one-way delay plus an unknown constant (the clock offset).
2. `anchor = min(transit)` over the last ~30 s (six 5 s buckets, each dropped
   30 s after it started even when no frame arrived, e.g. a static screen with
   a latency-free encoder). The fastest frame defines "zero excess delay".
3. `net+server(est) = (transit - anchor) + rtt_min / 2`.

Error bounds:

- The fastest frame's real pts→receive delay is assumed to be `rtt_min/2`.
  The truth lies between its host-side time and that plus `rtt_min` (one-way
  delay can be anywhere between 0 and the round trip), so the network part is
  off by at most **±rtt_min/2** (printed as `offset_err`).
- The fastest frame's own host-side time (`grab` + `encode` of that frame, at
  least a few ms; see the host log) is **not** included, so the estimate is
  biased **low** by that much.
- `pts` is taken at the loop tick, before the once-per-second display check
  and before the capturer waits for a desktop change, so it can precede the
  actual screen update by up to `spf`. Excess `grab` waiting (and the display
  check) therefore shows up in `net+server(est)`; compare with the host's
  `grab`.
- `last_delay` is measured by the host and includes the host's own send
  queue and the viewer's `io_loop`, so on a busy link `rtt_min` is high, which
  makes the estimate high and the bound loose.
- Clock drift between the two machines over the 30 s anchor window is
  negligible (≈1 ms at 30 ppm). Because buckets expire by age, a long static
  period does not stretch the window.
- If the excess delay stays high for the whole 30 s window (sustained
  backlog), the anchor rises with it and the estimate under-reports; `queue`,
  `queue_len` and the host's `wait` still show the backlog.
- After a host capture restart the anchor is rebuilt from new frames; frames
  of the old capture loop still queued at that moment get no estimate.

A wire-exact number needs either a host timestamp in a field the viewer can
read, or an end-to-end camera test (photograph a millisecond clock on the host
screen next to the headset view).

## Reading the logs

### Windows

Rust logs are files under the RustDesk log folder, one subfolder per process:

- Viewer (the RustDesk window you connect from): `%APPDATA%\RustDesk\log\`
- Host, installed as a service: the `server` subfolder of the service's
  profile, usually
  `C:\Windows\ServiceProfiles\LocalService\AppData\Roaming\RustDesk\log\server\`
  (portable or not installed: `%APPDATA%\RustDesk\log\server\`)

```powershell
Select-String -Path "$env:APPDATA\RustDesk\log\*.log" -Pattern LATENCY | Select-Object -Last 20
Get-Content <log folder>\server\<newest>.log -Wait | Select-String LATENCY
```

### Quest / Android

In release builds the Rust logger writes to a file inside the app's private
storage, not to logcat. The Flutter side therefore re-prints each new Rust
`LATENCY client` line with `debugPrint`, next to its own `LATENCY flutter
paint` line, and both reach logcat (tag `flutter`):

```sh
adb logcat | grep LATENCY
# Windows:  adb logcat | findstr LATENCY
```

The Rust lines are also in the app's log file
(`<app files>/RustDesk/Logs`), readable with `adb shell run-as <package>` only
on debuggable builds. Debug builds log Rust directly to logcat (tag `ffi`), so
there each client line appears twice.

## Reading the numbers together

Approximate glass-to-glass for one frame:

```
host:   grab (part that is real capture, not waiting) + encode + wait
net:    net+server(est) − host part                     (viewer line)
viewer: queue + decode + to_ui                          (viewer line)
UI:     ui_take                                         (viewer line)
      + paint                                           (flutter line)
+ compositor / display (not measured, ~1–2 refresh periods)
```

Things to look for:

- `queue` p95 or `queue_len` growing: the decoder is slower than the frame rate
  (watch `decode`), or a burst arrived after a network stall.
- `wait` spikes on the host with `send` max above `spf` at the same time: the
  socket write is the bottleneck (bitrate vs link, Wi-Fi or TCP/KCP
  retransmits). `wait` spikes with `send` max well below `spf`: the connection
  loop was busy with other work, or a web client's ack was late.
- `skip` > 0 with pacing on: frames arrived faster than the viewer showed them
  (backlog or burst); `shown_gap` p95 tells whether it stuttered.
- `ui_dropped` > 0: Flutter takes frames slower than the decoder delivers;
  `ui_take` and `paint` tell by how much. A large `ui_take − image` means the
  frame waited for a busy UI isolate before Dart even started on it.
- `sleep` close to `spf` with `idle` high: the loop mostly waits for screen
  changes; latency of a change is then dominated by the `spf` grid.
- A `wait` spike on the host with a `LATENCY conn` `busy` max in `video` on one
  connection: that viewer's socket write held the frame (compare that line's
  `send`). `busy` max in `read` or `rx`: the loop was busy elsewhere while
  the frame waited, and the viewer's input waited too (`in_gap`).
