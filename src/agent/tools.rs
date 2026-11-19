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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use base::fs;
use hbb_common::config::{LocalConfig, PeerConfig};
use hbb_common::rendezvous_proto::ConnType;

use crate::client::FileManager;
use crate::ui_session_interface::{io_loop, Session};

use super::handler::{AgentHandler, FileEntryInfo};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DIR_TIMEOUT: Duration = Duration::from_secs(30);
const JOB_TIMEOUT: Duration = Duration::from_secs(600);
const TERMINAL_OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(60);

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
        }
    ])
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub fn dispatch(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "list_peers" => list_peers(),
        "ls" => ls(args),
        "upload" => upload(args),
        "download" => download(args),
        "run" => run(args),
        "port_forward" => port_forward(args),
        other => Err(format!("unknown tool: {}", other)),
    }
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
        // print the exit code after it. Send it ALL as ONE send_terminal_input,
        // because some older RustDesk hosts deliver only the FIRST send of a
        // burst (observed on the PartyVR Dell build): four separate sends left
        // everything after the first undelivered, so the command never ran.
        // The markers are emitted with a string CONCATENATION ('__RD'+'_BEGIN_..')
        // so the literal, contiguous marker appears ONLY in the command's real
        // output, never in the terminal's echo of the input line -- otherwise the
        // poll loop and extract_between would match the echoed command instead of
        // the result. ';' separates the statements, the trailing CR submits, and
        // try/catch keeps the end marker printing even if the command throws.
        // Commands with embedded newlines are not supported by this one-line form.
        s.session.send_terminal_input(
            tid,
            format!(
                "Remove-Module PSReadLine -EA 0; $ProgressPreference='SilentlyContinue'; Write-Output ('__RD'+'_BEGIN_{nonce}__'); try {{ {command} }} catch {{ Write-Output ('ERR: ' + $_) }}; Write-Output ('__RD'+'_END_{nonce}_'+$LASTEXITCODE)\r",
                nonce = nonce, command = command
            ),
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
