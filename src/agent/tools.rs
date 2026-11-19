// MCP tool definitions and their implementations.
//
// Each tool that touches a peer opens its OWN RustDesk client connection with
// the right ConnType (file transfer, terminal, port forward are separate
// connections in the protocol), drives it through the reused client core, waits
// for the result on the AgentHandler's shared state, then tears the connection
// down (port_forward keeps its listener alive for the life of the process).
//
// All waits are time-bounded. Because profile.release uses panic="abort", a
// panic anywhere kills the whole server, so this code avoids unwrap()/expect()
// on peer- or IO-derived values and returns Err strings instead.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use base::fs;
use hbb_common::config::{LocalConfig, PeerConfig};
use hbb_common::rendezvous_proto::ConnType;

use crate::client::FileManager;
use crate::common::input::{
    MOUSE_BUTTON_LEFT, MOUSE_BUTTON_RIGHT, MOUSE_BUTTON_WHEEL, MOUSE_TYPE_DOWN, MOUSE_TYPE_MOVE,
    MOUSE_TYPE_UP, MOUSE_TYPE_WHEEL,
};
use crate::ui_session_interface::{io_loop, Session};

use super::handler::{AgentHandler, FileEntryInfo};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DIR_TIMEOUT: Duration = Duration::from_secs(30);
const JOB_TIMEOUT: Duration = Duration::from_secs(600);
const TERMINAL_OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(60);
const PEER_INFO_TIMEOUT: Duration = Duration::from_secs(30);
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------

/// A live agent session: the client `Session` plus a handle to its shared state.
struct AgentSession {
    session: Session<AgentHandler>,
    handler: AgentHandler,
}

/// Build a session for `id` with `conn_type`, wire the handler to the session's
/// reply channel, initialize the login config (which loads the saved PeerConfig,
/// including the remembered password used for auto-login), and spawn io_loop on
/// a plain OS thread. io_loop is #[tokio::main(current_thread)], i.e. a blocking
/// sync fn that builds its own runtime, so it must NOT be called from async
/// context -- a dedicated std::thread is exactly right.
fn start_session(id: &str, conn_type: ConnType, args: Vec<String>) -> AgentSession {
    let session: Session<AgentHandler> = Session {
        // Empty preset password: handle_hash() then falls through to the saved
        // per-peer config.password (the remembered password), which is the
        // user's intended auto-login behavior.
        password: String::new(),
        args,
        server_keyboard_enabled: Arc::new(RwLock::new(true)),
        server_file_transfer_enabled: Arc::new(RwLock::new(true)),
        server_clipboard_enabled: Arc::new(RwLock::new(true)),
        reconnect_count: Arc::new(AtomicUsize::new(0)),
        ..Default::default()
    };
    let handler = session.ui_handler.clone();
    handler.attach_sender(session.sender.clone());

    session.lc.write().unwrap().initialize(
        id.to_string(),
        conn_type,
        None,  // switch_uuid
        false, // force_relay
        None,  // adapter_luid
        None,  // shared_password
        None,  // conn_token
    );

    let s = session.clone();
    std::thread::spawn(move || {
        // Blocking: runs the connection until it closes.
        io_loop(s, 0);
    });

    AgentSession { session, handler }
}

// ---------------------------------------------------------------------------
// Cached DEFAULT_CONN sessions (screen + input)
// ---------------------------------------------------------------------------
//
// screenshot and the input tools (move_mouse/click/scroll/type_text/key) all
// drive a single DEFAULT_CONN (the normal remote-control connection) per peer.
// Unlike the file-transfer/terminal tools, which open and close a fresh
// connection per call, a DEFAULT_CONN is comparatively expensive -- it also
// starts a video stream from the host (which costs host CPU + network even
// though we discard every frame: on_rgba/get_rgba are no-ops in AgentHandler) --
// so we open ONE per peer and keep it alive, reusing it across calls. The
// session stays visible on the host, exactly like an interactive RustDesk
// session; nothing here hides it.

/// Keeps one live DEFAULT_CONN session per peer id for the life of the process
/// (until an error tears it down and it is reopened on the next call).
fn default_registry() -> &'static Mutex<HashMap<String, Arc<AgentSession>>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Arc<AgentSession>>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get the cached DEFAULT_CONN session for `peer_id`, opening and connecting a
/// new one if necessary. On a fresh connection this blocks until the peer info
/// has arrived (so is_screenshot_supported / input are ready). The whole
/// operation holds the registry lock, which is fine: the MCP server processes
/// requests one at a time, so there is never real contention, and it prevents
/// opening two sessions for the same peer.
fn default_session(peer_id: &str) -> Result<Arc<AgentSession>, String> {
    let mut reg = default_registry().lock().unwrap();
    if let Some(s) = reg.get(peer_id) {
        return Ok(s.clone());
    }
    let s = Arc::new(start_session(peer_id, ConnType::DEFAULT_CONN, Vec::new()));
    if let Err(e) = s.handler.wait_connected(CONNECT_TIMEOUT) {
        s.session.close();
        return Err(e);
    }
    if let Err(e) = s.handler.wait_peer_info(PEER_INFO_TIMEOUT) {
        s.session.close();
        return Err(e);
    }
    reg.insert(peer_id.to_string(), s.clone());
    Ok(s)
}

/// Drop and close the cached session for `peer_id` (called when a call on it
/// errors, so the next call reconnects cleanly).
fn evict_default_session(peer_id: &str) {
    if let Some(s) = default_registry().lock().unwrap().remove(peer_id) {
        s.session.close();
    }
}

// ---------------------------------------------------------------------------
// Tool catalog (returned by tools/list)
// ---------------------------------------------------------------------------

pub fn tool_definitions() -> Value {
    json!([
        {
            "name": "list_peers",
            "description": "List all saved RustDesk peers (address book entries) available to connect to, with alias, hostname, platform and whether a password is remembered.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "ls",
            "description": "List a directory on a remote peer over a file-transfer connection. Returns entries with name, type (dir/file), size and modified time.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string", "description": "The peer id (as shown by list_peers)." },
                    "path": { "type": "string", "description": "Remote directory path. Empty string lists drive roots on Windows." },
                    "include_hidden": { "type": "boolean", "description": "Include hidden entries (default false)." }
                },
                "required": ["peer_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "upload",
            "description": "Upload a single local file to a remote peer (local -> remote). Overwrites on the remote without prompting.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string" },
                    "local_path": { "type": "string", "description": "Absolute path of the local source file." },
                    "remote_path": { "type": "string", "description": "Absolute destination path on the remote peer." }
                },
                "required": ["peer_id", "local_path", "remote_path"],
                "additionalProperties": false
            }
        },
        {
            "name": "download",
            "description": "Download a single file from a remote peer to the local machine (remote -> local). Overwrites locally without prompting.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string" },
                    "remote_path": { "type": "string", "description": "Absolute path of the remote source file." },
                    "local_path": { "type": "string", "description": "Absolute destination path on the local machine." }
                },
                "required": ["peer_id", "remote_path", "local_path"],
                "additionalProperties": false
            }
        },
        {
            "name": "run",
            "description": "Run ONE command in a remote terminal (PowerShell on Windows hosts) and return its captured output and exit code. The session is shown on the host; nothing is hidden.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string" },
                    "command": { "type": "string", "description": "The command line to execute." },
                    "timeout_secs": { "type": "integer", "description": "Max seconds to wait for completion (default 60)." }
                },
                "required": ["peer_id", "command"],
                "additionalProperties": false
            }
        },
        {
            "name": "port_forward",
            "description": "Open a local TCP listener on 127.0.0.1:<local_port> that tunnels to <remote_host>:<remote_port> through the peer. The listener stays open for the life of the agent process.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string" },
                    "local_port": { "type": "integer", "description": "Local port to listen on (1-65535, not 0)." },
                    "remote_host": { "type": "string", "description": "Host reachable from the peer, e.g. 127.0.0.1 or a LAN name." },
                    "remote_port": { "type": "integer", "description": "Destination port on remote_host (1-65535, not 0)." }
                },
                "required": ["peer_id", "local_port", "remote_host", "remote_port"],
                "additionalProperties": false
            }
        },
        {
            "name": "screenshot",
            "description": "Capture a still screenshot of a remote peer's display and return it as a PNG image. Opens (and reuses) a normal remote-control connection to the peer; the session is visible on the host. Requires the host to run RustDesk 1.4.0 or newer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string", "description": "The peer id (as shown by list_peers)." },
                    "display": { "type": "integer", "description": "Zero-based display index to capture (default 0)." }
                },
                "required": ["peer_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "move_mouse",
            "description": "Move the remote mouse cursor to absolute coordinates (x, y) in the remote display's pixel space. Uses the shared remote-control connection to the peer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string" },
                    "x": { "type": "integer", "description": "Absolute X in remote display pixels." },
                    "y": { "type": "integer", "description": "Absolute Y in remote display pixels." }
                },
                "required": ["peer_id", "x", "y"],
                "additionalProperties": false
            }
        },
        {
            "name": "click",
            "description": "Click a mouse button at absolute coordinates (x, y) on the remote display (moves there first, then presses and releases). Requires keyboard/mouse control to be permitted on the host.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string" },
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "Mouse button (default left)." },
                    "double": { "type": "boolean", "description": "Double-click (default false)." }
                },
                "required": ["peer_id", "x", "y"],
                "additionalProperties": false
            }
        },
        {
            "name": "scroll",
            "description": "Scroll the mouse wheel at absolute coordinates (x, y) on the remote display. dx/dy are wheel deltas (positive dy typically scrolls down); the cursor is moved to (x, y) first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string" },
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "dx": { "type": "integer", "description": "Horizontal wheel delta (default 0)." },
                    "dy": { "type": "integer", "description": "Vertical wheel delta (default 0)." }
                },
                "required": ["peer_id", "x", "y"],
                "additionalProperties": false
            }
        },
        {
            "name": "type_text",
            "description": "Type a string of text on the remote peer (sends it as a key sequence). Requires keyboard control to be permitted on the host.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string" },
                    "text": { "type": "string", "description": "The literal text to type." }
                },
                "required": ["peer_id", "text"],
                "additionalProperties": false
            }
        },
        {
            "name": "key",
            "description": "Press one key on the remote peer (down then up). Use a single character (e.g. \"a\", \"1\") or a VK name from RustDesk's key map (e.g. \"VK_RETURN\", \"VK_ESCAPE\", \"VK_TAB\", \"VK_LEFT\", \"VK_F5\"). Optional modifiers are held during the press.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string" },
                    "name": { "type": "string", "description": "A single character, or a VK_* key name." },
                    "modifiers": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["alt", "ctrl", "shift", "meta"] },
                        "description": "Modifier keys to hold (meta = Windows/Command key)."
                    }
                },
                "required": ["peer_id", "name"],
                "additionalProperties": false
            }
        }
    ])
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch a tool call. On success returns the MCP `content` array: a single
/// text block for text-returning tools, or an image block for `screenshot`.
pub fn dispatch(name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "list_peers" => list_peers().map(text_content),
        "ls" => ls(args).map(text_content),
        "upload" => upload(args).map(text_content),
        "download" => download(args).map(text_content),
        "run" => run(args).map(text_content),
        "port_forward" => port_forward(args).map(text_content),
        "screenshot" => screenshot(args),
        "move_mouse" => move_mouse(args).map(text_content),
        "click" => click(args).map(text_content),
        "scroll" => scroll(args).map(text_content),
        "type_text" => type_text(args).map(text_content),
        "key" => key(args).map(text_content),
        other => Err(format!("unknown tool: {}", other)),
    }
}

/// Wrap a plain text result as a one-element MCP content array.
fn text_content(text: String) -> Value {
    json!([{ "type": "text", "text": text }])
}

fn arg_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required string argument '{}'", key))
}

fn arg_str_opt(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn arg_i64(args: &Value, key: &str) -> Result<i64, String> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("missing required integer argument '{}'", key))
}

// ---------------------------------------------------------------------------
// list_peers
// ---------------------------------------------------------------------------

fn list_peers() -> Result<String, String> {
    let favorites = LocalConfig::get_fav();
    let mut out = Vec::new();
    for (id, mtime, _path) in PeerConfig::get_vec_id_modified_time_path(&None) {
        // PeerConfig::load is the safe loader; it does NOT delete files on a
        // parse/decrypt failure (unlike batch_peers/Ab::load/Group::load).
        let cfg = PeerConfig::load(&id);
        let modified = mtime
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(json!({
            "id": id,
            "alias": cfg.options.get("alias").cloned().unwrap_or_default(),
            "username": cfg.info.username,
            "hostname": cfg.info.hostname,
            "platform": cfg.info.platform,
            "has_saved_password": !cfg.password.is_empty(),
            "favorite": favorites.contains(&id),
            "modified_time": modified,
        }));
    }
    let result = json!({ "count": out.len(), "peers": out });
    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------

fn ls(args: &Value) -> Result<String, String> {
    let peer_id = arg_str(args, "peer_id")?;
    let path = arg_str_opt(args, "path").unwrap_or_default();
    let include_hidden = args
        .get("include_hidden")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let s = start_session(&peer_id, ConnType::FILE_TRANSFER, Vec::new());
    let result = (|| {
        s.handler.wait_connected(CONNECT_TIMEOUT)?;
        let seq = s.handler.dir_seq();
        s.session.read_remote_dir(path.clone(), include_hidden);
        let (resolved_path, entries) = s.handler.wait_dir(seq, DIR_TIMEOUT)?;
        let items: Vec<Value> = entries
            .iter()
            .map(|e| {
                json!({
                    "name": e.name,
                    "type": if e.is_dir() { "dir" } else { "file" },
                    "entry_type": e.entry_type,
                    "size": e.size,
                    "modified_time": e.modified_time,
                    "is_hidden": e.is_hidden,
                })
            })
            .collect();
        let out = json!({
            "peer_id": peer_id,
            "path": resolved_path,
            "count": items.len(),
            "entries": items,
        });
        serde_json::to_string_pretty(&out).map_err(|e| e.to_string())
    })();
    s.session.close();
    result
}

// ---------------------------------------------------------------------------
// upload / download
// ---------------------------------------------------------------------------

fn upload(args: &Value) -> Result<String, String> {
    let peer_id = arg_str(args, "peer_id")?;
    let local_path = arg_str(args, "local_path")?;
    let remote_path = arg_str(args, "remote_path")?;

    // Fail fast with a clear message if the local source is missing.
    if !std::path::Path::new(&local_path).exists() {
        return Err(format!("local file does not exist: {}", local_path));
    }

    let s = start_session(&peer_id, ConnType::FILE_TRANSFER, Vec::new());
    let result = (|| {
        s.handler.wait_connected(CONNECT_TIMEOUT)?;
        let job_id = fs::get_next_job_id();
        // UPLOAD: is_remote = false. The client core reads local `path` and
        // writes remote `to` (verified against io_loop.rs SendFiles handling:
        // is_remote=false => "read {path} to remote {to}").
        s.session.send_files(
            job_id,
            fs::JobType::Generic as i32,
            local_path.clone(), // source (local)
            remote_path.clone(), // destination (remote)
            0,
            false,
            false, // is_remote
        );
        s.handler.wait_job(job_id, JOB_TIMEOUT)?;
        Ok(format!(
            "uploaded {} -> {}:{}",
            local_path, peer_id, remote_path
        ))
    })();
    s.session.close();
    result
}

fn download(args: &Value) -> Result<String, String> {
    let peer_id = arg_str(args, "peer_id")?;
    let remote_path = arg_str(args, "remote_path")?;
    let local_path = arg_str(args, "local_path")?;

    let s = start_session(&peer_id, ConnType::FILE_TRANSFER, Vec::new());
    let result = (|| {
        s.handler.wait_connected(CONNECT_TIMEOUT)?;
        let job_id = fs::get_next_job_id();
        // DOWNLOAD: is_remote = true. The client core reads remote `path` and
        // writes local `to` (verified against io_loop.rs SendFiles handling:
        // is_remote=true => "write to {to} from remote {path}").
        s.session.send_files(
            job_id,
            fs::JobType::Generic as i32,
            remote_path.clone(), // source (remote)
            local_path.clone(),  // destination (local)
            0,
            false,
            true, // is_remote
        );
        s.handler.wait_job(job_id, JOB_TIMEOUT)?;
        Ok(format!(
            "downloaded {}:{} -> {}",
            peer_id, remote_path, local_path
        ))
    })();
    s.session.close();
    result
}

// ---------------------------------------------------------------------------
// run (one terminal command)
// ---------------------------------------------------------------------------

fn next_nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", t, n)
}

/// Strip ANSI/VT escape sequences (CSI and OSC) that ConPTY emits, leaving
/// readable text. Not exhaustive, but handles the common cursor/color/erase
/// sequences and OSC title strings.
fn strip_ansi(input: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == 0x1b {
            // ESC
            if i + 1 < input.len() {
                match input[i + 1] {
                    b'[' => {
                        // CSI: ESC [ ... final byte in 0x40..=0x7e
                        i += 2;
                        while i < input.len() {
                            let c = input[i];
                            i += 1;
                            if (0x40..=0x7e).contains(&c) {
                                break;
                            }
                        }
                        continue;
                    }
                    b']' => {
                        // OSC: ESC ] ... terminated by BEL (0x07) or ESC \
                        i += 2;
                        while i < input.len() {
                            if input[i] == 0x07 {
                                i += 1;
                                break;
                            }
                            if input[i] == 0x1b
                                && i + 1 < input.len()
                                && input[i + 1] == b'\\'
                            {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                        continue;
                    }
                    _ => {
                        // Other ESC-x: skip the two bytes.
                        i += 2;
                        continue;
                    }
                }
            } else {
                break;
            }
        }
        // Drop other control bytes except \n and \t; keep \r folded out.
        if b == b'\r' {
            i += 1;
            continue;
        }
        if b == b'\n' || b == b'\t' || b >= 0x20 {
            out.push(b);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn run(args: &Value) -> Result<String, String> {
    let peer_id = arg_str(args, "peer_id")?;
    let command = arg_str(args, "command")?;
    let timeout = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_RUN_TIMEOUT);

    let tid: i32 = 1;
    let s = start_session(&peer_id, ConnType::TERMINAL, Vec::new());
    let result = (|| {
        s.handler.wait_connected(CONNECT_TIMEOUT)?;
        // open_terminal under the Windows helper can take 10-15s.
        s.session.open_terminal(tid, 50, 400);
        let _opened = s.handler.wait_terminal_opened(tid, TERMINAL_OPEN_TIMEOUT)?;

        // Let the initial prompt settle.
        std::thread::sleep(Duration::from_millis(600));

        let nonce = next_nonce();
        let begin = format!("__RD_BEGIN_{}__", nonce);
        let end_prefix = format!("__RD_END_{}_", nonce);

        // PowerShell: quiet the prompt, bracket the command with markers, and
        // print the exit code after it. One send per line, CR-terminated.
        // (Host default shell is assumed PowerShell on Windows; a cmd/bash host
        // would need shell detection -- noted as phase-2 work.)
        s.session.send_terminal_input(
            tid,
            "Remove-Module PSReadLine -EA 0; $ProgressPreference='SilentlyContinue'\r".to_string(),
        );
        s.session
            .send_terminal_input(tid, format!("Write-Output '{}'\r", begin));
        s.session
            .send_terminal_input(tid, format!("{}\r", command));
        s.session.send_terminal_input(
            tid,
            format!("Write-Output \"{}$LASTEXITCODE\"\r", end_prefix),
        );

        // Poll for the end marker.
        let deadline = std::time::Instant::now() + timeout;
        let mut text;
        loop {
            let (raw, closed, err) = s.handler.terminal_snapshot(tid);
            if let Some(e) = err {
                return Err(format!("terminal error: {}", e));
            }
            text = strip_ansi(&raw);
            if text.contains(&end_prefix) {
                break;
            }
            if closed {
                break;
            }
            if std::time::Instant::now() >= deadline {
                // Try to interrupt, then report what we have.
                s.session
                    .send_terminal_input(tid, "\u{0003}".to_string());
                s.session.close_terminal(tid);
                let partial = extract_between(&text, &begin, &end_prefix)
                    .unwrap_or_else(|| text.clone());
                return Err(format!(
                    "command timed out after {}s; partial output:\n{}",
                    timeout.as_secs(),
                    partial.trim()
                ));
            }
            std::thread::sleep(Duration::from_millis(150));
        }

        // Parse exit code from the end-marker line.
        let exit_code = parse_exit_code(&text, &end_prefix);
        let body = extract_between(&text, &begin, &end_prefix).unwrap_or_default();

        s.session.close_terminal(tid);

        let out = json!({
            "peer_id": peer_id,
            "exit_code": exit_code,
            "output": body.trim_end_matches('\n'),
        });
        serde_json::to_string_pretty(&out).map_err(|e| e.to_string())
    })();
    s.session.close();
    result
}

/// Text strictly between the BEGIN marker line and the END marker.
fn extract_between(text: &str, begin: &str, end_prefix: &str) -> Option<String> {
    let after_begin = text.split_once(begin)?.1;
    // Drop the remainder of the begin line (the CR/LF after the marker echo).
    let after_begin = after_begin.strip_prefix('\n').unwrap_or(after_begin);
    let body = match after_begin.split_once(end_prefix) {
        Some((b, _)) => b,
        None => after_begin,
    };
    // The command itself is echoed on its own line first; keep everything but
    // trim a leading echoed-newline run.
    Some(body.trim_start_matches('\n').to_string())
}

fn parse_exit_code(text: &str, end_prefix: &str) -> Option<i64> {
    let after = text.split(end_prefix).nth(1)?;
    let digits: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse::<i64>().ok()
}

// ---------------------------------------------------------------------------
// port_forward
// ---------------------------------------------------------------------------

// Keeps forward sessions (and thus their io_loop threads / listeners) alive for
// the life of the process.
fn forward_registry() -> &'static Mutex<Vec<AgentSession>> {
    static FORWARDS: OnceLock<Mutex<Vec<AgentSession>>> = OnceLock::new();
    FORWARDS.get_or_init(|| Mutex::new(Vec::new()))
}

fn port_forward(args: &Value) -> Result<String, String> {
    let peer_id = arg_str(args, "peer_id")?;
    let local_port = arg_i64(args, "local_port")?;
    let remote_host = arg_str(args, "remote_host")?;
    let remote_port = arg_i64(args, "remote_port")?;

    if local_port <= 0 || local_port > 65535 {
        return Err("local_port must be in 1..=65535".into());
    }
    if remote_port <= 0 || remote_port > 65535 {
        // port 0 is the RDP/mstsc path; reject it explicitly.
        return Err("remote_port must be in 1..=65535 (0 is reserved for RDP)".into());
    }

    // The single-mapping path in io_loop: args == [listen_port, remote_host,
    // remote_port] -> binds 127.0.0.1:listen_port and tunnels to the remote.
    let args_vec = vec![
        local_port.to_string(),
        remote_host.clone(),
        remote_port.to_string(),
    ];
    let s = start_session(&peer_id, ConnType::PORT_FORWARD, args_vec);

    // There is no on_connected for port forwarding (it runs through
    // port_forward::listen, not Remote::io_loop). Give the bind a moment; a bind
    // failure or peer error surfaces via msgbox("error", ...).
    std::thread::sleep(Duration::from_millis(1500));
    if let Some(e) = s.handler.first_error() {
        s.session.close();
        return Err(format!("port forward failed: {}", e));
    }

    let msg = format!(
        "listening on 127.0.0.1:{} -> {}:{} via {} (stays open until the agent exits)",
        local_port, remote_host, remote_port, peer_id
    );
    // Retain the session so its listener keeps running.
    forward_registry().lock().unwrap().push(s);
    Ok(msg)
}

// ---------------------------------------------------------------------------
// screenshot
// ---------------------------------------------------------------------------

fn arg_i32(args: &Value, key: &str) -> Result<i32, String> {
    let v = arg_i64(args, key)?;
    if v < i32::MIN as i64 || v > i32::MAX as i64 {
        return Err(format!("argument '{}' out of range", key));
    }
    Ok(v as i32)
}

fn screenshot(args: &Value) -> Result<Value, String> {
    let peer_id = arg_str(args, "peer_id")?;
    let display = args
        .get("display")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    let s = default_session(&peer_id)?;
    let result = (|| {
        if !s.session.is_screenshot_supported() {
            return Err(
                "peer does not support screenshot (host must run RustDesk >= 1.4.0)".to_string(),
            );
        }
        let sid = next_nonce();
        s.session.take_screenshot(display, sid.clone());
        s.handler.wait_screenshot(&sid, SCREENSHOT_TIMEOUT)?;

        // The PNG bytes are now in the client-side screenshot cache. Pull them
        // out to a temp file (ScreenshotAction::SaveAs, "0:<path>"), which also
        // clears the cache; returns "" on success, else an error string.
        let tmp = std::env::temp_dir().join(format!("rd-agent-shot-{}.png", sid));
        let tmp_str = tmp.to_string_lossy().to_string();
        let err = crate::client::screenshot::handle_screenshot(format!("0:{}", tmp_str));
        if !err.is_empty() {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("failed to save screenshot: {}", err));
        }
        let bytes = match std::fs::read(&tmp) {
            Ok(b) => b,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("failed to read screenshot file: {}", e));
            }
        };
        let _ = std::fs::remove_file(&tmp);
        let b64 = base64_encode(&bytes);
        Ok(json!([{
            "type": "image",
            "data": b64,
            "mimeType": "image/png"
        }]))
    })();
    if result.is_err() {
        // A failed capture may mean the shared session is dead; drop it so the
        // next call reconnects.
        evict_default_session(&peer_id);
    }
    result
}

// ---------------------------------------------------------------------------
// input: move_mouse / click / scroll / type_text / key
// ---------------------------------------------------------------------------

/// Run a closure against the peer's shared DEFAULT_CONN session, evicting the
/// cached session if the closure errors.
fn with_default_session<F>(peer_id: &str, f: F) -> Result<String, String>
where
    F: FnOnce(&AgentSession) -> Result<String, String>,
{
    let s = default_session(peer_id)?;
    let result = f(&*s);
    if result.is_err() {
        evict_default_session(peer_id);
    }
    result
}

fn move_mouse(args: &Value) -> Result<String, String> {
    let peer_id = arg_str(args, "peer_id")?;
    let x = arg_i32(args, "x")?;
    let y = arg_i32(args, "y")?;
    with_default_session(&peer_id, |s| {
        // MOUSE_TYPE_MOVE (0) with no button bits = an absolute move.
        s.session
            .send_mouse(MOUSE_TYPE_MOVE, x, y, false, false, false, false);
        Ok(format!("moved mouse to ({}, {}) on {}", x, y, peer_id))
    })
}

fn click(args: &Value) -> Result<String, String> {
    let peer_id = arg_str(args, "peer_id")?;
    let x = arg_i32(args, "x")?;
    let y = arg_i32(args, "y")?;
    let button = arg_str_opt(args, "button").unwrap_or_else(|| "left".to_string());
    let double = args.get("double").and_then(|v| v.as_bool()).unwrap_or(false);

    // Note: RustDesk's mouse protocol encodes left/right/wheel buttons. A
    // "middle" button is not represented, so map it to the wheel button value
    // (MOUSE_BUTTON_WHEEL = middle button on the host) for a middle click.
    let btn = match button.as_str() {
        "left" => MOUSE_BUTTON_LEFT,
        "right" => MOUSE_BUTTON_RIGHT,
        "middle" => MOUSE_BUTTON_WHEEL,
        other => return Err(format!("unknown button '{}' (use left/right/middle)", other)),
    };
    let down = (btn << 3) | MOUSE_TYPE_DOWN;
    let up = (btn << 3) | MOUSE_TYPE_UP;

    with_default_session(&peer_id, |s| {
        // Position the cursor, then press+release.
        s.session
            .send_mouse(MOUSE_TYPE_MOVE, x, y, false, false, false, false);
        s.session.send_mouse(down, x, y, false, false, false, false);
        s.session.send_mouse(up, x, y, false, false, false, false);
        if double {
            s.session.send_mouse(down, x, y, false, false, false, false);
            s.session.send_mouse(up, x, y, false, false, false, false);
        }
        Ok(format!(
            "{}{} click at ({}, {}) on {}",
            if double { "double " } else { "" },
            button,
            x,
            y,
            peer_id
        ))
    })
}

fn scroll(args: &Value) -> Result<String, String> {
    let peer_id = arg_str(args, "peer_id")?;
    let x = arg_i32(args, "x")?;
    let y = arg_i32(args, "y")?;
    let dx = args.get("dx").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let dy = args.get("dy").and_then(|v| v.as_i64()).unwrap_or(0) as i32;

    // Wheel events: mask = (MOUSE_BUTTON_WHEEL << 3) | MOUSE_TYPE_WHEEL, and
    // send_mouse treats x/y as the wheel delta for this event type.
    let mask = (MOUSE_BUTTON_WHEEL << 3) | MOUSE_TYPE_WHEEL;
    with_default_session(&peer_id, |s| {
        s.session
            .send_mouse(MOUSE_TYPE_MOVE, x, y, false, false, false, false);
        s.session.send_mouse(mask, dx, dy, false, false, false, false);
        Ok(format!(
            "scrolled (dx={}, dy={}) at ({}, {}) on {}",
            dx, dy, x, y, peer_id
        ))
    })
}

fn type_text(args: &Value) -> Result<String, String> {
    let peer_id = arg_str(args, "peer_id")?;
    let text = arg_str(args, "text")?;
    with_default_session(&peer_id, |s| {
        s.session.input_string(&text);
        Ok(format!("typed {} character(s) on {}", text.chars().count(), peer_id))
    })
}

fn key(args: &Value) -> Result<String, String> {
    let peer_id = arg_str(args, "peer_id")?;
    let name = arg_str(args, "name")?;
    let (mut alt, mut ctrl, mut shift, mut command) = (false, false, false, false);
    if let Some(mods) = args.get("modifiers").and_then(|v| v.as_array()) {
        for m in mods {
            match m.as_str().unwrap_or("") {
                "alt" => alt = true,
                "ctrl" => ctrl = true,
                "shift" => shift = true,
                "meta" | "command" | "win" => command = true,
                other => return Err(format!("unknown modifier '{}'", other)),
            }
        }
    }
    with_default_session(&peer_id, |s| {
        // press = true sends a down then up for the key. input_key silently
        // ignores an unknown multi-char name (not in KEY_MAP), so reject those
        // up front for a clear error.
        if name.chars().count() != 1 && !crate::client::KEY_MAP.contains_key(name.as_str()) {
            return Err(format!(
                "unknown key name '{}' (use a single character or a VK_* name)",
                name
            ));
        }
        s.session
            .input_key(&name, true, true, alt, ctrl, shift, command);
        Ok(format!("pressed key '{}' on {}", name, peer_id))
    })
}

// ---------------------------------------------------------------------------
// base64 (standard alphabet, with padding)
// ---------------------------------------------------------------------------
//
// Hand-rolled to avoid picking the wrong `base64` crate: the dependency tree
// carries two incompatible versions with different APIs, and this agent must
// build under `--locked`. A plain encoder is small, allocation-cheap and has no
// such ambiguity.

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // A PNG magic header round-trips to a known prefix.
        assert_eq!(base64_encode(&[0x89, b'P', b'N', b'G']), "iVBORw==");
    }
}
