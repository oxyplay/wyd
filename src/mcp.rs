//! `wyd mcp`: a minimal MCP (Model Context Protocol) server over stdio
//! (contract §18, agent-facing interface). Read-only: exposes Wyd's runtime
//! ownership queries to coding agents as tools.
//!
//! Transport is newline-delimited JSON-RPC 2.0 (the MCP stdio framing). No
//! SDK dependency — the handful of methods agents actually use are handled
//! directly: initialize, notifications/initialized, tools/list, tools/call,
//! ping.

use crate::server;
use crate::store::RuntimeStore;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::thread;

/// The MCP protocol version this server speaks. We implement the legacy
/// (2025) initialize-based protocol, not the 2026 `server/discover` era, so
/// this is pinned and never echoed back to a client (a client asking for a
/// newer version gets this and must fall back).
const PROTOCOL_VERSION: &str = "2025-11-25";

/// Read tools are always available; the execution tools (`start_run`,
/// `cancel_run`) exist only when the user launched this connection with
/// `--allow-run`. The MCP surface is never a way around that choice.
pub fn serve_stdio(allow_run: bool) -> std::io::Result<()> {
    // Keep the store fresh while serving, even with no `wyd serve`/TUI open —
    // but only if no daemon is already collecting, to avoid duplicate writers.
    if !server::serve_alive() {
        thread::spawn(server::collect_loop);
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(()); // client closed (kills the collect thread with the process)
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue, // malformed line: ignore
        };
        let Some(resp) = handle(&msg, allow_run) else {
            continue; // notification
        };
        let mut out = stdout.lock();
        writeln!(out, "{resp}")?;
        out.flush()?;
    }
}

/// Process one JSON-RPC message. Returns `Some(response)` for requests,
/// `None` for notifications (JSON-RPC: a notification has no id and must
/// never be answered).
fn handle(msg: &Value, allow_run: bool) -> Option<String> {
    let id = msg.get("id")?.clone(); // no id → notification → no response
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "initialize" => Some(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "wyd", "version": env!("CARGO_PKG_VERSION") }
                }
            })
            .to_string(),
        ),
        "ping" => Some(json!({ "jsonrpc": "2.0", "id": id, "result": {} }).to_string()),
        "tools/list" => Some(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tools(allow_run) }
            })
            .to_string(),
        ),
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or(json!({}));
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            Some(call_tool(&id, name, &args, allow_run))
        }
        // Unknown request method: a proper JSON-RPC error, not a tool-style
        // isError result.
        _ => Some(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "Method not found" }
            })
            .to_string(),
        ),
    }
}

fn tools(allow_run: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "list_sessions",
            "description": "List recorded coding-agent runtime sessions (id, agent, project, state, started_at).",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "explain",
            "description": "Explain which session owns a process (by pid): origin session, project, state.",
            "inputSchema": {
                "type": "object",
                "properties": { "pid": { "type": "integer", "description": "process id" } },
                "required": ["pid"]
            }
        }),
        json!({
            "name": "list_runs",
            "description": "List managed runs started through wyd (id, state, command, duration).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "state": { "type": "string", "description": "queued|starting|running|stopping|finished" },
                    "project": { "type": "string", "description": "absolute project root" },
                    "session": { "type": "string", "description": "session id (hex)" },
                    "limit": { "type": "integer" }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_run",
            "description": "Get one managed run's state, result and cleanup report. Optionally waits for a newer revision.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "run_id": { "type": "string" },
                    "after_revision": { "type": "integer", "description": "wait for a revision newer than this" },
                    "wait_ms": { "type": "integer", "description": "bounded wait, max 60000" }
                },
                "required": ["run_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "read_run_output",
            "description": "Read a bounded chunk of a run's captured stdout or stderr from a byte cursor.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "run_id": { "type": "string" },
                    "stream": { "type": "string", "description": "stdout|stderr" },
                    "cursor": { "type": "integer" },
                    "max_bytes": { "type": "integer" }
                },
                "required": ["run_id"],
                "additionalProperties": false
            }
        }),
    ];
    if allow_run {
        tools.push(json!({
            "name": "start_run",
            "description": "Start a command on the HOST through wyd's supervisor (not sandboxed). Returns a run id; execution continues even if this connection drops.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "request_id": { "type": "string", "description": "idempotency key" },
                    "argv": { "type": "array", "items": { "type": "string" }, "description": "executable and arguments, no implicit shell" },
                    "cwd": { "type": "string", "description": "absolute working directory" },
                    "timeout_ms": { "type": "integer" },
                    "session_id": { "type": "string", "description": "originating session id (metadata, not authorization)" },
                    "project_root": { "type": "string" }
                },
                "required": ["request_id", "argv", "cwd"],
                "additionalProperties": false
            }
        }));
        tools.push(json!({
            "name": "cancel_run",
            "description": "Ask a run to stop. Idempotent: repeating it returns the current status.",
            "inputSchema": {
                "type": "object",
                "properties": { "run_id": { "type": "string" } },
                "required": ["run_id"],
                "additionalProperties": false
            }
        }));
    }
    tools
}

fn call_tool(id: &Value, name: &str, args: &Value, allow_run: bool) -> String {
    let store = match RuntimeStore::open(&RuntimeStore::default_path()) {
        Ok(s) => s,
        Err(e) => return tool_error(id, &e.to_string()),
    };
    let text = match name {
        "list_runs" | "get_run" | "read_run_output" | "start_run" | "cancel_run" => {
            match run_tool(name, args, allow_run) {
                Ok(text) => text,
                Err(e) => return tool_error(id, &e.to_string()),
            }
        }
        "list_sessions" => match store.sessions() {
            Ok(s) => serde_json::to_string_pretty(
                &s.iter()
                    .map(|x| {
                        json!({
                            "id": x.id.to_string(),
                            "agent": x.agent,
                            "project": x.project,
                            "state": if x.ended_at.is_some() { "ended" } else { "active" },
                            "started_at": x.started_at,
                        })
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_else(|_| "[]".into()),
            Err(e) => return tool_error(id, &e.to_string()),
        },
        "explain" => {
            let pid = args.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
            // Resolve boot + live start_time (like `wyd why`).
            match crate::server::explain_pid(pid) {
                Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into()),
                Err(e) => return tool_error(id, &e.to_string()),
            }
        }
        other => return tool_error(id, &format!("unknown tool {other:?}")),
    };
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": text }], "isError": false }
    })
    .to_string()
}

/// Managed-run tools. Read tools work on any connection; `start_run` and
/// `cancel_run` require `--allow-run`, and the gate is checked here, not in
/// the schema alone.
fn run_tool(name: &str, args: &Value, allow_run: bool) -> std::io::Result<String> {
    use crate::model::run::RunSpec;
    use crate::runner::client::{self, Client};

    match name {
        "start_run" if !allow_run => {
            return Err(std::io::Error::other(
                "start_run is disabled: restart wyd mcp with --allow-run",
            ));
        }
        "cancel_run" if !allow_run => {
            return Err(std::io::Error::other(
                "cancel_run is disabled: restart wyd mcp with --allow-run",
            ));
        }
        _ => {}
    }

    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".into());
    match name {
        "list_runs" => {
            // Durable read: works with or without a live supervisor.
            let store = RuntimeStore::open(&RuntimeStore::default_path())?;
            let filter = crate::store::RunFilter {
                project: args
                    .get("project")
                    .and_then(Value::as_str)
                    .map(String::from),
                session: args.get("session").and_then(Value::as_str).map(|s| {
                    crate::model::session::RuntimeSessionId::from_u64(
                        u64::from_str_radix(s, 16).unwrap_or(0),
                    )
                }),
                state: args
                    .get("state")
                    .and_then(Value::as_str)
                    .and_then(crate::model::run::RunState::parse),
                limit: args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize),
            };
            let runs: Vec<crate::runner::RunView> = store
                .run_list(&filter)?
                .iter()
                .map(crate::runner::RunView::from_record)
                .collect();
            Ok(pretty(&serde_json::to_value(runs)?))
        }
        "get_run" => {
            let id = run_id_arg(args)?;
            if !crate::server::serve_alive() {
                // No supervisor: a finished run is still readable.
                let store = RuntimeStore::open(&RuntimeStore::default_path())?;
                return Ok(match store.run_get(id)? {
                    Some(record) => pretty(&serde_json::to_value(
                        crate::runner::RunView::from_record(&record),
                    )?),
                    None => "null".into(),
                });
            }
            let after = args.get("after_revision").and_then(Value::as_i64);
            let wait = args.get("wait_ms").and_then(Value::as_u64).unwrap_or(0);
            match Client::new().get(id, after, wait)? {
                Some(view) => Ok(pretty(&serde_json::to_value(view)?)),
                None => Ok("null".into()),
            }
        }
        "read_run_output" => {
            let id = run_id_arg(args)?;
            let stream = match args.get("stream").and_then(Value::as_str) {
                Some("stderr") => crate::runner::logs::Stream::Stderr,
                _ => crate::runner::logs::Stream::Stdout,
            };
            let cursor = args.get("cursor").and_then(Value::as_u64).unwrap_or(0);
            let max_bytes = args
                .get("max_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(64 * 1024)
                .clamp(1, 1024 * 1024) as usize;
            if crate::server::serve_alive() {
                return Ok(pretty(
                    &Client::new().output(id, stream, cursor, max_bytes)?,
                ));
            }
            // No supervisor: the retained file is still readable, so reading
            // a finished run's output never needs a daemon.
            let store = RuntimeStore::open(&RuntimeStore::default_path())?;
            let Some(record) = store.run_get(id)? else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("run {id} not found"),
                ));
            };
            let paths = crate::runner::logs::RunPaths::new(
                RuntimeStore::default_path().with_file_name("runs"),
            );
            let chunk = crate::runner::logs::read_chunk(
                &paths.stream_file(id.0, stream),
                cursor,
                max_bytes,
            )?;
            let truncated = match stream {
                crate::runner::logs::Stream::Stdout => record.logs.stdout_truncated,
                crate::runner::logs::Stream::Stderr => record.logs.stderr_truncated,
            };
            Ok(pretty(&json!({
                "stream": stream.as_str(),
                "data": chunk.data,
                "next_cursor": chunk.next_cursor,
                "truncated": truncated,
                "eof": chunk.eof,
            })))
        }
        "start_run" => {
            client::ensure_supervisor()?;
            let argv: Vec<String> = serde_json::from_value(
                args.get("argv")
                    .cloned()
                    .ok_or_else(|| std::io::Error::other("argv is required"))?,
            )
            .map_err(|e| std::io::Error::other(format!("argv must be a string array: {e}")))?;
            let cwd = args
                .get("cwd")
                .and_then(Value::as_str)
                .ok_or_else(|| std::io::Error::other("cwd is required"))?;
            let request_id = args
                .get("request_id")
                .and_then(Value::as_str)
                .ok_or_else(|| std::io::Error::other("request_id is required"))?;
            let mut spec = RunSpec::new(request_id, argv, std::path::PathBuf::from(cwd));
            if let Some(ms) = args.get("timeout_ms").and_then(Value::as_u64) {
                spec.timeout = std::time::Duration::from_millis(ms.max(1));
            }
            if let Some(root) = args.get("project_root").and_then(Value::as_str) {
                spec.project_root = Some(std::path::PathBuf::from(root));
            }
            if let Some(sid) = args.get("session_id").and_then(Value::as_str) {
                spec.session_id = Some(crate::model::session::RuntimeSessionId::from_u64(
                    u64::from_str_radix(sid, 16).unwrap_or(0),
                ));
                spec.session_origin = crate::runner::caller_session(spec.session_id);
            }
            // The connection's own environment, never a stale daemon env.
            let env: Vec<(String, String)> = std::env::vars().collect();
            let view = Client::new().start(&spec, &env)?;
            Ok(pretty(&serde_json::to_value(view)?))
        }
        "cancel_run" => {
            client::ensure_supervisor()?;
            let id = run_id_arg(args)?;
            match Client::new().cancel(id)? {
                Some(view) => Ok(pretty(&serde_json::to_value(view)?)),
                None => Ok("null".into()),
            }
        }
        other => Err(std::io::Error::other(format!("unknown run tool {other:?}"))),
    }
}

fn run_id_arg(args: &Value) -> std::io::Result<crate::model::run::RunId> {
    let raw = args
        .get("run_id")
        .ok_or_else(|| std::io::Error::other("run_id is required"))?;
    let id = match raw {
        Value::String(s) => s.parse::<i64>().ok(),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
    .ok_or_else(|| std::io::Error::other("run_id must be a decimal id"))?;
    Ok(crate::model::run::RunId(id))
}

fn tool_error(id: &Value, msg: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": msg }], "isError": true }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle_msg(line: &str) -> Option<String> {
        let v: Value = serde_json::from_str(line).unwrap();
        handle(&v, false)
    }

    #[test]
    fn initialize_returns_capabilities() {
        let resp = handle_msg(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], "wyd");
    }

    #[test]
    fn tools_list_lists_tools() {
        let resp = handle_msg(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        let names: Vec<&str> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"list_sessions"));
        assert!(names.contains(&"explain"));
    }

    #[test]
    fn notifications_get_no_response() {
        // A notification has no id and must never be answered — even for an
        // unknown method.
        assert!(handle_msg(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        assert!(handle_msg(r#"{"jsonrpc":"2.0","method":"prompts/list"}"#).is_none());
    }

    #[test]
    fn unknown_request_method_gets_json_rpc_error() {
        let resp = handle_msg(r#"{"jsonrpc":"2.0","id":3,"method":"prompts/list"}"#).unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["error"]["code"], -32601, "Method not found");
    }

    #[test]
    fn initialize_pins_version_not_echoes() {
        // A client asking for a future version must get our supported version,
        // not an echo of the request.
        let resp = handle_msg(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
    }
}
