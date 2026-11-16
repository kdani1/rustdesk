# Patches for dependencies

## hwcodec: this branch needs the fork

`hwcodec-778df1f-nvenc-low-latency.patch` is `git diff 778df1f..HEAD` of the hwcodec fork
(`C:\Projects\hwcodec`, branch `rustdesk-nvenc-low-latency`), on top of the upstream revision
778df1f that `Cargo.lock` pins.

Since the M9/M13/M14 merges RustDesk calls hwcodec API that exists only in that fork, so this
branch does **not** build against upstream `https://github.com/rustdesk-org/hwcodec` (errors
E0425/E0599 wherever the `hwcodec` feature is on: Windows, Linux, macOS and the Android viewer).
`libs/scrap/Cargo.toml` still names the upstream git URL. Until the fork is published, a build
has to use the fork through the `[patch]` in the `CARGO_HOME` config of the build
(`C:\RustDeskBuild\cargo\config.toml`, set by `C:\RustDeskBuild\latency\env.cmd`):

```toml
[patch."https://github.com/rustdesk-org/hwcodec"]
hwcodec = { path = "C:/Projects/hwcodec" }
```

Removing that override does not give a working upstream build: it also needs these RustDesk
commits reverted. Another machine or CI needs the fork checked out and the same `[patch]` (or
the patch above applied to an hwcodec checkout at 778df1f).

Fork-only API that RustDesk uses:

| API | Used in |
|---|---|
| `hwcodec::ffmpeg::set_nvenc_vbv_frames(frames: i32)` | `libs/scrap/src/common/hwcodec.rs`, `vram.rs` (M14 VBV) |
| `hwcodec::ffmpeg_ram::encode::Encoder::request_keyframe(&mut self) -> Result<(), ()>` | `libs/scrap/src/common/hwcodec.rs` (M13) |
| `hwcodec::ffmpeg_ram::encode::Encoder::set_framerate(&mut self, framerate: i32) -> Result<(), ()>` (C: `ffmpeg_ram_set_framerate`) | `libs/scrap/src/common/hwcodec.rs` (M7 virtual framerate) |
| `hwcodec::vram::encode::Encoder::request_keyframe(&mut self) -> Result<(), i32>` | `libs/scrap/src/common/vram.rs` (M13) |
| `hwcodec::vram::encode::Encoder::encode_last(&mut self, ms: i64) -> Result<&mut Vec<EncodeFrame>, i32>` | `libs/scrap/src/common/vram.rs` (M13 kept image, M14 static refine) |
| `hwcodec::vram::decode::Decoder::decode_with_output(&mut self, packet: &[u8], output: bool) -> Result<Option<&mut Vec<DecodeFrame>>, i32>` | `libs/scrap/src/common/vram.rs` (M9) |

The fork's C++ changes (nvenc low-latency options, forced IDR, VBV, bounded `Query` poll, open
and decode logging) have no Rust API of their own; they are in the patch as well. The VRAM
`hwcodec::vram::encode::Encoder::set_framerate` is upstream API, but on FFmpeg nvenc the fork
changes what it does (M7): upstream it is a stub that returns -1, in the fork it rescales the
bitrate to bitrate × clamp(opened fps / fps, 0.5, 4) (virtual framerate) and keeps the VBV at
N frames of that per-frame budget; AMF and QSV still return -1.

Whether and where to publish the fork (and then point `libs/scrap/Cargo.toml` at it) is the
owner's decision; until then `Cargo.toml` and `Cargo.lock` stay as they are.
