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
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Must match `src/mcp.rs` PROTOCOL_VERSION.
const PROTOCOL_VERSION: &str = "2025-11-25";

/// The full store schema (`RuntimeStore::init`), so a pre-seeded store exists
/// before the server spawns — no CREATE/WAL race between the background
/// collector and the tools' store opens.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS boots (
    boot_id BLOB PRIMARY KEY,
    platform_epoch BLOB NOT NULL,
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    session_id INTEGER PRIMARY KEY,
    boot_id BLOB NOT NULL,
    agent TEXT NOT NULL,
    root_pid INTEGER NOT NULL,
    root_start_time INTEGER NOT NULL,
    project TEXT,
    started_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    ended_at INTEGER
);
CREATE TABLE IF NOT EXISTS resources (
    resource_id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    root_boot_id BLOB NOT NULL,
    root_pid INTEGER NOT NULL,
    root_start_time INTEGER NOT NULL,
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    stopped_at INTEGER
);
CREATE TABLE IF NOT EXISTS resource_members (
    resource_id INTEGER NOT NULL,
    boot_id BLOB NOT NULL,
    pid INTEGER NOT NULL,
    start_time INTEGER NOT NULL,
    PRIMARY KEY (resource_id, boot_id, pid, start_time)
);
CREATE TABLE IF NOT EXISTS exact_ownership (
    resource_id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS attribution_decisions (
    decision_id INTEGER PRIMARY KEY AUTOINCREMENT,
    resource_id INTEGER NOT NULL,
    observed_at INTEGER NOT NULL,
    resolver_version INTEGER NOT NULL,
    verdict TEXT NOT NULL,
    winner_session_id INTEGER
);
CREATE TABLE IF NOT EXISTS attribution_candidates (
    decision_id INTEGER NOT NULL,
    session_id INTEGER NOT NULL,
    anchor_kind TEXT NOT NULL,
    anchor_score INTEGER NOT NULL,
    project_score INTEGER NOT NULL,
    temporal_score INTEGER NOT NULL,
    relationship_score INTEGER NOT NULL,
    total_score INTEGER NOT NULL,
    rejected_reason TEXT,
    PRIMARY KEY (decision_id, session_id)
);
CREATE TABLE IF NOT EXISTS evidence (
    decision_id INTEGER NOT NULL,
    session_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (decision_id, session_id, kind)
);
CREATE TABLE IF NOT EXISTS session_aliases (
    vendor TEXT NOT NULL,
    vendor_session_id TEXT NOT NULL,
    session_id INTEGER NOT NULL,
    vendor_started_at INTEGER,
    vendor_ended_at INTEGER,
    PRIMARY KEY (vendor, vendor_session_id)
);
";

/// Replicates `RuntimeStore::default_path` so the test seeds the same store
/// the spawned server will open.
fn store_path(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/wyd/state.db")
    } else {
        home.join("xdg").join("wyd/state.db")
    }
}

/// Pre-create an initialized store with one recorded session, so
/// `list_sessions` returns a deterministic, successful answer.
fn seed_store(home: &Path) {
    let path = store_path(home);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO sessions
            (session_id, boot_id, agent, root_pid, root_start_time, project,
             started_at, last_seen_at, ended_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
        rusqlite::params![1u8, [7u8; 16], "omp", 100, 1000, "/src/wyd", 1000, 2000],
    )
    .unwrap();
}

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
        seed_store(&home);
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

    // 4. Unknown request method → a proper JSON-RPC error (-32601), not a
    //    silent success or a tool-style result.
    let unknown = c.request(&json!({ "jsonrpc": "2.0", "id": 4, "method": "prompts/list" }));
    assert_eq!(unknown["error"]["code"], -32601);
    assert_eq!(unknown["id"], 4);

    // 5. tools/call list_sessions → a successful result carrying the seeded
    //    session (the store is pre-created, so this is deterministic).
    let call = c.request(&json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": { "name": "list_sessions", "arguments": {} }
    }));
    assert_eq!(call["result"]["content"][0]["type"], "text");
    assert_eq!(
        call["result"]["isError"], false,
        "list_sessions must succeed against the seeded store: {}",
        call
    );
    assert!(
        call.get("error").is_none(),
        "a tool result is not a JSON-RPC error"
    );
    let text = call["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("omp"),
        "list_sessions must include the seeded session: {text}"
    );

    // 6. tools/call explain for pid 1 (init/launchd, always present) → a
    //    successful text result; it may own nothing, but must not error.
    let explain = c.request(&json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": { "name": "explain", "arguments": { "pid": 1 } }
    }));
    assert_eq!(explain["result"]["content"][0]["type"], "text");
    assert_eq!(
        explain["result"]["isError"], false,
        "explain(1) must succeed: {explain}"
    );
    assert!(
        explain.get("error").is_none(),
        "explain is a tool result, not a JSON-RPC error"
    );

    // 7. Client closes stdin → the server exits cleanly.
    c.shutdown();
}
