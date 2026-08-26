//! `wyd mcp`: a minimal MCP (Model Context Protocol) server over stdio
//! (contract §18, agent-facing interface). Read-only: exposes Wyd's runtime
//! ownership queries to coding agents as tools.
//!
//! Transport is newline-delimited JSON-RPC 2.0 (the MCP stdio framing). No
//! SDK dependency — the handful of methods agents actually use are handled
//! directly: initialize, notifications/initialized, tools/list, tools/call,
//! ping.

use crate::store::RuntimeStore;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

pub fn serve_stdio() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(()); // client closed
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue, // malformed line: ignore
        };
        let Some(resp) = handle(&msg) else {
            continue; // notification
        };
        let mut out = stdout.lock();
        writeln!(out, "{resp}")?;
        out.flush()?;
    }
}

/// Process one JSON-RPC message. Returns `Some(response)` for requests,
/// `None` for notifications.
fn handle(msg: &Value) -> Option<String> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let id = msg.get("id");
    match method {
        "initialize" => {
            let params = msg.get("params").cloned().unwrap_or(json!({}));
            let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2024-11-05");
            Some(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": requested,
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "wyd", "version": env!("CARGO_PKG_VERSION") }
                    }
                })
                .to_string(),
            )
        }
        "notifications/initialized" => None,
        "ping" => Some(json!({ "jsonrpc": "2.0", "id": id, "result": {} }).to_string()),
        "tools/list" => Some(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tools() }
            })
            .to_string(),
        ),
        "tools/call" => {
            let id = id.cloned().unwrap_or(Value::Null);
            let params = msg.get("params").cloned().unwrap_or(json!({}));
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            Some(call_tool(&id, name, &args))
        }
        // Anything else (sampling, prompts, roots) is out of scope: respond
        // with an error so clients know it's unsupported rather than hang.
        _ => Some(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "content": [{"type":"text","text":"unsupported method"}], "isError": true }
            })
            .to_string(),
        ),
    }
}

fn tools() -> Vec<Value> {
    vec![
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
    ]
}

fn call_tool(id: &Value, name: &str, args: &Value) -> String {
    let store = match RuntimeStore::open(&RuntimeStore::default_path()) {
        Ok(s) => s,
        Err(e) => return tool_error(id, &e.to_string()),
    };
    let text = match name {
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
        handle(&v)
    }

    #[test]
    fn initialize_returns_capabilities() {
        let resp = handle_msg(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#)
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
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
        assert!(handle_msg(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    }

    #[test]
    fn unknown_method_is_an_error() {
        let resp = handle_msg(r#"{"jsonrpc":"2.0","id":3,"method":"prompts/list"}"#).unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["isError"], true);
    }
}
