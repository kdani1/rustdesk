// rustdesk-agent: a headless RustDesk CLIENT exposed as a stdio MCP server.
//
// This is intentionally a thin shim. All implementation lives in
// `librustdesk::agent` (src/agent/), because that module is compiled inside the
// library crate and can therefore reach the crate-private `client`,
// `ui_session_interface` and `port_forward` modules that a standalone binary
// cannot see.
//
// Do NOT add `#![windows_subsystem = "windows"]` here: the process needs a real
// console because the MCP transport is newline-delimited JSON-RPC over
// stdin/stdout. Do NOT rename the executable to anything containing `host=` or
// `--`; core's custom-server parsing keys off the exe name.

fn main() {
    #[cfg(all(
        not(any(target_os = "android", target_os = "ios")),
        feature = "flutter"
    ))]
    {
        librustdesk::agent::run();
    }
    #[cfg(not(all(
        not(any(target_os = "android", target_os = "ios")),
        feature = "flutter"
    )))]
    {
        eprintln!(
            "rustdesk-agent must be built for a desktop target with the `flutter` feature \
             (cargo build --release --features flutter,hwcodec,vram --bin rustdesk-agent)"
        );
        std::process::exit(1);
    }
}
