// Minimal MCP (Model Context Protocol) server over stdio.
//
// Transport: newline-delimited JSON-RPC 2.0 -- exactly one JSON object per line,
// which is the simplest framing Claude Code's stdio MCP client accepts. We
// hand-roll it with serde_json because no rmcp/jsonrpc crate is in the locked
// dependency graph and `--locked` forbids adding one.
//
// We implement just enough of MCP: `initialize`, `tools/list`, `tools/call`, and
// we ignore notifications (methods with no `id`, e.g. notifications/initialized).
//
// STDOUT HAZARD (resolved in phase 2): two code paths in the reused client core
// write to the real stdout with `println!` -- the file-transfer digest error
// (src/client/io_loop.rs ~2605, reachable on the download path) and an RDP debug
// line (src/port_forward.rs ~148). Either could interleave a stray line into
// this JSON-RPC stream. This is now handled structurally: super::stdio_guard
// (installed first thing in run()) saves the real stdout handle for the writer
// below and redirects the process STD_OUTPUT_HANDLE elsewhere, so those println!
// can no longer reach the protocol pipe. The writer here never touches
// io::stdout(); it goes through stdio_guard::write_all to the saved handle.

use std::io::BufRead;

use serde_json::{json, Value};

use super::stdio_guard;
use super::tools;

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Run the stdio server loop on the current (plain, non-async) thread. Returns
/// when stdin reaches EOF.
pub fn serve() {
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                eprintln!("[rustdesk-agent] stdin read error: {}", e);
                break;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                // Parse error, no id available.
                write_response(&json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": format!("parse error: {}", e) }
                }));
                continue;
            }
        };
        handle_message(&req);
    }
}

fn handle_message(req: &Value) {
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = req.get("id").cloned();

    // Notifications have no id; we never answer them.
    let is_notification = id.is_none();

    match method {
        "initialize" => {
            reply_result(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "serverInfo": { "name": "rustdesk-agent", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "tools": {} }
                }),
            );
        }
        "tools/list" => {
            reply_result(id, json!({ "tools": tools::tool_definitions() }));
        }
        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or(Value::Null);
            let name = params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            // Guard the whole call so a panic in a tool (panic=abort would kill
            // the process) cannot take the server down for a recoverable error:
            // we validate/handle errors inside, returning them as tool errors.
            // dispatch returns the full MCP `content` array on success (a text
            // block for most tools, an image block for `screenshot`), or an
            // error string we wrap as a text block with isError = true.
            match tools::dispatch(&name, &args) {
                Ok(content) => reply_result(
                    id,
                    json!({
                        "content": content,
                        "isError": false
                    }),
                ),
                Err(text) => reply_result(
                    id,
                    json!({
                        "content": [ { "type": "text", "text": text } ],
                        "isError": true
                    }),
                ),
            }
        }
        "ping" => {
            reply_result(id, json!({}));
        }
        _ => {
            if is_notification {
                // e.g. notifications/initialized, notifications/cancelled -> ignore.
                return;
            }
            reply_result_err(id, -32601, &format!("method not found: {}", method));
        }
    }
}

fn reply_result(id: Option<Value>, result: Value) {
    let id = match id {
        Some(v) => v,
        None => return, // notification; no reply
    };
    write_response(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

fn reply_result_err(id: Option<Value>, code: i64, message: &str) {
    let id = match id {
        Some(v) => v,
        None => return,
    };
    write_response(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    }));
}

fn write_response(v: &Value) {
    // One object per line, written to the saved stdout handle via stdio_guard
    // (NOT io::stdout(), whose STD_OUTPUT_HANDLE has been redirected away). The
    // underlying handle is unbuffered, so the client sees each message promptly.
    let mut buf = v.to_string().into_bytes();
    buf.push(b'\n');
    stdio_guard::write_all(&buf);
}
