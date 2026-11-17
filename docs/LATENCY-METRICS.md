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
LATENCY server display=0 fps=29.8 spf=33ms grab=… encode=… wait=… send=… sleep=… sent=149 skipped=0 idle=12 send_slow=0 age=… kbps=8130 set_kbps=4096 maxf=61234 key=1 keymax=61234 qchg=1 qchg_key=0 gap=n/a pacing=deadline reanchor=0 rkey=0 rkept=0 rmerge=0 refine=0 vfps=0 vfchg=0 gpuprio=off rfi=0 rfi_idr=0 rfi_kept=0 rfi_max=0 qos_sig=tcp
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
| `set_kbps` | the bitrate the encoder was last set to (`encoder.bitrate()`, kbps; at capture start and after each `set_quality`): the QoS bitrate. With NVENC opened at 30 fps and run at 60 fps, `kbps` ≈ 2 × `set_kbps` is expected (`vram.rs`, framerate 30). With the virtual framerate (`vfps` > 0) NVENC itself runs at `set_kbps` × clamp(nominal / `vfps`, 0.5, 4), and `kbps` ≈ `set_kbps` is expected under full motion. |
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
| `rfi` | Windows host, `enable-nvenc-rfi=Y` (M22): live UDP lanes' loss reports served by reference frame invalidation on the running encoder (the lost frames are no longer referenced; the next frame is the recovery point). Requests of several lanes for one display that came within one loop iteration count once; a lane whose wait a recovery frame or key frame ended before the loop took its request withdraws it (not counted) |
| `rfi_idr` | RFI requests the encoder could not serve (not the NV SDK driver, H.264, the lost frame more than 4 frames back or unknown), and recovery frames the kept image could not give (that invalidation is also in `rfi`): a key frame through the refresh path instead (also counted in `gap`/`rkey` like any refresh) |
| `rfi_kept` | recovery frames encoded from the kept last image (`encode_last`) because no new image came within one capture wait after the invalidation (static screen) |
| `rfi_max` | largest recovery frame of the window, bytes (0 when none): compare with `keymax` for the spike RFI saves |
| `refine` | static refine (on with `nvenc-vbv-frames` > 0 when the running encoder is FFmpeg nvenc, the one that gets the VBV): re-encodes of the kept last image after a key frame or a large frame (> 2 × the running average size) once the screen stayed unchanged for 150 ms, at most 3 per such frame, one per loop iteration. They are in `kbps`, `maxf` and `key`, not in `sent`, and they give no `wait` sample (neither do `rkept` key frames), so `wait` compares the same captured frames whether refine is on or off. |
| `vfps` | Windows host, virtual framerate (`nvenc-nominal-fps` > 0, nvenc encoders only): the frame rate the encoder's bitrate is scaled for at the end of the window, the QoS fps cap, or with `nvenc-fps-from-send-rate=Y` the encoded frames per second kept between half the cap and the cap. 0: off (option 0, another encoder, or the encoder refused a rate and was reset to `nvenc-nominal-fps`). When the encoder refuses a rate and also the reset to nominal (in practice only the NVENC SDK driver, whose Reconfigure can fail), it keeps the last applied value, the rate the encoder is stuck at, until the capture restarts; a warning `virtual fps: ... stays at N fps until restart` marks this. Either way later cap changes are not applied until the capture restarts. |
| `vfchg` | how often `vfps` changed in the window (each one is an IDR-free nvenc reconfigure with the next frame) |
| `gpuprio` | Windows host (roadmap M17): what this capture loop's start applied. `off`: neither `enable-host-gpu-priority` nor `enable-host-dwm-mmcss` was `Y` (also on other platforms). `gpu`: GPU priority on and in effect (process class `high`/`above_normal`/`held`/`kept:…`, or `SetGPUThreadPriority` on the capture device returned success); `gpu-failed`: the option is on but neither took effect. `dwm`: `DwmEnableMMCSS(TRUE)` succeeded (or was already on); `dwm-failed`: the option is on but the call failed or is unavailable. Both options on: the parts joined by `+` (`gpu+dwm`, `gpu-failed+dwm`, `gpu+dwm-failed`, `gpu-failed+dwm-failed`). Labels A/B windows; the return codes are in the `LATENCY server gpu_priority` line of the same capture start (below). |

`age`, `kbps`, `maxf`, `key`, `keymax`, `qchg`, `qchg_key`, `reanchor`, `rkey`, `rkept`,
`rmerge`, `refine`, `vfchg` and the `rfi` fields are only counted while logging is on, like the
other fields; `set_kbps` and `vfps` are kept always.

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
  with `nvenc-nominal-fps` at 0, while the loop runs at up to 60, so N = 1 is one NVENC frame
  budget, about two real frames at 60 fps. With the virtual framerate (`nvenc-nominal-fps` > 0)
  the bitrate in that formula is the rescaled one and the frame rate the nominal one, so N = 1
  is one real frame of the QoS bitrate at `vfps` (inside the 0.5-4 clamp). `ldkfs` is left at
  NVENC's default (1 with `tune=ull`). Turns static refine on for FFmpeg nvenc encoders: the two
  only go on together (roadmap M14); another encoder (VP9/AV1, AMF, QSV, the NVENC SDK driver)
  gets neither.
- **`enable-static-refine`** (`N` turns static refine and the VBV off together, so
  `nvenc-vbv-frames` is then ignored; any other value changes nothing, refine is on exactly when
  the running encoder got the VBV of `nvenc-vbv-frames` > 0; read when an encoder is created):
  see `refine` above.

### Host: virtual framerate (roadmap M7)

Windows host, server options, off by default in this build.

- **`nvenc-nominal-fps`** (number; empty or `0` = today: nvenc opens at 30 fps and its bitrate
  is not rescaled; other values are clamped to 10-120, `60` is the intended one; read when an
  encoder is created). FFmpeg nvenc (VRAM and RAM paths) and the NVENC SDK driver open at this
  frame rate. NVENC's CBR budgets bitrate / frame rate per frame, so the capture loop tells the
  encoder the frame rate it runs at (`vfps`) and FFmpeg nvenc sets its bitrate to the QoS
  bitrate × clamp(nominal / `vfps`, 0.5, 4): the per-frame budget is QoS bitrate / `vfps` and
  the real bitrate follows the QoS bitrate. FFmpeg's nvenc reconfigure applies it with the next
  frame, free of an IDR with patch 0009 (so an fps change needs no restart; `vfchg` counts them).
  The NVENC SDK driver reconfigures its real frame rate instead. The 4× clamp bounds the scale
  relative to the current QoS bitrate, which itself moves with every QoS ratio change after open;
  so the result is also capped at the larger of 4× the opened bitrate and the QoS bitrate (HEVC
  level limits: the virtual framerate never asks for more than 4× the opened bitrate unless QoS
  alone already does). Below nominal / 4 fps, or when that cap applies, the stream stays under
  the QoS bitrate. AMF, QSV, VP8/VP9/AV1 are unchanged. The price: at 60 fps a frame gets
  half the bits it gets today at the same quality setting, so text after typing or scrolling is
  softer (check `maxf` and the picture).
- **`nvenc-fps-from-send-rate`** (`Y`; read when the capture loop starts; only with
  `nvenc-nominal-fps` > 0). `vfps` is the number of frames encoded in the last second (the
  `check_qos` window, refines and kept-image key frames included) instead of the QoS fps cap,
  kept between half the cap and the cap. It rises with the next window, falls only after two
  windows in a row, and changes under 10 % or of 1 fps are ignored; a cap change applies at once.
  A sparse desktop (typing) then keeps today's bits per frame at a 60 fps cap (`vfps` 30), full
  motion gets the QoS bitrate, and the first second of motion after an idle screen is at most 2×
  the QoS bitrate (today's steady state at 60 fps).

Gate for turning it on (roadmap M7): `kbps` / `set_kbps` about 1.0 at 60 fps under motion, no
`LATENCY switch` on an fps change, text sharpness and `maxf` while typing and scrolling, Wi-Fi
`wait` p95 compared with option 0, and `new encoder:` still showing the VRAM encoder.

Log lines (always, not gated by `enable-latency-log`; hwcodec lines carry `[UTIL]`, `[FFMPEG_VRAM_ENC]` or `[FFMPEG_RAM_ENC]`):

| Line | Meaning |
|---|---|
| `[UTIL] hevc_nvenc open: bit_rate=… fps=… rc_buffer_size=… vbv_frames=… forced_idr=…` | once per nvenc encoder: what FFmpeg settled on after `avcodec_open2`. With `vbv_frames=0` `rc_buffer_size` is FFmpeg's own default VBV (expected 2 × `bit_rate`), with N > 0 the value set. `forced_idr=0` means FFmpeg refused `forced-idr`, or an earlier encoder of that name gave a forced IDR without the key flag in this process; the encoder then reports forced key frames as unsupported (refresh then restarts at once). |
| `forced IDR verified: key frame of N bytes` | once per encoder: the first forced key frame came out key-flagged |
| `forced IDR gave no key frame, forced keyframes off` | a forced key frame came out without the key flag (`refresh_no_key` follows): forced key frames are reported unsupported for encoders of that name until the host process restarts, so this line comes at most once per process and name, and later refreshes restart at once (`refresh_unsupported`) |
| `[FFMPEG] avg bitrate change: a -> b` / `[FFMPEG] vbv buffer size change: a -> b` | with `nvenc-vbv-frames` > 0, FFmpeg's own nvenc reconfigure lines for the first 8 such messages in the process: the proof that a bitrate change re-applies the VBV |
| `static refine on` | the capture loop starts with static refine |
| `[FFMPEG_VRAM_ENC] hevc_nvenc virtual framerate: opened at 60 fps, runs at 30, bit_rate … for … kbps` (or `[FFMPEG_RAM_ENC]`) | once per nvenc encoder, the first virtual-framerate rescale: the opened frame rate, the running one, the bitrate set in bit/s and the QoS bitrate. Also turns on FFmpeg's `avg bitrate change` lines (above) for the next 8 reconfigures |
| `virtual fps: <service> encoder runs at N fps (cap C)` | the capture loop applied the virtual framerate the first time after an encoder was created |
| `virtual fps: <service> encoder refused N fps, off until restart` | `set_fps` failed (should not happen with FFmpeg nvenc; the NVENC SDK driver's reconfigure can fail); the encoder keeps the bitrate it had, `vfps` becomes 0 |
| `refresh: this encoder has no forced key frame, switch` | `refresh_unsupported` |
| `[NVENC] caps hevc: rfi=… ir=… ss_ir=… multiref=… h264: …; hevc encoder rfi=on dpb=5 numRefL0=1` | M22, once per encoder of hwcodec's NV SDK driver opened with `enable-nvenc-rfi=Y`: the NVENC caps `SUPPORT_REF_PIC_INVALIDATION`, `SUPPORT_INTRA_REFRESH`, `SINGLE_SLICE_INTRA_REFRESH` and `SUPPORT_MULTIPLE_REF_FRAMES` for both codecs on this GPU and driver, and whether this encoder runs with RFI (HEVC with the cap only; `off` for H.264). The roadmap's precondition for M22 |
| `vram encoder: NV SDK driver for reference frame invalidation (luid …)` | M22: an HEVC encoder on NVIDIA was opened with the NV SDK driver instead of FFmpeg nvenc because a lane wants RFI |
| `vram encoder: the NV SDK driver failed to open, FFmpeg nvenc stays (no RFI in this process)` / `… failed, FFmpeg nvenc from the next start` | M22: the NV SDK driver could not be opened, or its encodes failed (the capture loop's usual encode-failure restart): FFmpeg nvenc for the rest of the process, hardware encoding stays on |
| `switch to the NV SDK driver for reference frame invalidation` | M22: a lane wants RFI while the running HEVC encoder is FFmpeg nvenc: one capture restart, `reason=nv_rfi` |

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
| `reason` | why the first restart of the gap happened. For a `SWITCH`: `refresh` (a viewer's `RefreshVideo`; also when a requested refresh was served by a fresh capture start), the `enable-refresh-keyframe` reasons below, `codec`, `nv_rfi` (M22: switch to the NV SDK driver), `portable_service`, `i444`, `gdi_vram`, `screenshot`, `dxgi_access_lost`, `scale` (Android), `privacy_mode`, `new_subscriber`, `encode_fail`, `encoder_need_switch`, `display_changed`, `quality` (encoder that cannot change bitrate), `record`. For any other error: its text, spaces as `_`, at most 60 characters. |
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

### Host: fast bitrate estimator, shadow mode (roadmap M15)

Server option `enable-fast-abr-shadow`: on Windows **on** unless `N` (it only logs); on other
platforms off unless `Y`, so they behave as before by default. Each
authorized remote-control or camera connection runs a delay-based congestion estimator
(`src/server/video_qos/fast.rs`) and logs the bitrate it would give the shared encoder. It
changes nothing the host sends: the existing TestDelay controller still sets the bitrate and
the fps. Created when a connection is authorized while the option is on; `N` stops it within
a second (back on: within a second for a connection that has one, else from the next one),
and once no connection has it on the capture loop's wait is the old one again (300 ms slices,
no stall reports).
Cost while on: per video write two clock reads and two short, uncontended locks (the estimator's),
an evaluation at most every 50 ms over at most 2 s of frames, and with the meter one
`SIO_TCP_INFO` call per 50 ms.

Inputs, per connection:

- every video frame write: when the capture loop queued it, when its write began and ended,
  its encoded bytes, its display. From the connection's video branch, or from the writer
  task when the session is split (`enable-split-session-io`);
- the TCP meter of the session socket, when `enable-tcp-meter` gives one (M11): a reading
  (`SIO_TCP_INFO`) at most every 50 ms, taken after a write, a stall report or the 1 s timer.
  With the estimator on, the meter samples even with `enable-latency-log` off;
- the capture loop: while any connection runs an estimator, the loop waits for slow
  connections in slices of 2 frame periods (20-300 ms) instead of 300 ms, and once it has
  waited max(3 frame periods, 50 ms) it reports the connections that have not taken the
  frame (`on_send_stall`) after every slice. With no estimator the wait is as before;
- once per second: the encoder's configured bitrate, fps, the floor and ceiling QoS allows
  (`min_ratio`, the latest quality), whether ABR is on (`in_vbr_state`).

What it computes, every 50 ms (on those events; a stall report always evaluates):

- **Units.** Rates are measured: `r_send` (bytes written into the socket per second, from the
  meter; unmetered: bytes the capture loop queued) and `r_acked` (bytes TCP delivered:
  `BytesOut - BytesRetrans - BytesInFlight` deltas; unmetered: bytes whose write ended), over
  500 ms. `k` is an EWMA of measured / configured bitrate over seconds in which the encoder
  ran at 80 % of its fps or more and used its budget: a sample above `k` comes in with weight
  0.5, one between 0.8 x `k` and `k` with 0.2; one under 0.8 x `k` is content-limited (typing,
  a static screen: the encoder undershoots) and is not learned from (`k_rej`); it only lets
  `k` drift down 1 % (never below the sample), so a real change of the coupling is followed
  slowly. While `k` drifts so the target is not clamped to the ceiling. A second in which the
  configured bitrate changed (a fast cut, another viewer's, the backstop) gives no sample at
  all: its rate belongs to two settings. The target is kept in
  measured kbps and
  `would_set_kbps = target / k` is the configured bitrate it would set. So nvenc's frame-rate
  coupling (30-fps budget at 60 fps) and the virtual framerate (M7) do not skew it.
- **Queue `Q`** (ms), the largest of: the newest frame's delay from queue to write end above
  its 10 s minimum (held for 200 ms; a frame over 2x the average size is allowed its excess
  bytes at the best delivery rate seen), the age of a write in progress, and with the meter
  `RTT - MinRTT` and the unsent backlog beyond one average frame at the delivery rate.
- **Threshold.** `T_hi` = clamp(2.5 x p95 of `Q` seen in NORMAL over 10 s, 30, 150) ms
  (60 until 20 samples), recomputed once per second; `T_lo` = `T_hi` / 3.
- **States** (`st=N/H/O/S/R` counts evaluations):
  - `S` SEVERE: a stall report of 200 ms or more (never for `ack=1` connections), once per
    stall episode: the capture loop reports the same unfetched frame after every slice, and
    the reports until this connection's next write ends are one episode (an outage of any
    length is halved once). When that write ends a HOLD starts, so the queue the episode left
    is not cut again; only fresh frames showing a shortfall after it cut. Only the capture
    loop's report triggers it, because a connection's own task is blocked in the write.
  - `R` RECEIVER: meter v1, the sender was receive-window limited more than half of the last
    100 ms while `RTT - MinRTT` < `T_lo`: the viewer does not read; never a cut.
  - `O` OVERUSE: the link is not paused, `Q` stayed above `T_hi` all through the last 100 ms
    (minimum of the step function; a write pending since before the window counts with its
    age), and the queue grows now or has stood above `T_hi` for 500 ms without draining
    (metered: `r_acked` over 1.05 x `r_send` drains it, a cut took effect, so a standing
    queue is not cut again on every hold while it goes; unmetered rates are equal by
    construction, unchanged). Grows: metered, TCP
    delivered under 0.95 x what was written over the last 100 ms; unmetered, a write was in
    progress for 80 % of the last 250 ms or more over at least 3 writes, or the last 3 or more
    frames' delays rose by 5 ms and ended above `T_lo`. Paused: metered, under 1500 bytes
    delivered in the last 100 ms; unmetered, one write blocked for 100 ms with no other one
    ending. A Wi-Fi airtime pause thus never cuts (a long one is SEVERE through the capture
    loop); a capacity drop does.
  - `N` NORMAL: p90 of `Q` over 500 ms below `T_lo`, no write older than `T_lo`.
  - `H` between the thresholds.
- **Decisions** (only with `elig=yes`):
  - cut on OVERUSE: max(floor, 0.5 x target, min(0.9 x target, 0.85 x `r_acked`));
  - severe: max(floor, 0.5 x target);
  - rto: after an RTO episode (meter), judged once on delivery and send from the meter
    reading that showed it on (never on the outage that caused it), over at least 300 ms and
    not while the link is paused: if delivery stayed under 0.85 x send, max(floor, 0.5 x
    target, 0.85 x that delivery). Otherwise, not judged within 1 s, seen within 500 ms after
    a cut (the same episode), or covered by a cut while watched, it is ignored (`rto_ign`);
  - after any cut a HOLD of max(250 ms, 2 x srtt) in which no other cut is made (`held`), so
    a stall report and the write's own end do not cut one queue twice. A cut of under 3 % (at
    the floor) is not made (`floor`);
  - increase: after 1 s in NORMAL, not app-limited (frames under 0.7 x fps, or `r_send` over
    1 s under 0.7 x target), at most every 500 ms: x1.08 per second below 0.9 x `C`, else
    +max(150 kbps, 2 %) per second, never above the ceiling. `C` (`c_kbps`) is the EWMA (0.3)
    of `r_acked` at cuts; OVERUSE within 10 s of reaching 0.9 x `C` lowers it by 10 %, and a
    target 15 % above `C` forgets it (the link got better). App-limited (light content, the
    link is not probed): increases only undo cuts, after 3 s in NORMAL and with frames in the
    last second, at most back to the target before the cuts and not past `C`, without
    reaching or forgetting `C` (`app_incs`). So a cut does not stay for as long as the content
    stays light (the TestDelay loop raised then too), and the target never climbs past what
    was in place before on unused budget. A quality change forgets the level to restore;
  - a user quality change sets the target to the new ceiling and forgets `C`.
- **Eligible** (`elig`): `yes`, or why not: `ctx` (no encoder context yet), `vbr` (ABR off or
  an encoder that cannot change bitrate), `ack` (the viewer acks frames, `video_ack_required`:
  the capture loop's wait includes the network round trip; its stall reports are counted but
  never SEVERE), `multi` (frames from more than one display in the last 2 s: one shared ratio,
  several encoders), `br` (no bitrate stored yet), `init` (target not set yet: after 1 s at
  full frame rate, or 3 s).

In shadow mode the encoder keeps the old controller's bitrate, so after a cut the estimator
does not see the effect of its own decision: under a lasting shortfall it keeps cutting every
HOLD down to the floor, and recovers from there. Read the first cut of an episode (`react_ms`)
and the false-cut rate, not the depth.

```
FASTABR shadow conn=5 ack=0
FASTABR shadow conn=5 kind=cut from_kbps=11755 to_kbps=5877 would_set_kbps=5877 k=1.00 q_ms=148.5 t_hi_ms=30 r_send=8000 r_acked=6104 react_ms=100 fc=0 meter=1 skipped=0
LATENCY fast conn=5 mode=shadow live=- elig=yes state=N meter=1 ack=0 k=0.97 set_kbps=4804 stream_kbps=4804 target_kbps=5120 would_set_kbps=5278 floor_kbps=1442 ceil_kbps=4804 c_kbps=- r_send_1s=4650 r_send=4700 r_acked=4690 app_lim=0 q=0.1/0.4/3.2 t_hi_ms=30 st=95/3/0/0/0 cuts=0 severe=0 rto=0 rto_ign=0 incs=4 held=0 floor=0 fc=0 stalls=0 react_max_ms=0 k_rej=0 applied=0 refused=0 resync=0 fps_cap=60/-
```

The `mode`, `live`, `stream_kbps`, `applied`, `refused`, `resync` and `fps_cap` fields, and
the live-mode lines, are described under "Host: fast bitrate adaptation live" below.

| Line / field | Meaning |
|---|---|
| `FASTABR shadow conn= ack=` | once when the estimator starts (`FASTABR live ...` with `enable-fast-abr=Y`); `... on` / `... off` when the options turn it on or off |
| `FASTABR shadow kind=` | always (not gated by `enable-latency-log`), at most one line per connection per 2 s (`skipped`: decisions not logged since the last line). `cut`, `severe`, `rto`; increases are only counted |
| `from_kbps` / `to_kbps` | target before and after, measured kbps |
| `would_set_kbps` | `to_kbps / k`: the encoder bitrate it would set |
| `k` | measured / configured bitrate |
| `q_ms` / `t_hi_ms` | queue at the decision, threshold |
| `r_send` / `r_acked` | over 500 ms, kbps (see Units) |
| `react_ms` | from the queue first passing `T_hi` (for a blocked write: its start + `T_hi`) to the decision. The roadmap's reaction-time gate: under 200 ms |
| `fc` | metered only: `1` when `r_acked` >= 0.95 x `r_send` at the cut, the roadmap's false-cut test (gate: at most 1 per 10 min). `-` unmetered: the lock-stepped capture loop makes both rates equal there, so the test says nothing |
| `meter` | the decision used the TCP meter's rates |
| `LATENCY fast` | every 5 s per connection with the estimator, with `enable-latency-log=Y` |
| `elig` / `state` | see above; `state` is the newest one. Live only, `displays`: more than one video service (display or camera) shares the ratio |
| `set_kbps` | the encoder's configured bitrate now (what the old controller set) |
| `target_kbps` / `would_set_kbps` | the estimator's target (measured) and what it would configure; `-` before `init` |
| `floor_kbps` / `ceil_kbps` | the configured range QoS allows |
| `c_kbps` | learned ceiling `C` (measured), `-` when none |
| `r_send_1s` | send rate over the last second; `r_send` / `r_acked` the newest 500 ms values |
| `app_lim` | 1: the stream does not use its target (static screen, encoder undershoot): increases only undo cuts (after 3 s in NORMAL with frames sent, at most to the target before the cuts and `c_kbps`) |
| `q` | `Q` at every evaluation, p50/p95/max ms |
| `st` | evaluations per state N/H/O/S/R in the window |
| `cuts` / `severe` / `rto` / `rto_ign` / `incs` / `held` / `floor` / `fc` | decisions and skipped cuts in the window (see Decisions) |
| `app_incs` | of `incs`, the ones made while app-limited (they neither reach nor forget `C`) |
| `stalls` | capture-loop stall reports for this connection |
| `k_rej` | full-rate seconds whose `k` sample was content-limited (under 0.8 x `k`): not learned from |
| `react_max_ms` | largest `react_ms` in the window |

Tests (`cargo test latency_fast`): replays of recorded `LATENCY server` windows
(`src/server/video_qos/tests/fast_traces.txt`: 10 min Wi-Fi at 60 fps, 12 min Wi-Fi at 15-30
fps, 5 min LAN; each window's frame count and send/wait percentiles become frame writes, so
these runs are unmetered), and a small fluid model (one sender buffer draining into a link)
for a capacity step (metered: `react_ms` at most 200, first cut at most 300 ms after the step;
unmetered: after the send buffer fills), 150 ms airtime pauses every 2 s for 10 min (at most
1 cut), a 1 s outage (one SEVERE), recovery (at most 8 %/s), receive-window limiting, RTO
(a 400 ms pause with an RTO in it and full delivery after: no cut), `ack` and multi-display;
unit tests for one SEVERE per stall episode, the RTO judged on delivery after it, and `k`
ignoring content-limited seconds. The model is not a TCP model. The thresholds are to be
calibrated from the `LATENCY fast` / `FASTABR` lines of a week of normal use (roadmap gate),
with the meter on where possible (`enable-tcp-meter`: without it the send buffer hides the
queue until it is full). The meter's `unsent` is still unverified on the real link (see TCP
meter above); the metered backlog term depends on it.

Rollback: `enable-fast-abr-shadow N`.

### Host: fast bitrate adaptation live (roadmap M16)

Server option `enable-fast-abr`: off unless `Y`, on every platform (the estimator works
without the TCP meter too; the meter only exists on Windows). Read every second by each
connection that runs an estimator: a connection authorized while `enable-fast-abr=Y` or
`enable-fast-abr-shadow` is on (on Windows the shadow option is on by default, so every
connection has one); `N` or empty hands the bitrate back to the TestDelay controller within a
second. Stock viewers need nothing: the estimator only reads the host's own writes, the TCP
meter and the capture loop. The viewer part (`enable-fps-cap-split`) is in "Viewer: fps
control".

What changes with `Y` (`src/server/video_qos.rs` `on_fast`, `src/server/video_qos/fast.rs`):

- **Reports.** After every decision, and once a second without one, a connection's estimator
  reports to the stream its target in configured kbps (`would_set_kbps`; none while shadow,
  off or not eligible) and the decision. Kbps and ratio convert through the encoder's own
  bitrate curve: hwcodec RAM and VRAM encoders (NVENC, AMF, QSV) compute their bitrate
  non-linearly from the ratio (`HwRamEncoder::calc_bitrate`: x2 for H.264, x1.5 for H.265 up
  to 2000 kbps of base x ratio, decaying toward x1 above), so the video service hands QoS
  that curve and a kbps target is inverted on it by bisection. The curve is used only while
  it reproduces the bitrate the encoder was last set to at its ratio (within 1 %); otherwise,
  and for vpx/aom (linear), the kbps per unit of ratio come from that last set point.
  A connection with a target is "fast"; one without, or one the stream cannot take (`vbr`:
  ABR off or an encoder that cannot change bitrate; `displays`: several video services share
  the ratio; `br`: no bitrate yet; `epoch`: a quality choice since its last context), drops
  out of the fast set and is left to the TestDelay loop.
- **Cut** (`cut`, `severe`, `rto`): the stream's ratio comes down to this connection's target
  at once (`check_qos` gives it to the encoder before the next frame). A target at or above
  the stream changes nothing (`lower`); one at the floor neither (`floor`).
- **Increase**: the ratio rises by the decision's factor (x1.08 per s, or the additive step
  near the learned ceiling), but never above the lowest target of the fast connections (the
  "min" policy: one slow viewer holds the stream; refused `min`), at most every 100 ms over
  all connections (`spacing`), and not while any viewer's TestDelay replies average 150 ms of
  excess or more (`delay`; as the old loop's own increases. A viewer without an estimator,
  for example a stock web client acking frames, has no other voice, and a fast one's replies
  may see a queue past the first hop, as over a relay, that its estimator does not).
- **Following the stream.** Once a second each estimator lowers its target to what the stream
  has (`stream_kbps` x `k`) when the stream is more than 3 % lower: another viewer's lower
  target, a TestDelay backstop cut, or its own refused increase (`resync` counts these). So it
  raises from the bitrate the encoder really has, not from its own past target.
- **Quality choice.** Every image-quality message (also a repeated one) bumps an epoch and
  drops every target: the ratio jumps to the new quality as before, reports made before it
  are void (`epoch`), and each estimator's next second sets its target to the new ceiling
  and forgets the learned ceiling `C`.
- **TestDelay loop beside it**, while any connection is fast: it makes no increase (both
  call sites, the 3 s periodic adjustment and the reply path); its cuts stay as a backstop but
  are skipped for 2 s after a fast cut (one queue is not cut twice), with no cooldown started,
  so a confirmation after that cuts at once (logged `FASTABR backstop cut`).
- **FPS.** A fast connection's delay replies lower its fps only at the bitrate floor
  (`min_ratio`, within 2 %) or for a reply with 1 s of excess or more; while the bitrate can
  still come down, the estimator answers the delay. The probe-timeout brake (a probe
  outstanding over 2 s) is unchanged, and so are fps increases and the viewers' fps caps
  (`custom_fps`, `auto_adjust_fps`: the stream runs at the lowest cap, as before; `fps_cap`).
- **M7 virtual framerate.** With `nvenc-nominal-fps` the encoder's real bitrate follows the
  configured one at any fps, so `k` is about 1 and an fps change does not move the bitrate.
  Without it nvenc's real bitrate scales with fps / 30; `k` absorbs it (learned over
  full-rate seconds), so the estimator still converts in measured units, only slower after an
  fps change.
- **Several viewers.** Each connection runs its own estimator; the stream takes the lowest
  target ("min"). With `enable-primary-viewer-rule=Y` only the primary viewers' targets count
  (refused `secondary`); see "Host: several viewers, primary viewer rule (roadmap M18)".

Cost: a report locks `VIDEO_QOS` once per decision (at most every 50 ms per connection, in
practice a few per second) and once per second; nothing per frame.

```
FASTABR live conn=5 ack=0
FASTABR live conn=5 kind=cut from_kbps=8120 to_kbps=6730 would_set_kbps=6801 k=0.99 q_ms=61.0 t_hi_ms=30 r_send=7900 r_acked=7918 react_ms=110 fc=1 meter=1 skipped=0 ratio=0.670->0.561
FASTABR applied conn=5 display=… ratio=0.561 kbps=6801 reaction_ms=126 unlogged=0
FASTABR backstop cut ratio=0.561->0.393
LATENCY fast conn=5 mode=live live=on elig=yes state=N … stream_kbps=6801 … applied=3 refused=1 resync=0 fps_cap=60/54
```

| Line / field | Meaning |
|---|---|
| `FASTABR live conn= mode` / `FASTABR shadow conn= mode` | the option changed between shadow and live for this connection |
| `FASTABR live kind=… ratio=a->b` | the decision line (same fields as in shadow, same 2 s limit) with what the stream did: `ratio=` the ratio before and after, `refused=` why nothing changed (`lower`, `floor`, `min`, `spacing`, `delay`, `epoch`, `displays`, `vbr`, `br`, `user`, `secondary`: not a primary viewer, M18) |
| `FASTABR applied` | `check_qos` gave a fast cut to the encoder: `ratio`, the encoder's `kbps`, `reaction_ms` from the queue first passing `T_hi` (as `react_ms`) to here, the roadmap's reaction time (gate: 120-200 ms). At most one line per second; `unlogged` counts the cuts since the last line |
| `FASTABR backstop cut` | the TestDelay loop cut while a connection is fast (outside the 2 s after a fast cut) |
| `mode` | `live` or `shadow` (in the `LATENCY fast` line) |
| `live` | what the stream did with the last report: `on` (this connection's target bounds the stream), the reason it does not (`displays`, `vbr`, `br`, `epoch`, `ack`, `multi`, `init`, `ctx`, `off`), `-` in shadow mode |
| `stream_kbps` | the configured bitrate the stream's ratio stands for now (`set_kbps` is the encoder's, up to a frame behind) |
| `applied` / `refused` | this connection's decisions the stream applied / did not apply in the window |
| `resync` | seconds the target came down to the stream's bitrate |
| `fps_cap` | this viewer's `custom_fps` / `auto_adjust_fps` caps as the host holds them, `-` when not sent. A fork viewer with `enable-fps-cap-split` sends its decode limit as the second one |

Tests (`cargo test latency_fast_live`): `on_fast` (cut at once, increases bounded by the
lowest target and spaced, no increase while a viewer shows delay, the TestDelay loop as
backstop with the 2 s suppression and the fps hold, quality epoch, refusals), the estimator's
report and its following of a lower stream, and a closed loop on the fluid model where the
encoder follows the estimator (4 Mbps step under 8 Mbps: first cut within 300 ms, at most 4
cuts, never under 2 Mbps; back at 8 Mbps within 25 s after the step ends).

Gate (roadmap M16): a capacity step with `New-NetQosPolicy -PolicyStore ActiveStore` (at least
2 Mbps, removed twice; check that it acts on the live connection), shadow vs live in the same
session: `reaction_ms`, peak `wait`/`send` and recovery time improve; on Wi-Fi, cuts per minute
no more than in shadow mode.

Rollback: `enable-fast-abr` empty or `N` (shadow again), or also `enable-fast-abr-shadow N`.

### Host: several viewers, primary viewer rule (roadmap M18)

Server option `enable-primary-viewer-rule`: Windows hosts, off unless `Y` (other platforms
ignore it). Read once per second by the QoS tick (`VideoQoS::tick_primary`, run from each
capture loop's per-second `update_display_data`); `N` or empty restores the "min over all
viewers" rule within a second. Stock viewers need nothing: the rule uses only what the host
already sees (their input, their writes, their TestDelay replies). Code:
`src/server/video_qos/primary.rs` (gate, policy), `src/server/video_qos.rs` (`apply_primary`
and the filters below), `src/server/video_service.rs` (wait, key frames),
`src/server/key_refresh.rs` (`on_lag_key`), `src/server/connection.rs` (gate, input, control).

- **Primary set**, every second: the connections with control (keyboard permission on a
  remote-control session: not view-only, not camera) whose viewer sent mouse, key or touch
  input the host took within the last 30 s. When none qualifies, every connection is primary
  and the rule changes nothing but the fps floor below. A new connection is not primary while
  another one is primary by input (its first second's start-up guard does not hold the
  stream at 15 fps).
- **What only primary viewers steer:** the stream's fps (the minimum of their delay-driven fps,
  their start-up guard) and fps cap (their `custom_fps` / `auto_adjust_fps`); the TestDelay
  ratio loop (their delay and confirmed reductions; a secondary viewer's reply never triggers
  a cut); fast mode (M16) cuts and increases (a secondary viewer's estimator is refused
  `secondary`, and its target does not bound increases); the quality: the latest image-quality
  choice among the primary viewers (a secondary viewer's choice applies only when no primary
  viewer made one; when the primary set changes the quality, the ratio jumps to it as on a
  quality choice). A secondary viewer's own fps cap is not enforced per connection: it gets
  the stream's frames, bounded by the LAGGING queue below.
- **Fps floor for viewer requests** (roadmap open question 5; the same floor as the viewer's
  gentle fps control, M5): while the rule is on, a `custom_fps` lower than the one the viewer
  logged in with (stock viewers halve it under decode load) counts as at least min(login
  value, 24). Only the login options' `custom_fps` is the reference (a viewer sends one there
  only at "custom" image quality); without one it is 30, so a Balanced-quality stock viewer's
  first reduction (15) counts as 24. This protects the other viewers from a loaded stock viewer even when it is
  primary, at the cost of sending that viewer more frames than it decodes. A user who lowers
  the fps on a stock viewer mid-session gets the same floor. `auto_adjust_fps` (a fork
  viewer's decode limit with `enable-fps-cap-split`) is taken as it is.
- **LAGGING**, only while someone is primary by input, only for secondary connections: a
  second counts as behind when the connection's video queue reached 3 unwritten frames of a
  display at some enqueue, it dropped frames, its TestDelay probe has been out past 2 s, its
  replies confirm a queue (the same confirmation that cuts the bitrate for a primary viewer),
  or its fast estimator's target is below 0.6 x the stream's. 2 behind seconds in a row make
  it LAGGING; 5 clean seconds in a row end it.
- **Left behind.** The capture loop waits for a LAGGING connection's fetch of a frame at most
  one frame period (`wait_done`: it waits as before for everyone else, then gives the LAGGING
  ones the rest of `spf`), so one slow viewer no longer holds capture for up to 3 s.
- **Bounded queue, drop to key frame** (`VideoGate::on_enqueue`, on the capture thread): when a
  frame of a display is enqueued for a LAGGING connection that still has 3 unwritten frames of
  that display, that frame and every following delta frame of the display are dropped for
  this connection until a key frame, which passes and ends the run. A dropped frame counts as
  fetched at once. Works with and without split session I/O (M12): the count drops when the
  frame is written, by the connection loop or the writer task.
- **Key frames.** While any connection drops to a key frame of a display, its capture loop
  forces one (M13 forced key frame, kept image on a static screen) at most once per 3 s
  (`LAG_KEY_INTERVAL`), and not within 200 ms after another key frame or while one is
  requested. This works without `enable-refresh-keyframe` (which governs viewer refreshes
  only); these key frames do not count toward that option's escalation to `SWITCH`, nor as
  refresh key frames in the `LATENCY` counters. An encoder that refuses a forced key frame is
  restarted once (`SWITCH` reason `lag_key_unsupported`; its first frame is a key frame) and
  that display stops dropping and leaving LAGGING connections behind, and is never asked for
  a lag key frame again, until a viewer refresh's forced key frame (`enable-refresh-keyframe`)
  is accepted on it, else until the host process restarts. The shared encoder means every
  viewer of the display gets these key frames. Only a connection the loop still sends to
  counts: one that switched to another display (or closed this one) keeps its drop run until
  it gets a key frame of the display or goes, but no longer makes the loop force key frames.
- **Monitors only.** View-camera connections have no gate (their frames carry the camera index
  as `display`) and camera capture loops neither leave connections behind nor force lag key
  frames.
- **Displays.** A gate tracks displays 0-7; frames of higher display indexes are never dropped
  and their LAGGING connections are waited for as before.

Cost with the rule off: per video frame and connection one atomic flag test plus one atomic
add and max (queue count) on the capture thread and one atomic subtract after the write; per
input event one `Instant::now` and an atomic store; once a second a `Config` read. With the
rule on, also once a second a lock of the gate registry; per frame a check of the LAGGING set
(a copy is refreshed only when it changes); while some connection drops to a key frame of a
display, once the 3 s interval is over, a lock of the gate registry per frame of that display
until the key frame is requested. The key flag of a frame is only looked at for a
LAGGING or dropping connection.

```
PRIMARY rule=on primary=[5] secondary=[7] lagging=[] fps=60 ratio=0.670 quality=Balanced
PRIMARY rule=on primary=[5] secondary=[7] lagging=[7] fps=60 ratio=0.670 quality=Balanced
PRIMARY key display=0 dropping=1
PRIMARY drops conn=7 frames=212 runs=4 peak_queue=3 lagging=true
PRIMARY rule=off fps=30 ratio=0.670 quality=Balanced
PRIMARY display=0 encoder has no forced key frame: lagging viewers are no longer dropped to key frames
PRIMARY display=0 encoder takes forced key frames again
```

| Line / field | Meaning |
|---|---|
| `PRIMARY rule=` | logged when the rule, the primary set or the LAGGING set changes (always, not gated by `enable-latency-log`) |
| `primary=` / `secondary=` | connection ids that steer the stream / that do not |
| `lagging=` | the LAGGING connections (left behind by the capture loop, dropped to key frames) |
| `fps` / `ratio` / `quality` | the stream's fps, bitrate ratio and quality after the change |
| `PRIMARY key display= dropping=` | the capture loop forced a key frame for connections dropping to one; `dropping` how many of the connections it sends to wait for it (at most one line per 3 s per display) |
| `PRIMARY drops conn=` | at most every 10 s, per secondary connection that dropped frames since the last line: `frames` dropped, `runs` drop-to-key runs started, `peak_queue` the highest number of unwritten frames seen at an enqueue, `lagging` its state now |
| `PRIMARY display= encoder has no forced key frame` | warning, once per display: no drop-to-key there any more (until the next line) |
| `PRIMARY display= encoder takes forced key frames again` | a viewer refresh's forced key frame was accepted on a display marked above: drop-to-key is back on there |

Tests (`cargo test latency_primary`): the gate (drop run to the next key frame, key frames pass
a full queue, no drop unless LAGGING, counts released when a connection goes, no underflow),
the primary set, the wait's one-frame grace, LAGGING hysteresis, the fps floor; `VideoQoS`
with a primary and a secondary viewer (the secondary's delay and cap do not lower the fps, its
delay does not cut the ratio, its quality choice waits, its fast cut is refused, a new viewer
is not primary, rule off keeps the minimum); lag key frames never escalate a refresh.

Gate (roadmap M18): Dell (fork, controlling) and Quest (stock) at the same time, only the
Quest's path throttled (`New-NetQosPolicy ... -IPDstPrefixMatchCondition <Quest IP>`): the
Dell's fps and `total_to_ui` must not change with the rule on; the Quest's key frames at most
one per 3 s (`PRIMARY key` lines, Quest `LATENCY` key counts).

Rollback: `enable-primary-viewer-rule` empty or `N`.

### Host: GPU scheduling priority (roadmap M17)

Server options, Windows hosts only, both off unless `Y`, read when a capture loop starts (a
change applies from the next capture restart, e.g. a reconnect or a codec/display change):

- `enable-host-gpu-priority`: while a capture runs, the server process enables
  `SeIncreaseBasePriorityPrivilege` on its token and sets its GPU scheduling class with
  `D3DKMTSetProcessSchedulingPriorityClass` to HIGH (all of its GPU contexts: capture copies,
  the VRAM conversion, CUDA/NVENC), falling back to ABOVE_NORMAL when HIGH is refused; never
  REALTIME. The first capture loop raises it, the last one to end restores the class it found.
  It also calls `IDXGIDevice::SetGPUThreadPriority(7)` on the capture device (the VRAM encoders
  share it) and tells hwcodec to do the same on D3D11 devices encoders open themselves (FFmpeg's
  D3D11VA upload device of a RAM nvenc encoder; fork API, `docs/patches/README.md`).
  Independent of `enable-host-priority-boost` (CPU side: process class, MMCSS "Capture").
- `enable-host-dwm-mmcss`: `DwmEnableMMCSS(TRUE)` while a capture runs, `FALSE` when the last
  one ends. Separate so it can be A/B tested alone; the roadmap keeps it only if it measurably
  helps (it may be a no-op on Windows 11).

Code: `src/platform/windows_gpu_priority.rs`, `libs/scrap/src/dxgi/mod.rs`
(`set_gpu_thread_priority`), `src/server/video_service.rs` (guard before the encoder opens),
hwcodec `cpp/common/platform/win/win.cpp` (`ApplyGpuThreadPriority`).

```
LATENCY server gpu_priority display=0 process=high privilege=ok capture=7:0x00000000 encoder=7 hags=on dwm_mmcss=off
LATENCY server gpu_priority display=0 off
LATENCY server gpu_priority released process=restored:normal dwm_mmcss=-
gpu_priority encoder=hevc_nvenc thread=7 hr=0x00000000
```

| Field | Meaning |
|---|---|
| `display` | the capture loop's display index |
| `off` | both options off: nothing changed (hwcodec is told 0, its own devices stay at their default). Logged only with `enable-latency-log=Y`, so a host with every option at its default logs as before; the other `gpu_priority` lines are always logged |
| `process` | the D3DKMT class: `high` (raised), `above_normal(high:<NTSTATUS>)` (HIGH refused, e.g. `0xc0000061` privilege not held; ABOVE_NORMAL set), `failed(...)` (both refused, class unchanged), `kept:<class>` (already HIGH or more, unchanged and not restored), `held` (an earlier capture loop of this process already raised it), `unavailable` (gdi32 lacks the D3DKMT exports), `off` (only `enable-host-dwm-mmcss` is on) |
| `privilege` | `SeIncreaseBasePriorityPrivilege`: `ok`, `not_held` (the token lacks it: a portable instance under a normal user), `failed(<HRESULT>)`, `-` (not tried: `held` or `off`) |
| `capture` | `7:<HRESULT>` from `SetGPUThreadPriority(7)` on the capture device, `n/a` for capturers that do not capture through a D3D device in this process (GDI fallback, also after DXGI recovery ran out or with DirectX capture turned off by option; magnifier, portable service, camera), `off` |
| `encoder` | the GPU thread priority hwcodec gives encoder-owned D3D11 devices opened from now on (7 or 0). Each such device logs its own `gpu_priority encoder=<name> thread=7 hr=<HRESULT>` line (hwcodec log, no `LATENCY` prefix); none when the encoder uses the capture device (VRAM path) |
| `hags` | hardware-accelerated GPU scheduling from `HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers` `HwSchMode`: `on` (2), `off` (1), `default` (no value: the system and driver default applies; check Settings > Display > Graphics), `?` |
| `dwm_mmcss` | `DwmEnableMMCSS(TRUE)` HRESULT, `held` (already on in this process), `unavailable`, `off` |
| `released` | the last capture loop with the guard ended: `process=restored:<class>` / `restore_failed(<NTSTATUS>)` / `-` (nothing was raised), `dwm_mmcss=<HRESULT of FALSE>` / `-` |

Cost: a few system calls and one registry read per capture start; nothing per frame.

Gate (roadmap M17), on Alza with a GPU-bound game at about 99% GPU (the game's own frame
counter visible), a 60 fps session from the Dell over LAN, 2-3 minutes per run, the same scene:

1. `enable-host-gpu-priority` empty (baseline), reconnect, play; then `Y`, reconnect, play.
   Compare from the `LATENCY server` lines (filter by `gpuprio=`: `off` against `gpu`): `encode` p95/max, `grab`
   p95, `age` p95 and `fps`. Target: encode p95 from the measured 10-16 ms toward ~3 ms
   (estimate). The game's fps must not drop materially (more than about 5%).
2. Check the `gpu_priority` line: `process=high` and `capture=7:0x00000000`. With
   `above_normal(...)` or `privilege=not_held` the run measures less than intended; with
   `gpuprio=gpu-failed` nothing took effect, so the run is not a GPU-priority run. `capture=n/a`
   means the capture ran through GDI (the `gdi: true` line): that run measures a different
   capture path, repeat it with DXGI capture.
3. Only then `enable-host-dwm-mmcss=Y` on top (same comparison, `gpu` against `gpu+dwm`).
   Without a measurable gain, leave it off. A DWM-only run is labelled `dwm`.
4. Note `hags=`: with HAGS on, Sunshine uses REALTIME for NVENC; this build never does. If HIGH
   shows no effect with HAGS on, a run with HAGS off (owner's setting, reboot) tells whether
   HAGS is the reason.

Rollback: `enable-host-gpu-priority` (and `enable-host-dwm-mmcss`) empty or `N`, then
reconnect; the last capture loop restores the process class.

### Host and viewer: KCP send window (roadmap M15)

Server option `kcp-sndwnd`, empty by default: a number of segments, clamped to 32-1024, for
KCP sessions made after it is set (each end reads its own option; it is sender-side and needs
no negotiation). KCP's turbo profile uses 1024, and its endpoint stops taking data while
more than 2 x the window is waiting, so up to about 2048 segments (about 2.4 MB) plus the
endpoint's 128-entry send channel can queue in front of the link. A smaller window bounds that
queue (64: about 180 KB), and also caps KCP's throughput at window x MTU per RTT (64 at 50 ms:
about 14 Mbps). Works with `allow-kcp-congestion-control` or alone. Not done: the bound of the
128-entry channel and a `kcp_backlog` field need a kcp-sys change (fork). Rollback: empty.

### UDP video lane (shadow probe)

Roadmap M19, design and negotiation in docs/UDP-VIDEO.md. Off by default: host server option
`enable-udp-video` `shadow` (or `Y`, the value M19 used, which keeps meaning shadow; live mode
is `live` since the M20 review, see the next section);
the viewer option `enable-udp-video` is on by default and only acts when a host offers. The
video still goes over TCP; every Nth frame (`udp-video-shadow-every`, default 2) is also sent
over UDP, and the viewer only measures it. These lines are logged whenever a lane exists,
independent of `enable-latency-log` (the lane exists only to measure). Since M20 the window
lines carry `mode=shadow` after the lane id.

```
UDPVIDEO host conn=5 not offered (viewer v1): host option enable-udp-video is "" (live: live, shadow or Y: shadow)
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

Host server option `enable-udp-video` `live` (off by default; `Y` is shadow) and a viewer that
advertises lane version 3 (the first M20 viewer, 2, gets no live offer; its host's live offers
are refused by this viewer): video frames go over UDP, everything else stays on TCP; any
trouble falls back to TCP video in the same session (docs/UDP-VIDEO.md, "Live mode"). Logged
whenever a lane exists, like the shadow lines.

```
UDPVIDEO host conn=5 offer lane=3f2a… mode=live addr=192.168.1.5:61234 every=2 burst=8 timeout_ms=1000 viewer=192.168.1.9 installed=true
UDPVIDEO host conn=5 live: a loss is repaired by a forced key frame (enable-refresh-keyframe=Y)
UDPVIDEO host conn=5 lane=3f2a… pacing burst=8 hires_timer=true
UDPVIDEO host conn=5 qWAVE AudioVideo flow added
UDPVIDEO host conn=5 active lane=3f2a… mode=live peer=192.168.1.9:50511 every=2 after_ms=41
UDPVIDEO host conn=5 lane=3f2a… mode=live win_s=5.0 frames=300 keys=1 skipped=0 qdrop=0 dgrams=4410 kB=5900 send_ms=0.9/3.1/8.0 lag_ms=0.1/0.2/0.6 err=0 pings=25 refused=0 paced_ms=0.4/2.6/7.6 win_wait_ms=2.0/9.5/31.0 win_to=0 inflight_max=4 acks=410 ack_rtt_ms=2.2/4.9/12.1 base_ms=1.6 vlost=0 kf_req=0 tail_kf=0
UDPVIDEO host conn=5 stop lane=3f2a… mode=live: sustained loss: 4 key frames in 5 s (last: request for display 0 frames 812..=812) (in_band=true displays=[0])
UDPVIDEO host conn=5 video back on TCP, next offer in 30 s
UDPVIDEO host conn=5 retry 1 of 2
UDPVIDEO viewer peer=123456789 accepted lane=3f2a… mode=live host=192.168.1.5:61234 local=192.168.1.9:50511 every=2 timeout_ms=1000
UDPVIDEO viewer peer=123456789 live lane=3f2a… epoch=0 held=1 closed=false
UDPVIDEO viewer peer=123456789 lane=3f2a… mode=live win_s=5.0 frames=300 lost=0 gated=0 frag_miss=0 dgrams=4535/4535 loss_pct=0.00 reorder=0/0 dup=0 late=0 bad=0 drop=0/0/0/0 shown=300 held=1/1 stale=0 gated_app=0 kf_req=0 lost_fixed=0 tcp_drop=0 owd_var_ms=0.4/2.0/8.1 asm_ms=0.6/2.9/7.9 rtt_ms=2.0/3.6/6.6 pings=25/25 acks=410 rx_err=0
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
| `kf_req` | key frame requests (`UdpVideoFeedback`) the viewer sent over TCP |
| `tail_kf` | key frames the host asked for itself on a tail loss: a display's newest frame (sent, or dropped from the full queue) not settled by the ACKs 200 ms (plus `base_ms`) after it went out, with nothing newer sent, so no later frame could show the viewer the gap (one small frame lost on a static screen). Once per newest frame id. `kf_req` and `tail_kf` together, 4 within 5 s, end the lane (sustained loss) |

The `stop` line gives the reason (handshake timeout, `no datagram from the viewer in N ms`,
`N datagram sends failed in a row`, `sustained loss` with the last request or `tail losses`,
`viewer asked: …`), `in_band` (the STOP
marker waits for the next video item, so it comes right before the first TCP frame) and the
displays that get a key frame for their restart on TCP. After a live lane ends the host offers
again after 30 s and 120 s, then stays on TCP.

Viewer window line, live (`mode=live`): the receive-side fields as in the shadow line (`lost`
here: 80 ms after a frame's first fragment, above the pacer's 25 ms longest spread of a frame,
or 5 ms after a later frame completed), plus:

| Field | Meaning |
|---|---|
| `gated` | complete frames the receive thread dropped after a loss because they were not key frames (their reference is missing) |
| `shown` | frames handed to the decoder path (decoder queue, pacer, fps control, latency stats, as TCP frames are) |
| `held` | frames held for the ACTIVE marker or the SwitchDisplay of their epoch / most held at once. Only a SwitchDisplay right after an EPOCH marker starts an epoch; the ones the host sends outside the video stream (display switch answer, message query) do not |
| `stale` | frames of an epoch before a SwitchDisplay already read, dropped |
| `gated_app` | frames the io_loop dropped while the display waited for a key frame (a held frame lost to the 32-frame hold limit, or a loss it had already seen) |
| `kf_req` | key frame requests sent: one when a display breaks (a loss, or a held frame dropped by the hold limit), then one every 250 ms while no key frame came. Losses while a display waits ask nothing |
| `lost_fixed` | losses a key frame completed in the same poll right behind them repaired (the host's tail loss key frame, typically): no request |
| `tcp_drop` | TCP frames dropped after the lane ended until a key frame, for displays whose last UDP frames did not all arrive |
| `acks` | ACK datagrams sent |

The viewer asks the host to stop the lane (`request stop`) when a display has no key frame 1 s
after a loss, frames wait 1 s for ACTIVE or for the SwitchDisplay of their epoch, frames arrive
for 1 s but only of an ended epoch (none shown), the host is silent for `timeout_ms`, or 5
receive errors come in a row (the watchdog runs on the 1 s status tick, so it acts after 1-2 s). With a live lane the viewer's `LATENCY client` fields keep their meaning:
UDP frames enter the same handling as TCP frames; the session `kB/s` counts both.

### UDP video lane (FEC, M21)

Host server option `udp-video-fec-percent` (default 20, `0` = no parity) and a viewer that
advertises lane version 4: every frame carries Reed-Solomon parity (at least 2 per block of at
most 200 data shards, at most 4 blocks per frame), live and shadow lanes alike
(docs/UDP-VIDEO.md, "FEC (M21)"). The fields below are added to the lines of the two sections
above.

```
UDPVIDEO host conn=5 offer lane=3f2a… mode=live addr=192.168.1.5:61234 every=2 burst=8 timeout_ms=1000 fec=20 viewer=192.168.1.9 installed=true
UDPVIDEO host conn=5 lane=3f2a… mode=live win_s=5.0 frames=300 keys=1 skipped=0 qdrop=0 dgrams=5320 kB=7050 send_ms=1.0/3.4/8.6 lag_ms=0.1/0.2/0.6 err=0 pings=25 refused=0 fec_frames=300 fec_dgrams=910 fec_off=0 fec_ms=0.01/0.03/0.40 paced_ms=… vlost=0 kf_req=0 tail_kf=0
UDPVIDEO viewer peer=123456789 accepted lane=3f2a… mode=live host=192.168.1.5:61234 local=192.168.1.9:50511 every=2 timeout_ms=1000 fec=20
UDPVIDEO viewer peer=123456789 lane=3f2a… mode=live win_s=5.0 frames=300 lost=0 gated=0 frag_miss=0 dgrams=5402/5445 loss_pct=0.79 … rx_err=0 fec_rec=21 fec_unrec=0 fec_shards=37 parity=880
```

Host: `fec` in the `offer` line is the parity percentage offered (0: none, also for a viewer
below lane v4). The window line gets, while the lane sends parity:

| Field | Meaning |
|---|---|
| `fec_frames` | frames sent with parity |
| `fec_dgrams` | parity datagrams among `dgrams` (`kB` includes them) |
| `fec_off` | frames sent without parity because they are larger than 4 blocks of 254 fragments (about 1.37 MB) |
| `fec_ms` | p50/p95/max of the parity encoding time per protected frame (part of `send_ms`). Each block's parity is encoded after its data shards were handed to the socket, so it delays later blocks and the parity, never the first datagram or `send_us` |

Viewer: `fec` in the `accepted` line is the host's percentage. Both window lines (shadow and
live) end with:

| Field | Meaning |
|---|---|
| `fec_rec` | delivered frames that completed only because lost data fragments were rebuilt from parity: losses repaired on arrival, with no key frame, request or wait. Counted on delivery: a rebuilt frame that is then gated behind an earlier loss counts in `gated` only. A data fragment merely reordered behind the parity that rebuilt it also counts here (the frame completed early) and then as `dup` |
| `fec_unrec` | frames declared lost although at least one of their shards arrived and carried FEC: a block missed more shards than parity arrived. Part of `lost`; a frame of which nothing arrived counts in `lost` only |
| `fec_shards` | data fragments rebuilt |
| `parity` | parity datagrams received, also those of frames that needed none (they arrive after the frame was delivered and are only counted) |

`dgrams` and `loss_pct` count parity datagrams like any other, so they are the raw datagram
loss; `lost`, `frag_miss` and `kf_req` are what was left after FEC. Roadmap gate: with the same
loss pattern as an M20 run, fewer `lost` and more `fec_rec`.

### UDP video lane (RFI, M22)

Host server option `enable-nvenc-rfi=Y` (off by default), a viewer that advertises lane version
5, an NVIDIA GPU and HEVC: a lost frame is repaired by reference frame invalidation instead of a
key frame where possible (docs/UDP-VIDEO.md, "RFI (M22)"). The fields below are added to the
lines of the live sections above and to `LATENCY server` (`rfi`, `rfi_idr`, `rfi_kept`,
`rfi_max`, see "Host").

```
UDPVIDEO host conn=5 offer lane=3f2a… mode=live addr=192.168.1.5:61234 every=2 burst=8 timeout_ms=1000 fec=20 rfi=true viewer=192.168.1.9 installed=true
UDPVIDEO host conn=5 live: a loss is repaired by reference frame invalidation where the encoder can (enable-nvenc-rfi=Y), else a forced key frame (enable-refresh-keyframe=Y)
[NVENC] caps hevc: rfi=1 ir=1 ss_ir=1 multiref=1 h264: rfi=1 ir=1 ss_ir=1 multiref=1; hevc encoder rfi=on dpb=5 numRefL0=1
UDPVIDEO host conn=5 lane=3f2a… mode=live win_s=5.0 frames=300 … vlost=1 kf_req=1 tail_kf=0 rfi_req=1 rp=1
UDPVIDEO viewer peer=123456789 lane=3f2a… mode=live win_s=5.0 frames=300 lost=1 gated=2 … kf_req=1 lost_fixed=0 tcp_drop=0 rp=1 fix_ms=31.2/31.2/31.2 owd_var_ms=…
LATENCY server display=0 fps=59.8 … key=0 keymax=0 … rfi=1 rfi_idr=0 rfi_kept=0 rfi_max=41220 qos_sig=udp
```

(The caps values above are an example; the first run on the RTX 5060 Ti logs the real ones.)

Host: `rfi` in the `offer` line says the lane takes recovery points (option on, viewer lane v5,
live mode). The window line gets:

| Field | Meaning |
|---|---|
| `rfi_req` | loss reports (part of `kf_req`) turned into an RFI request to the capture loop: the lane still knew the first lost frame's pts (the last 128 frames since the display's last key frame). The rest asked for a key frame as before; the capture loop may still answer an RFI request with a key frame (`rfi_idr`) |
| `rp` | frames sent to this viewer flagged `RECOVERY_POINT`: a recovery frame whose invalidation started at or before this viewer's first lost frame. Another viewer's recovery frame is not flagged here (it may reference a frame this viewer lost) |

`kf_req` and `tail_kf` still count toward sustained loss (4 within 5 s end the lane), RFI or not.

Viewer window line (`mode=live`) adds:

| Field | Meaning |
|---|---|
| `rp` | broken displays that resumed at a `RECOVERY_POINT` frame instead of a key frame |
| `fix_ms` | p50/p95/max from a display breaking (the loss reached the io_loop) to the frame that resumed it, key frame or recovery point: the recovery time the roadmap gate compares between RFI and IDR |

Roadmap gate: with the same loss pattern, `rfi_idr` low, `fix_ms` and the viewer's `lost` no
worse than with the option off, and `rfi_max` well below the `keymax` of the IDR run; the Dell
(Iris Xe) decodes the stream after a recovery point without artefacts.

### UDP video lane over the internet (M24)

Host server option `enable-udp-video-wan=Y` (off by default) on top of `enable-udp-video`
(`live` or `shadow`), a viewer of lane v6 (`enable-udp-video-wan` not `N` on it, a direct
session whose host address it knows, no proxy or WebSocket on it), and a session on a direct
internet path: TCP punch, direct IP access or KCP over a punched IPv4 UDP socket, never relay,
WebRTC or KCP over IPv6 (docs/UDP-VIDEO.md,
"Internet lane (M24)"). Every line of the sections above applies; the window lines are the
same. The handshake adds:

```
UDPVIDEO host conn=5 offer lane=3f2a… mode=live addr=0.0.0.0:61234 every=2 burst=8 timeout_ms=1000 fec=20 rfi=false viewer=81.2.3.4 installed=true path=wan4 public_port=40312 mapped=true nat_ms=38 rendezvous=203.0.113.7:21116
UDPVIDEO viewer peer=123456789 accepted lane=3f2a… mode=live host=198.51.100.9:40312 local=0.0.0.0:50511 every=2 timeout_ms=1000 fec=20 path=wan4 max_datagram=1024
UDPVIDEO viewer peer=123456789 candidate lane=3f2a… port=61877 mapped=true nat_ms=41
UDPVIDEO host conn=5 candidate lane=3f2a… viewer=81.2.3.4:61877 after_ms=1180
UDPVIDEO host conn=5 HELLO from 81.2.3.4:61877 lane=3f2a… at_us=1250312 path=wan4 punches=3
UDPVIDEO viewer peer=123456789 ready lane=3f2a… after_ms=96 hellos=5
UDPVIDEO host conn=5 stop lane=3f2a…: internet lane: no HELLO from the viewer 81.2.3.4 in 6 s (offered=true candidate=81.2.3.4:61877 punches=240 refused=0): …
UDPVIDEO host conn=5 not offered (viewer v5): not a LAN address; internet lane: the viewer's lane v5 has none (…)
```

Host `offer` line (an internet lane logs it on the lane's receive thread once the rendezvous
server's DNS lookup, bounded at 1 s, and the NAT query are done, so it may come up to 2 s after
the login):

| Field | Meaning |
|---|---|
| `addr` | the lane socket: the wildcard of the viewer's family and its local port |
| `path` | `wan4` (datagrams at most 1024 bytes) or `wan6` (1184); LAN lanes do not print it (1400) |
| `public_port` | the port the offer names: the rendezvous server's answer (`mapped=true`), else the local port (no answer within 1 s, IPv6, no server, a viewer in 100.64.0.0/10) |
| `mapped` | the rendezvous server answered the `TestNatRequest` from the lane socket |
| `nat_ms` | time the query took (up to 1000 when there was no answer; the DNS lookup before it is not counted) |
| `rendezvous` | the server address asked; `none` for IPv6, an empty server, a viewer in 100.64.0.0/10 (CGNAT or an overlay such as Tailscale: the port the server sees belongs to another path, so the local port is offered) or a DNS lookup with no answer within 1 s |

Viewer `accepted` line: `path` (`lan`, `wan4`, `wan6`) and `max_datagram` (the offer's budget).
`host` is the host IP this viewer reached the session on, with the offer's port.

Viewer `candidate` line, from the lane thread before the first HELLO: `port` is the lane socket's
public port (`mapped=true`: the rendezvous server's answer) or its local port (also toward a
host in 100.64.0.0/10), sent to the host in `UdpVideoControl{KindCandidate}`; `nat_ms` the
lookup and query time. Host `candidate` line: the address the host punches (the viewer's session
IP and that port) and `after_ms` since the offer went out (after the host's own lookup and NAT
query, so it is the candidate's round trip: the offer's trip, the viewer's lookup and query, the
candidate's trip). The host's `active ... after_ms` and the 6 s handshake timeout count from the
same moment. Host `HELLO` line: `path` and `punches`, the punch datagrams sent before the viewer's first
HELLO came in (0: the viewer got through without the host's NAT needing them).

Internet stop reasons (host): `no HELLO ... in 6 s` with `offered` (the offer went out),
`candidate` (`none`: the viewer never reported one: it refused or its NAT query and HELLOs never
started), `punches` and `refused` (datagrams from other addresses or failing the crypto). Many
punches and no HELLO means a NAT on either side that maps or filters per destination (symmetric
NAT, CGNAT) or a path that drops UDP; the session stays on TCP/KCP and the next offer comes after
10 s, 30 s and 120 s (`RETRY_BACKOFF_WAN`), then never in that session. A failed handshake is
retried like a fallback (unlike on the LAN), and so is the viewer's handshake failure or refusal;
the three retries are one budget. The viewer's `handshake failed` line says the same from its
side (`no HELLO_ACK in 3000 ms`); a viewer with a SOCKS proxy or WebSocket advertises v5 and
refuses an internet offer (`internet lane: a proxy or WebSocket is set on the viewer`).

Keepalive: on an internet lane the host's receive thread sends a PING when it has sent nothing
for 500 ms (the viewer's PINGs every 200 ms are answered with PONGs, so this only fires when
those are lost). It shows in no line; the viewer takes it as traffic (its silence timer).

Roadmap gate (4a/5): on a phone hotspot and another network, the live window lines (`lost`,
`fec_rec`, `fec_unrec`, `rtt_ms`, `owd_var_ms`) per 5 s, and at 1-3% loss no multi-frame stall
(`fix_ms`, the viewer's `kf_req`).

### Per-frame feedback (M23)

Host server option `enable-video-feedback=Y` (off by default) and a fork viewer on Windows
(`enable-video-feedback` not `N`): the viewer reports every video frame's arrival, size, decode
and present time and, on a live UDP lane, its losses, at most every 50 ms over the session
stream; the host turns that into a one-way queueing delay, a GCC trend, jitter and loss rates
(docs/UDP-VIDEO.md, "Per-frame feedback (M23)"). Works on every transport (TCP, KCP, relay,
WebRTC, UDP lane). Measurement only unless `video-feedback-qos=Y`.

```
VFB host conn=5 advertised version=1 qos=false fec_adaptive=false
VFB viewer host_version=1 sending=true
VFB host conn=5 secs=5.0 batches=100 rl=0 frames=300 unmatched=0 udp_frames=0 done=299 dropped=0 q_owd=0.4/2.1/9.8 trend_max=3.2 thr=6.0 usage=Normal over=0 under=0 jitter=0.6 viewer=6.1/8.0/14.2 decode=3.2/4.9/9.1 queue_max=1 dgram_loss=- frames_lost=0 lost_ranges=0 fec_rec=0 fec_unrec=0 switches=0 last=[age_ms=12 frames=3 q_owd=0.2/1.1 trend=0.4 thr=6.0 usage=Normal jitter=0.6 kbps=9850 viewer=7.2 decode=3.9 queue=0 udp=false loss=- frame_loss=- fec_rec=0 lost=0] qos=false
VFB viewer secs=5.0 batches=100 frames=300 done=299 lost=0 dropped=0 bytes=6100
UDPVIDEO host conn=5 fec 25% (datagram loss 10.2% over 1 s, adaptive)
```

The `advertised` and `host_version` lines come at each login (always on); the window lines every
5 s with `enable-latency-log=Y` (each side's own option), only while batches come; the `fec` line
on each adaptive change (`udp-video-fec-adaptive=Y`, always on).

`VFB host`, per connection:

| Field | Meaning |
|---|---|
| `secs` | window length |
| `batches` / `rl` | batches taken / dropped by the host's rate limit (more than 40 per second) |
| `frames` | arrivals reported; `unmatched`: of them, not found in the host's send log (a frame sent more than 512 frames ago, or before the log started) |
| `udp_frames` | matched frames that went over the UDP lane |
| `done` | decode/present entries; `dropped`: entries the viewer left out of full batches (64 frames, 64 done, 16 loss ranges) |
| `q_owd` | p50/p95/max of the per-frame queueing one-way delay in ms: (viewer arrival - host send) minus its lowest value over the last 10 s (1 s buckets, dropped by age, so a static screen does not keep an old minimum). TCP and the UDP lane have different fixed delays, so a change of transport between two matched frames starts a new baseline (and the trendline skips that gradient); for the first 1 s of a baseline (session start, each switch) there is no `q_owd` sample. The host stamps a frame when it takes it off the connection's video channel (TCP: right before the write, so a blocked write counts; UDP: before the lane queue and pacer), the viewer when the frame enters its decoder path (TCP: after the parse; UDP: as the lane releases it). Covers host write, network, relay legs and the viewer's io_loop; a large frame's own transmission time counts too, which is why QoS uses each batch's smallest value |
| `trend_max` | largest GCC modified trend (delay-gradient slope x min(frames, 60) x 4) in the window |
| `thr` / `usage` | the overuse detector's adaptive threshold and state at the end of the window |
| `over` / `under` | transitions into overuse / underuse in the window |
| `jitter` | inter-arrival jitter in ms (RFC 3550 style, gain 1/16) |
| `viewer` | p50/p95/max of arrival-to-UI-handover in ms of the frames shown (the viewer's own delay: queue, decode, conversion, callback) |
| `decode` | p50/p95/max of arrival-to-decode-end in ms, shown or not |
| `queue_max` | largest decoder queue length at an arrival |
| `dgram_loss` | UDP lane datagram loss in percent over the window (`-`: no lane counters) |
| `frames_lost` | UDP frames the viewer declared lost (after FEC); `lost_ranges`: loss ranges reported, repaired ones included |
| `fec_rec` / `fec_unrec` | UDP frames FEC saved / could not save |
| `switches` | transport changes (TCP to UDP lane or back) between matched frames, each a new `q_owd` baseline |
| `last=[...]` | the last batch's `FeedbackInput`: `age_ms` since it, matched `frames`, `q_owd` batch min/max, `trend`, `thr`, `usage`, `jitter`, `kbps` (bytes of its frames over their arrival span), `viewer` / `decode` (largest), `queue`, `udp`, `loss` / `frame_loss` (UDP, percent over 1 s), `fec_rec`, `lost` |
| `fec_pct` | the adaptive FEC percentage, once it took over (`udp-video-fec-adaptive=Y`) |
| `qos` | `video-feedback-qos` is on: the feedback steers QoS and the lane fallback |

`VFB viewer`: `batches` sent, `frames` / `done` / `lost` entries in them, `dropped` entries left
out, `bytes` of the batches (protobuf size, before encryption).

With `video-feedback-qos=Y` the `qos_sig` of the `LATENCY server` line is unchanged; the delay
sample video_qos gets becomes `max(sample, q_owd)` (largest batch minimum since the last
TestDelay) plus 150 ms if the UDP datagram loss over 1 s went above 10%, unless the live lane's
sample already carries its own 150 ms frame-loss penalty (one penalty per sample). A live lane
that loses more than 10% of its frames after FEC over 3 s (at least 30 frames) goes back to TCP
with the `UDPVIDEO host ... stop` reason `feedback: N of M frames lost after FEC in 3 s (P% >
10%; Q host queue drops left out), datagram loss D%`. Frames the host's own tap dropped on a full
lane queue (`qdrop`) leave a gap the viewer counts as lost; the Q of them in the window are taken
off N, so the rule blames only the network.

The UDP lane's host window line gets `fec_pct=P` once the adaptive FEC changed the lane's
percentage.

Roadmap gate (M23): with the hbbr-to-viewer leg throttled, `q_owd` (and `usage=Overuse`) shows
the congestion while the host's TCP meter `rtt` (`LATENCY tcp`) does not.

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
Set fps to 27 (field=auto reason=decrease last_auto=Some(54) custom=60 limited=27 min_decode_fps=30 max_queue_len=3 direct=true | d0 …;)
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

`field` in `Set fps to` (both controllers): how the fps went to the host (roadmap M16). With
local option `enable-fps-cap-split` (off by default on every platform, `Y` turns it on,
until the stock host versions in use are checked to cap on tag 28; read when a connection
starts) and a host of version 1.2.2 or later, `auto`: as `Misc.auto_adjust_fps` (upstream
tag 28). Hosts since 1.2.2 cap the stream at min(`custom_fps`, `auto_adjust_fps`), so the
effect is the same as before, but `custom_fps` stays the user's setting and the host can tell
a decode limit from a user choice (the `fps_cap` field of `LATENCY fast`). Otherwise
`custom`: as `OptionMessage.custom_fps`, as before (older hosts, which may not handle tag 28,
always get this). Before the split a user `custom_fps` replaced the controller's limit on the
host; with it the host would keep the lower auto cap, so a user `custom_fps` above the last
auto cap sent on the connection is followed by `auto_adjust_fps` = that custom fps, logged
`Set fps to N (field=auto reason=user-custom)`, and both controllers start again from the
user's fps as before.

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
Set fps to 37 (field=auto gentle custom=60 target=43 floor=24 cap=46.0 overload=2/2 … decision=decrease sent=37 direct=true | d0 …;| d0 ready=1 …;)
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
