// rustdesk-agent: RustDesk's own client capabilities, exposed to an AI over a
// stdio MCP (Model Context Protocol) server.
//
// This module lives INSIDE the library crate on purpose: it needs the
// crate-private `client`, `ui_session_interface` and `port_forward` modules
// (Session<T>, InvokeUiSession, io_loop, port_forward::listen) that a standalone
// [[bin]] cannot reach. The actual executable, src/bin/rustdesk-agent.rs, is a
// thin shim that just calls `run()`.
//
// Design notes:
//  * Each peer tool opens its own client connection with the right ConnType
//    (file transfer / terminal / port forward are distinct connections), drives
//    the reused client core, and reports results through an AgentHandler
//    (see handler.rs) that the synchronous tool code waits on.
//  * io_loop / ipc / PeerConfig async helpers are #[tokio::main] and panic if
//    called from inside an existing tokio runtime, so everything here runs on
//    plain std threads; no tokio runtime is created in the agent itself.
//  * profile.release uses panic="abort", so a panic kills the whole server.
//    Tool code therefore avoids unwrap()/expect() on peer/IO values and returns
//    error strings handled at the request boundary.
//
// User policy baked in (explicit, on the user's own machines; the host still
// shows the session -- nothing here hides, persists or conceals anything):
//  * full access to ALL saved peers, no allow-list, no per-call confirmation,
//  * auto-accept the insecure-connection (unverified key) prompt,
//  * auto-login with each peer's saved password.

mod handler;
mod input_stub;
mod mcp;
mod tools;

/// Entry point called by the rustdesk-agent binary. Initializes the client core
/// (as the interactive user, so config resolves to %APPDATA%\RustDesk) and then
/// serves MCP over stdio until stdin closes.
pub fn run() {
    if !crate::common::global_init() {
        eprintln!("[rustdesk-agent] global_init() failed");
        std::process::exit(1);
    }
    crate::load_custom_client();

    // Logs go to a file / stderr (init_log never writes to stdout), which keeps
    // the stdout JSON-RPC stream clean. See mcp.rs for the stdout hazard note.
    hbb_common::init_log(false, "agent");

    #[cfg(windows)]
    {
        // Sets the custom rendezvous server from the exe-name license (harmless
        // for "rustdesk-agent") and hardens DLL loading in release builds. Must
        // run as the interactive user; server/key then resolve automatically
        // from the user's RustDesk2.toml (custom-rendezvous-server + key).
        if !crate::platform::windows::bootstrap() {
            eprintln!("[rustdesk-agent] windows bootstrap() failed");
            std::process::exit(1);
        }
    }

    hbb_common::log::info!("rustdesk-agent: MCP stdio server starting");
    mcp::serve();
    hbb_common::log::info!("rustdesk-agent: stdin closed, exiting");
}
