#[cfg(any(test, not(target_os = "linux")))]
mod audio_resampler;
mod keyboard;
/// cbindgen:ignore
pub mod platform;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use platform::{
    clip_cursor, get_cursor, get_cursor_data, get_cursor_pos, get_focused_display,
    set_cursor_pos, start_os_service,
};
#[cfg(not(any(target_os = "ios")))]
/// cbindgen:ignore
mod server;
#[cfg(not(any(target_os = "ios")))]
pub use self::server::*;
mod client;
mod microphone_forwarding;
mod lan;
#[cfg(not(any(target_os = "ios")))]
mod rendezvous_mediator;
#[cfg(not(any(target_os = "ios")))]
pub use self::rendezvous_mediator::*;
/// cbindgen:ignore
pub mod common;
#[cfg(not(any(target_os = "ios")))]
pub mod ipc;
#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    feature = "flutter"
)))]
pub mod ui;
mod version;
pub use version::*;
#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
mod bridge_generated;
#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
pub mod flutter;
#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
pub mod flutter_ffi;
use common::*;
mod auth_2fa;
#[cfg(not(target_os = "ios"))]
mod clipboard;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod core_main;
mod custom_server;
mod lang;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod port_forward;
mod port_forward_mux;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod tray;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod whiteboard;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod updater;

mod ui_cm_interface;
mod ui_interface;
mod ui_session_interface;

// Headless MCP agent. Lives inside the lib so it can reach the private
// client/ui_session_interface/port_forward modules (a [[bin]] cannot). Gated on
// flutter because it mirrors the flutter Session<T> shape (cursor_shapes,
// is_multi_ui_session, on_texture). The thin bin is src/bin/rustdesk-agent.rs.
#[cfg(all(
    not(any(target_os = "android", target_os = "ios")),
    feature = "flutter"
))]
pub mod agent;

mod hbbs_http;

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub mod clipboard_file;

pub mod privacy_mode;

#[cfg(windows)]
pub mod virtual_display_manager;

mod kcp_stream;
// UDP video lane (M19 first half): staged, not wired into sessions yet. docs/UDP-VIDEO.md
mod udp_video;
// Per-frame video feedback viewer -> host and its host-side estimator (roadmap M23).
// docs/UDP-VIDEO.md "Per-frame feedback (M23)"
mod video_feedback;
// The host's session stream, whole or split into a reader and a writer task (roadmap M12).
#[cfg(not(target_os = "ios"))]
mod session_io;
