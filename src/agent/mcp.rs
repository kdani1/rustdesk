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
// STDOUT HAZARD (v1 note): two code paths in the reused client core write to the
// real stdout with `println!` -- the file-transfer digest error
// (src/client/io_loop.rs ~2605) and an RDP debug line (src/port_forward.rs ~148).
// Either could in principle interleave a stray line into this JSON-RPC stream.
// The RDP line only fires for RDP/mstsc forwarding (port 0), which this agent
// never requests. The digest line only fires on a rare file-transfer error. Logs
// from init_log go to a file/stderr, not stdout. For v1 we accept this and keep
// all our framing on a single locked stdout handle with an explicit flush per
// message. PHASE 2 should redirect STD_OUTPUT_HANDLE so those println! cannot
// reach the client at all.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

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
            match tools::dispatch(&name, &args) {
                Ok(text) => reply_result(
                    id,
                    json!({
                        "content": [ { "type": "text", "text": text } ],
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
    let s = v.to_string();
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    // One object per line, then flush so the client sees it promptly.
    let _ = lock.write_all(s.as_bytes());
    let _ = lock.write_all(b"\n");
    let _ = lock.flush();
}
