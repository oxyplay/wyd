//! End-to-end MCP conformance: run the real `wyd mcp` binary as a client and
//! drive the full legacy (2025) initialize-based handshake over stdio.
//!
//! The unit tests in `src/mcp.rs` cover `handle()` in isolation; this one
//! exercises the real process — framing, dispatch, serialization, lifecycle —
//! so a protocol-drift regression that breaks a real agent fails here first.
//!
//! `wyd mcp` is deliberately SDK-free (a few JSON-RPC methods by hand), so
//! this test is the guard against drift in the wire protocol we actually ship.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Must match `src/mcp.rs` PROTOCOL_VERSION.
const PROTOCOL_VERSION: &str = "2025-11-25";

/// A minimal stdio MCP client driving the real binary.
struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl McpClient {
    fn spawn() -> Self {
        // Hermetic: point HOME/XDG at a throwaway dir so the spawned server's
        // collect loop and store never touch the real user's state.
        let home = std::env::temp_dir().join(format!(
            "wyd-mcp-conformance-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_wyd"))
            .arg("mcp")
            .env("HOME", &home)
            .env("XDG_DATA_HOME", home.join("xdg"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn `wyd mcp`");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
        }
    }

    /// Write one JSON-RPC request line, read the single response line, and
    /// return it parsed.
    fn request(&mut self, req: &Value) -> Value {
        let stdin = self.stdin.as_mut().expect("client already closed");
        writeln!(stdin, "{req}").expect("write request");
        stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("read response — did the server die?");
        serde_json::from_str(&line).expect("response is valid JSON")
    }

    /// Write one message with no expectation of a response (a notification).
    fn send(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("client already closed");
        writeln!(stdin, "{msg}").expect("write message");
        stdin.flush().unwrap();
    }

    /// Close stdin; the server's read loop hits EOF and must exit cleanly.
    fn shutdown(&mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().expect("wait for server exit");
        assert!(
            status.success(),
            "`wyd mcp` must exit 0 on client close: {status}"
        );
    }
}

#[test]
fn full_handshake_as_a_client() {
    let mut c = McpClient::spawn();

    // 1. initialize → pinned protocol version, capabilities, serverInfo.
    let init = c.request(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "conformance", "version": "0" }
        }
    }));
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["id"], 1);
    assert_eq!(
        init["result"]["protocolVersion"], PROTOCOL_VERSION,
        "server must advertise its pinned version, not echo the client's"
    );
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert_eq!(init["result"]["serverInfo"]["name"], "wyd");

    // 2. notifications/initialized must never be answered. Send it, then a
    //    ping; the first response we see must be the ping's, not the
    //    notification's.
    c.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }));
    let ping = c.request(&json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }));
    assert_eq!(
        ping["id"], 2,
        "the initialized notification must produce no response"
    );
    assert_eq!(ping["result"], json!({}));

    // 3. tools/list → exactly the two tools, each with a JSON Schema object.
    let list = c.request(&json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }));
    let tools = list["result"]["tools"]
        .as_array()
        .expect("tools/list returns a tools array");
    let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    names.sort();
    assert_eq!(names, ["explain", "list_sessions"]);
    for t in tools {
        assert_eq!(
            t["inputSchema"]["type"], "object",
            "every tool inputSchema must be a JSON Schema object"
        );
    }

    // 4. tools/call list_sessions → a valid tool result envelope. Whether it
    //    returns data or `isError: true` depends on the store, which the
    //    background collector races on startup; the protocol contract is the
    //    envelope (text content, a boolean isError, no JSON-RPC error).
    let call = c.request(&json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": { "name": "list_sessions", "arguments": {} }
    }));
    assert_eq!(call["result"]["content"][0]["type"], "text");
    assert!(call["result"]["isError"].is_boolean());
    assert!(
        call.get("error").is_none(),
        "a tool result is not a JSON-RPC error"
    );

    // 5. Client closes stdin → the server exits cleanly.
    c.shutdown();
}
