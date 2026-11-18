# Patches for dependencies

## hwcodec: this branch needs the fork

`hwcodec-778df1f-nvenc-low-latency.patch` is `git diff 778df1f..HEAD` of the hwcodec fork
(`C:\Projects\hwcodec`, branch `rustdesk-nvenc-low-latency`), on top of the upstream revision
778df1f that `Cargo.lock` pins.

Since the M9/M13/M14 merges RustDesk calls hwcodec API that exists only in that fork, so this
branch does **not** build against upstream `https://github.com/rustdesk-org/hwcodec` (errors
E0425/E0599 wherever the `hwcodec` feature is on: Windows, Linux, macOS and the Android viewer).

The fork is published as `https://github.com/kdani1/hwcodec`, branch `rustdesk-nvenc-low-latency`
(a GitHub fork of rustdesk-org/hwcodec). `libs/scrap/Cargo.toml` names it and `Cargo.lock` pins
commit e65c44d, so a plain checkout of this branch builds on any machine or CI.

For local work on hwcodec itself, the build machine overrides the git dependency with the local
checkout in its `CARGO_HOME` config (`C:\RustDeskBuild\cargo\config.toml`, set by
`C:\RustDeskBuild\latency\env.cmd`). That override drops the `source` line of the hwcodec entry
from the local `Cargo.lock`; do not commit that local change.

```toml
[patch."https://github.com/kdani1/hwcodec"]
hwcodec = { path = "C:/Projects/hwcodec" }
```

After changing hwcodec, push the fork branch and update the commit in `Cargo.lock`
(`cargo update -p hwcodec` without the override) before committing a RustDesk change that needs it.

Fork-only API that RustDesk uses:

| API | Used in |
|---|---|
| `hwcodec::ffmpeg::set_nvenc_vbv_frames(frames: i32)` | `libs/scrap/src/common/hwcodec.rs`, `vram.rs` (M14 VBV) |
| `hwcodec::ffmpeg::set_encode_gpu_thread_priority(priority: i32)` (C: `hwcodec_set_encode_gpu_thread_priority`) | `libs/scrap/src/common/hwcodec.rs` `set_encode_gpu_thread_priority`, called from `src/platform/windows_gpu_priority.rs` (M17 GPU priority: `SetGPUThreadPriority` on D3D11 devices encoders create themselves) |
| `hwcodec::ffmpeg_ram::encode::Encoder::request_keyframe(&mut self) -> Result<(), ()>` | `libs/scrap/src/common/hwcodec.rs` (M13) |
| `hwcodec::ffmpeg_ram::encode::Encoder::set_framerate(&mut self, framerate: i32) -> Result<(), ()>` (C: `ffmpeg_ram_set_framerate`) | `libs/scrap/src/common/hwcodec.rs` (M7 virtual framerate) |
| `hwcodec::vram::encode::Encoder::request_keyframe(&mut self) -> Result<(), i32>` | `libs/scrap/src/common/vram.rs` (M13) |
| `hwcodec::vram::encode::Encoder::encode_last(&mut self, ms: i64) -> Result<&mut Vec<EncodeFrame>, i32>` | `libs/scrap/src/common/vram.rs` (M13 kept image, M14 static refine) |
| `hwcodec::vram::decode::Decoder::decode_with_output(&mut self, packet: &[u8], output: bool) -> Result<Option<&mut Vec<DecodeFrame>>, i32>` | `libs/scrap/src/common/vram.rs` (M9) |
| `hwcodec::vram::encode::set_nv_rfi(on: bool)` | `libs/scrap/src/common/vram.rs` (M22: NV SDK driver encoders opened afterwards keep their input for forced IDRs and `encode_last`, run P2/ultra-low-latency, log the NVENC caps and, for HEVC with the cap, use DPB 5 / one L0 reference for RFI) |
| `hwcodec::vram::encode::Encoder::invalidate_refs(&mut self, pts: i64) -> Result<(), i32>` | `libs/scrap/src/common/vram.rs` (M22 reference frame invalidation; `EncodeFrame::key` 2 marks the recovery point) |

The fork's C++ changes (nvenc low-latency options, forced IDR, VBV, bounded `Query` poll, open
and decode logging) have no Rust API of their own; they are in the patch as well. The VRAM
`hwcodec::vram::encode::Encoder::set_framerate` is upstream API, but on FFmpeg nvenc the fork
changes what it does (M7): upstream it is a stub that returns -1, in the fork it rescales the
bitrate to bitrate × clamp(opened fps / fps, 0.5, 4), capped at max(bitrate, 4 × the opened
bitrate) (virtual framerate), and keeps the VBV at N frames of that per-frame budget; AMF and
QSV still return -1. M22 adds the C entry points `nv_set_rfi`, `nv_encode_last` and
`nv_invalidate_refs` (`cpp/nv/nv_ffi.h`, bound by bindgen) and `invalidate_refs` in the
drivers' call table (`-1` for AMF, MFX and FFmpeg); the NV driver's `request_keyframe` and
`encode_last` now work for encoders opened with `nv_set_rfi(1)` and keep refusing (`-1`)
otherwise. The NV driver passes a frame counter as `inputTimeStamp`.

The patch was last regenerated at fork commit e65c44d (merge of `rustdesk-b3-rate`, M7/M17,
and `rustdesk-b3-udp`, M22).
The patch file is kept as a readable record of the fork's changes against 778df1f.
