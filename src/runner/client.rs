//! Local API client: one request per connection, and the atomic
//! start-on-demand of the supervisor.
//!
//! "The socket exists" and "connect succeeded" are not enough to decide that
//! a supervisor is ready, and two clients starting one at the same time must
//! not both win. The launcher takes an exclusive `flock` on a lock file next
//! to the socket, re-checks readiness under the lock, spawns `wyd serve`
//! detached, and waits for a real handshake before returning.

use crate::model::run::{RunId, RunSpec, RunState};
use crate::runner::{RunView, Stream};
use crate::server;
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Longest a single request may wait for a reply.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// How long to wait for a freshly spawned supervisor to answer.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Client {
    socket: PathBuf,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self {
            socket: server::socket_path(),
        }
    }

    /// Send one command and return its `data` payload.
    pub fn request(&self, cmd: &str, extra: Value) -> std::io::Result<Value> {
        let mut body = match extra {
            Value::Object(map) => Value::Object(map),
            _ => json!({}),
        };
        body["cmd"] = json!(cmd);
        let line = body.to_string();
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        let mut reader = BufReader::new(stream);
        let mut resp = String::new();
        reader.read_line(&mut resp)?;
        let parsed: Value = serde_json::from_str(resp.trim())
            .map_err(|e| std::io::Error::other(format!("bad reply: {e}: {resp}")))?;
        if parsed.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(parsed.get("data").cloned().unwrap_or(Value::Null))
        } else {
            Err(std::io::Error::other(
                parsed
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string(),
            ))
        }
    }

    pub fn start(&self, spec: &RunSpec, env: &[(String, String)]) -> std::io::Result<RunView> {
        let data = self.request("start_run", json!({ "spec": spec, "env": env }))?;
        view_from(data)
    }

    /// Bounded long-poll for a run newer than `after_revision`.
    pub fn get(
        &self,
        id: RunId,
        after_revision: Option<i64>,
        wait_ms: u64,
    ) -> std::io::Result<Option<RunView>> {
        let data = self.request(
            "get_run",
            json!({ "run_id": id.to_string(), "after_revision": after_revision, "wait_ms": wait_ms }),
        )?;
        match data.get("run") {
            Some(Value::Null) | None => Ok(None),
            Some(_) => view_from(data).map(Some),
        }
    }

    pub fn output(
        &self,
        id: RunId,
        stream: Stream,
        cursor: u64,
        max_bytes: usize,
    ) -> std::io::Result<Value> {
        self.request(
            "read_run_output",
            json!({
                "run_id": id.to_string(),
                "stream": stream.as_str(),
                "cursor": cursor,
                "max_bytes": max_bytes,
            }),
        )
    }

    pub fn cancel(&self, id: RunId) -> std::io::Result<Option<RunView>> {
        let data = self.request("cancel_run", json!({ "run_id": id.to_string() }))?;
        match data.get("run") {
            Some(Value::Null) | None => Ok(None),
            Some(_) => view_from(data).map(Some),
        }
    }
}

fn view_from(data: Value) -> std::io::Result<RunView> {
    let run = data.get("run").cloned().unwrap_or(Value::Null);
    serde_json::from_value(run).map_err(|e| std::io::Error::other(format!("bad run view: {e}")))
}

/// Is a supervisor answering on the socket right now?
fn handshake(socket: &Path) -> bool {
    match UnixStream::connect(socket) {
        Ok(mut stream) => {
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
            if stream.write_all(b"{\"cmd\":\"ping\"}\n").is_err() {
                return false;
            }
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).is_ok()
                && serde_json::from_str::<Value>(line.trim())
                    .ok()
                    .and_then(|v| v.get("ok").and_then(Value::as_bool))
                    == Some(true)
        }
        Err(_) => false,
    }
}

/// Start the supervisor if it is not already answering. Safe to call from any
/// number of clients at once.
pub fn ensure_supervisor() -> std::io::Result<()> {
    let socket = server::socket_path();
    if handshake(&socket) {
        return Ok(());
    }

    let lock_path = socket.with_file_name("wyd.lock");
    // The state directory may not exist yet on a fresh install.
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    // SAFETY: `lock` owns a valid fd for the duration of the call.
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    // Re-check under the lock: another client may have started it while we
    // waited, and then the socket is already live.
    if handshake(&socket) {
        return Ok(());
    }
    let _ = std::fs::remove_file(&socket); // stale socket from a dead daemon

    let exe = std::env::current_exe()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(socket.with_file_name("wyd-serve.log"))?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    // Detach: the supervisor outlives this CLI and must not take a terminal
    // Ctrl-C aimed at the run.
    crate::platform::pgroup::detach(&mut cmd);
    let child = cmd.spawn()?;

    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if handshake(&socket) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "supervisor did not become ready (pid {}); see {}",
            child.id(),
            socket.with_file_name("wyd-serve.log").display()
        ),
    ))
}

/// Wait for a run to reach a terminal state, re-polling the bounded long
/// poll. Returns the last view seen.
pub fn wait_for_finish(client: &Client, id: RunId) -> std::io::Result<RunView> {
    let mut revision = None;
    loop {
        let Some(view) = client.get(id, revision, crate::runner::MAX_WAIT_MS)? else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("run {id} not found"),
            ));
        };
        revision = Some(view.revision);
        if view.state == RunState::Finished {
            return Ok(view);
        }
    }
}
