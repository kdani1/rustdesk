# Video latency metrics ("step 0")

Per-stage latency numbers for the video path. They are measurement only: no
frame rate, bitrate, queue or transport behaviour changes, and there is no wire
protocol change, so the build stays compatible with stock RustDesk peers.

All numbers are wall-clock durations on one machine's monotonic clock, except
the two marked **(est)**, which need the host's clock and are estimated (see
"Clock estimate" below).

Code:

| Side | File | Hooks |
|---|---|---|
| Host | `src/server/latency_stats.rs` | `src/server/video_service.rs` `run()`, `src/server/connection.rs` `rx_video` (video frame write) |
| Viewer | `src/client/latency.rs` | `src/client/io_loop.rs` (receive, TestDelay, 1 s status timer), `src/client.rs` `start_video_thread`, `src/flutter.rs` (soft-render handoff and drop, `next_rgba`, quality status) |
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
LATENCY server display=0 fps=29.8 spf=33ms grab=… encode=… wait=… send=… sleep=… sent=149 skipped=0 idle=12 send_slow=0
```

| Field | Meaning |
|---|---|
| `fps` | frames handed to at least one connection, per second |
| `spf` | current frame period from QoS (1000 / target fps) |
| `grab` | capturer call until it returned the image. **Includes the wait for the desktop to change**: DXGI `AcquireNextFrame` blocks up to `spf`. A large `grab` p50 on an active screen means the capture is waiting, not slow. The once-per-second display check, which runs after the frame's `pts` is taken and before the capturer call, is not in `grab` (it is in the viewer's `net+server(est)`). |
| `encode` | after grab until the encoded frame was put on every connection's queue: BGRA→YUV conversion (RAM path), encode, protobuf, `send_video_frame` |
| `wait` | after that until every connection had **taken the frame off its queue to start sending** ("fetched", `connection.rs` `rx_video`). It is the time the frame waits behind the previous frame's socket write. It is not the time until the peer received it: bytes can still sit in the kernel send buffer. The previous frame's write overlaps this frame's sleep, grab and encode, so only the part of `send` that outlasts them (about one `spf`) shows up here; the rest of `wait` is the connection's select loop busy elsewhere (input, audio, file transfer), or, for a web client, the viewer's late ack. |
| `send` | one connection's `stream.send` of one video frame. Over TCP: protobuf serialization, encryption and the write into the socket, including any wait for send-buffer space (backpressure). Over WebRTC it is the data-channel queueing, over a relay the write to hbbr. One sample per connection per frame, all connections of the display merged. Not the time until the peer received the frame. A capture-loop restart (`SWITCH`) drops that display's partial window. A camera and a monitor with the same index share one `send` store, so either line can hold the other's writes and only part of its window. |
| `send_slow` | how many of those writes took more than a fixed 16 ms (counted even beyond the 1024-sample cap) |
| `sleep` | the rest of the frame period the loop sleeps after `wait` (only iterations that slept) |
| `sent` / `skipped` | captured frames that were / were not handed to a connection (encoder produced nothing, e.g. libvpx frame dropping, or no subscriber) |
| `idle` | iterations where the capturer had no new image within `spf` (static screen) |

Not covered: the re-encode of the last image that non-latency-free encoders
(mediacodec, videotoolbox) do on idle iterations.

### Viewer (Rust)

```
LATENCY client display=0 codec=H265 hw=false fps=29.9 net+server(est)=… queue=… decode=… to_ui=… ui_take=… total_to_ui(est)=… offset_err<=1.5ms queue_len=2 ui_dropped=3 rtt_min=3ms
```

| Field | Meaning |
|---|---|
| `codec` | negotiated codec of the decoded frames |
| `hw` | `true` only when the frame was decoded to a GPU texture (VRAM decoder, Windows). `false` covers both software decoding and the FFmpeg "hwram" decoders, which can be hardware on desktop. On the Quest build (no `mediacodec` feature) `false` means software. The decoder actually created is in the log ("create … decoder"). |
| `fps` | decoded frames per second |
| `net+server(est)` | host loop tick (`pts`) → message parsed in `io_loop` on the viewer. Includes host grab, encode, host queueing, socket buffers, network, retransmits. See "Clock estimate". |
| `queue` | parsed in `io_loop` → taken by the decoder thread (key frames: the decoder channel; other frames: the 120-deep `ArrayQueue`) |
| `decode` | `VideoHandler::handle_frame`: decode plus YUV→RGBA conversion |
| `to_ui` | decode done → the UI callback returned. Soft render (Android/Quest): buffer swap and posting `EventToUI::Rgba`. Texture render: copy into the texture. |
| `ui_take` | soft render only: buffer marked ready for Flutter (just before `EventToUI::Rgba` is posted) → Flutter released it (`next_rgba`). Covers the wait in the Dart UI isolate's event queue, `getRgbaSize`/`getRgba`, and `decodeImageFromPixels` + `update` in `ImageModel.onRgba` (the Flutter line's `image` part). `ui_take − image` ≈ the port-queue wait, which grows when the UI isolate is busy. Measured on one clock (Rust), sampled per frame Flutter took; frames counted in `ui_dropped` have none. It overlaps `to_ui` only by the post call itself (microseconds). `n/a` on the texture-render path. |
| `total_to_ui(est)` | `net+server(est)` + receive → `to_ui` done, per frame (does not include `ui_take`) |
| `offset_err<=` | the clock estimate's network error bound, `rtt_min/2` |
| `queue_len` | the largest video queue backlog seen after a decode in the window |
| `ui_dropped` | decoded frames the soft renderer threw away because Flutter had not taken the previous one yet. They have `queue` and `decode` samples but no `to_ui`/`total`. |
| `rtt_min` | smallest non-zero round trip the host reported in its last 10 TestDelay probes (`last_delay`) |

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
Lat: net+srv ~X | queue Y | dec Z | ui W | take U | paint V | total ~T ms
```

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
- `ui_dropped` > 0: Flutter takes frames slower than the decoder delivers;
  `ui_take` and `paint` tell by how much. A large `ui_take − image` means the
  frame waited for a busy UI isolate before Dart even started on it.
- `sleep` close to `spf` with `idle` high: the loop mostly waits for screen
  changes; latency of a change is then dominated by the `spf` grid.
