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
| Host | `src/server/latency_stats.rs` | `src/server/video_service.rs` `run()` and its restart wrapper in `new()`, `src/server/connection.rs` (select loop, `rx_video` video frame write, `refresh_video_display`), `src/server.rs` `create_tcp_connection` (`TransportInfo`), `libs/scrap/src/dxgi/mod.rs` (`last_present_age`) |
| Viewer | `src/client/latency.rs` | `src/client/io_loop.rs` (receive, TestDelay, 1 s status timer, `fps_control`, writes of UI messages), `src/client.rs` `start_video_thread` (`VideoCounters`, `PresentPacer`), `AudioBuffer` / `start_audio_thread`, `src/client/audio_target.rs` (adaptive audio target), `src/platform/windows.rs` (`viewer_thread_boost`, `viewer_process_boost`), `RaceLog`, `VideoHandler`, `src/client/audio_playback.rs` (underruns), `src/ui_session_interface.rs` `Session::send`, `src/flutter.rs` (soft-render handoff, drop and replace, `next_rgba`, quality status) |
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
LATENCY server display=0 fps=29.8 spf=33ms grab=… encode=… wait=… send=… sleep=… sent=149 skipped=0 idle=12 send_slow=0 age=… kbps=8130 set_kbps=4096 maxf=61234 key=1 keymax=61234 qchg=1 qchg_key=0 gap=n/a pacing=deadline reanchor=0
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
| `gap` | capture restarts completed in the window (see `LATENCY switch` below): from the refresh request or the restart until the restarted capture loop sent its first frame to a connection, p50/p95/max ms |

`age`, `kbps`, `maxf`, `key`, `keymax`, `qchg`, `qchg_key` and `reanchor` are only counted while
logging is on, like the other fields; `set_kbps` is kept always.

Not covered: the re-encode of the last image that non-latency-free encoders
(mediacodec, videotoolbox) do on idle iterations.

### Host: capture restarts, connection loop, transport

```
LATENCY switch source=monitor display=0 reason=refresh restarts=1 gap_ms=187 not_logged=0
LATENCY conn id=3 via=TCP-punch busy=0.0/0.4/45.0 busy_max_in=video busy_slow=2 in_gap=… in_slow=4 in_n=812 send=… send_slow=1 frames=298
TRANSPORT host conn=3 type=TCP-punch peer=192.0.2.10:52311 secure=true detail=accepted
```

| Line / field | Meaning |
|---|---|
| `LATENCY switch` | one line per completed capture restart (`gap`), with logging on, at most one per source and display per second. A capture restart is any error return of the capture loop (`run()` in `video_service.rs`), which the service restarts: a `SWITCH` (refresh, codec change, new subscriber, …) or an error. |
| `source` / `display` | `monitor` or `camera`, and the display index. Gaps are kept per source and display. |
| `reason` | why the first restart of the gap happened. For a `SWITCH`: `refresh` (a viewer's `RefreshVideo`; also when a requested refresh was served by a fresh capture start), `codec`, `portable_service`, `i444`, `gdi_vram`, `screenshot`, `dxgi_access_lost`, `scale` (Android), `privacy_mode`, `new_subscriber`, `encode_fail`, `encoder_need_switch`, `display_changed`, `quality` (encoder that cannot change bitrate), `record`. For any other error: its text, spaces as `_`, at most 60 characters. |
| `restarts` | restarts within this gap (a restart that failed again before sending a frame adds one) |
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
| `TRANSPORT host` | once per incoming connection, when it is admitted (before the identity handshake), whether logging is on or not |
| `conn` | the connection id |
| `type` | `Intranet` (the controller connected to our listener after `FetchLocalAddr`), `TCP-punch` (a TCP hole punch), `Direct` (direct IP access to the listening port), `KCP-UDP` / `KCP-IPv6` (KCP over a punched UDP socket), `WebRTC`, `Relay` (through hbbr) |
| `peer` | the peer address the connection keys on (for `Relay` the controller's address hbbs reported, not hbbr's) |
| `secure` | whether the identity handshake runs the key exchange |
| `detail` | `TCP-punch`: `accepted` (the controller's connect reached our listener) or `dialed` (our own punch connected); `Relay`: the relay server; `-` otherwise |

### Viewer (Rust)

```
LATENCY client display=0 codec=H265 hw=false pacing=on fps=29.9 net+server(est)=… queue=… decode=… to_ui=… ui_take=… total_to_ui(est)=… offset_err<=1.5ms queue_len=2 ui_dropped=3 ui_replaced=0 rtt_min=3ms rx_fps=30.0 dec_fps=29.9 shown_fps=29.9 skip=0 decode_skip=n/a shown_gap=… discard=0 dec_fail=0 via=TCP
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
| `decode_skip` | `decode` of the skipped frames (p50/p95/max ms), `n/a` when none was skipped |
| `shown_gap` | time between two consecutive frames that reached the screen (`to_ui` done → next `to_ui` done; frames counted in `ui_dropped`, and frames a later one replaced (`ui_replaced`), are left out: the gap runs from the frame shown before the replaced one to the replacing one), p50/p95/max ms. A gap is added once the next frame did not replace its frame, so the newest one lands in the next window. Gaps over 1 s (static screen) are left out. Its p95 shows stutter: with pacing on it must not grow in calm use. |
| `discard` | received frames dropped undecoded, count in the window: `discard_queue` after a refresh request, or evicted by a full 120-deep queue |
| `dec_fail` | `handle_frame` calls that returned no picture or an error |
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
LATENCY client priority process=restored
LATENCY client audio_policy=adaptive floor_ms=60
audio trim 412ms to target 60ms, generation=3
TRANSPORT viewer peer=123456789 type=TCP direct=true secured=true kcp=false rendezvous=rs.example.com:21116 connect_ms=412
RACE site=connect tcp=ok@180 udp=err@3010 v6=pending webrtc=pending winner=TCP ms=180
```

| Line / field | Meaning |
|---|---|
| `decoder` | every decoder creation (`VideoHandler::new` / `reset`, e.g. a codec switch) |
| `pacing` | once per video thread (display) start: whether present pacing is on for it (the option is read then) |
| `priority thread=` | viewer priority boost (`enable-viewer-priority-boost`, Windows), once per `io_loop` round of a remote-control or camera session and once per decoder thread (`display=`) start. `prio`: `above_normal` (raised to THREAD_PRIORITY_ABOVE_NORMAL, restored when the round or thread ends), `off` (option `N`), `kept:<n>` (the thread already ran at that priority or higher, unchanged), `failed (<error>)` |
| `priority process=` | the counted process boost: logged when the first such session raises it and when the last one ends (`restored`, or `kept_server` when the in-process server's host priority boost holds the class). `process`: `above_normal` (raised from NORMAL or lower), `kept:<class>` (the class, hex, was already higher: unchanged and not restored), `failed(<error>)`. `ecoqos`: `opt_out` (power throttling of execution speed off; the earlier state is put back on release), `failed(<error>)` |
| `audio_policy` | once per audio format of a session (`AudioBuffer` creation): `adaptive` (the `audio-target-ms` target, `floor_ms` its lower bound) or `legacy` (`try_shrink`) |
| `audio trim` | each trim of the adaptive policy: how much buffered audio was dropped, the target it trimmed to, and the playback discontinuity generation (crossfade). Not tied to the latency option; at most one every 2 s. The legacy rule logs `skip <samples>, based …` instead. |
| `path` | `vram/<driver>` (hwcodec GPU decoder, D3D11 texture output; driver `FFMPEG`, `AMF`, `MFX` or `NV`), `hwram/<FFmpeg decoder>/<hw device>` (FFmpeg decoder with CPU output, converted to RGBA), `vpx`, `aom` (software), `other` (MediaCodec), `none` (creation failed) |
| `hw` | `true` for the `vram` path: frames reach the UI as GPU textures (the `hw` of the LATENCY line) |
| `use_texture_render` | the viewer's texture-render setting. With `vram` and this off nothing is displayed. On desktop the RGBA path uses the texture renderer when this is on or the session shows more than one display, otherwise the soft renderer (`ui_take`, `ui_dropped`). |
| `TRANSPORT viewer` | once per connection round, right after the connection is established: `type` the transport label, `direct` whether it is peer to peer, `secured` the encrypted handshake, `kcp` KCP over UDP, `rendezvous` the rendezvous server used, `connect_ms` from the start of `Client::start` (rendezvous, punching, race, secure handshake) |
| `RACE` | one line per connection race. `site`: `start` (the preferred punch request against the TCP-punch-only request, `preferred` / `tcp_punch_round`), `connect` (the direct attempts after a punch: `tcp`, `udp`, `v6`, `webrtc`, then `relay` when the relay was requested afterwards), `relay_response` (`v6`, `relay`, `webrtc` after a relay answer). Each candidate: `ok@ms` / `err@ms`, milliseconds since the race started, or `pending` when the race ended before it finished (it was dropped). `winner` is the transport label of the result, `none` if all failed; `ms` the race's total time. |

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
not part of it.

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
