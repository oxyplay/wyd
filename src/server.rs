//! `wyd serve`: the local supervisor and ownership API over a Unix socket
//! (contract §15–16). Keeps the store fresh by running the collector on a
//! loop, serves line-delimited JSON requests to local clients, and owns every
//! managed run's process groups.
//!
//! The socket is 0600 in the user's state directory and the protocol is
//! line-delimited JSON: `{"cmd": ...}` in, `{"ok":...}` out. It is not, and
//! must never become, a TCP shell service.

use crate::collect::{self, OwnershipTracker};
use crate::model::process::ProcessIdentity;
use crate::platform::{BootIdentityProvider, SystemBoot};
use crate::runner::{Supervisor, logs::Stream};
use crate::scanner::{ProcessScanner, processes::SysinfoProcessScanner};
use crate::store::RuntimeStore;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_secs(2);
/// Longest a client may take to send one request line.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Concurrent connection threads. A stuck client occupies one, never the
/// acceptor.
const MAX_CLIENTS: usize = 64;
/// A supervisor started on demand exits after this long without clients or
/// runs. An explicit `wyd serve` never does.
const IDLE_EXIT: Duration = Duration::from_secs(300);
/// How long a graceful shutdown waits for active runs to stop.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Set by the SIGTERM/SIGINT handler (an atomic store is async-signal-safe).
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn request_shutdown(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_signal_handler() {
    // SAFETY: the handler only stores an atomic flag.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            request_shutdown as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            request_shutdown as *const () as libc::sighandler_t,
        );
    }
}

/// On SIGTERM/SIGINT, stop the active runs first, then leave. A run that
/// outlives the grace period is reported rather than silently abandoned; the
/// next supervisor start recovers it as `supervisor_lost`.
fn spawn_shutdown(supervisor: Arc<Supervisor>) {
    thread::spawn(move || {
        while !SHUTDOWN.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
        }
        let active = supervisor.active_runs();
        if active > 0 {
            eprintln!("wyd serve: stopping {active} active run(s)");
            supervisor.cancel_all();
            let deadline = Instant::now() + SHUTDOWN_GRACE;
            while supervisor.active_runs() > 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(100));
            }
            let left = supervisor.active_runs();
            if left > 0 {
                eprintln!("wyd serve: {left} run(s) still active; exiting anyway");
            }
        }
        let _ = std::fs::remove_file(socket_path());
        std::process::exit(0);
    });
}

/// The Unix socket lives next to the state database.
pub fn socket_path() -> PathBuf {
    RuntimeStore::default_path().with_file_name("wyd.sock")
}

/// Is a `wyd serve` daemon currently listening on the socket?
pub fn serve_alive() -> bool {
    let path = socket_path();
    path.exists() && UnixStream::connect(&path).is_ok()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run the daemon: a background collector loop, a Unix-socket acceptor and
/// the run supervisor. Single-instance: refuses to start if another
/// `wyd serve` is already listening on the socket, instead of silently
/// replacing it. `auto` marks a supervisor started on demand by a client; it
/// exits once idle, while an explicit `wyd serve` stays in the foreground.
pub fn serve(auto: bool) -> std::io::Result<()> {
    let path = socket_path();
    if serve_alive() {
        eprintln!("wyd serve already running ({})", path.display());
        return Ok(());
    }
    let _ = std::fs::remove_file(&path); // stale socket from a dead run
    let pid_path = RuntimeStore::default_path().with_file_name("wyd.pid");
    if let Some(dir) = pid_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&pid_path, std::process::id().to_string())?;

    let listener = UnixListener::bind(&path)?;
    // Restrict the socket to this user regardless of umask.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    eprintln!("wyd serve on {} (local runtime API)", path.display());

    let supervisor = Supervisor::open()?;
    thread::spawn(|| collect_loop(None));
    install_signal_handler();
    spawn_shutdown(Arc::clone(&supervisor));

    let clients = Arc::new(AtomicUsize::new(0));
    let last_seen = Arc::new(AtomicU64::new(now()));
    if auto {
        spawn_idle_exit(Arc::clone(&supervisor), Arc::clone(&last_seen));
    }

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept: {e}");
                continue;
            }
        };
        if clients.load(Ordering::SeqCst) >= MAX_CLIENTS {
            // Refuse rather than queue: a stuck client must not stall others.
            let mut stream = stream;
            let _ = writeln!(
                stream,
                "{}",
                err_json("too many concurrent clients; try again")
            );
            continue;
        }
        clients.fetch_add(1, Ordering::SeqCst);
        let supervisor = Arc::clone(&supervisor);
        let clients = Arc::clone(&clients);
        let last_seen = Arc::clone(&last_seen);
        thread::spawn(move || {
            if let Err(e) = handle(stream, &supervisor) {
                eprintln!("client: {e}");
            }
            last_seen.store(now(), Ordering::SeqCst);
            clients.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

/// Exit an on-demand supervisor once nothing needs it. Active runs keep it
/// alive: a client disconnecting must never stop a run.
fn spawn_idle_exit(supervisor: Arc<Supervisor>, last_seen: Arc<AtomicU64>) {
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(30));
            if supervisor.active_runs() > 0 {
                continue;
            }
            if now().saturating_sub(last_seen.load(Ordering::SeqCst)) >= IDLE_EXIT.as_secs() {
                std::process::exit(0);
            }
        }
    });
}

/// The most recent collected snapshot, published for other in-process
/// consumers so there is exactly one process scanner per process.
#[derive(Debug, Default)]
pub struct SnapshotSink {
    slot: parking_lot::Mutex<Option<crate::model::RuntimeSnapshot>>,
    ready: parking_lot::Condvar,
}

impl SnapshotSink {
    fn publish(&self, snap: crate::model::RuntimeSnapshot) {
        *self.slot.lock() = Some(snap);
        self.ready.notify_all();
    }

    /// The latest snapshot, waiting up to `timeout` for the first one.
    pub fn latest(&self, timeout: Duration) -> Option<crate::model::RuntimeSnapshot> {
        let mut guard = self.slot.lock();
        if guard.is_none() {
            self.ready.wait_for(&mut guard, timeout);
        }
        guard.clone()
    }
}

/// Collect + persist on a loop so the API stays fresh even with no TUI open.
/// Shared by `wyd serve`, `wyd mcp` and `wyd web`. When a sink is given, this
/// is the process's only scanner: consumers read the published snapshot
/// instead of scanning again.
pub fn collect_loop(sink: Option<Arc<SnapshotSink>>) {
    let mut tracker = OwnershipTracker::new();
    loop {
        let mut snap = collect::snapshot();
        tracker.record(&snap.processes, &snap.logical_items);
        tracker.layer_session_leftovers(&mut snap.logical_items, &snap.processes);
        if let Some(sink) = &sink {
            sink.publish(snap);
        }
        thread::sleep(REFRESH);
    }
}

fn handle(stream: UnixStream, supervisor: &Arc<Supervisor>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    // Bounded read: a client cannot make the daemon buffer unbounded input.
    if reader
        .by_ref()
        .take(crate::model::run::MAX_REQUEST_BYTES as u64)
        .read_line(&mut line)?
        == 0
    {
        return Ok(()); // client closed
    }
    let resp = match RuntimeStore::open(&RuntimeStore::default_path()) {
        Ok(mut store) => dispatch(&mut store, supervisor, &line),
        Err(e) => err_json(&e.to_string()),
    };
    let mut w = stream;
    writeln!(w, "{resp}")?;
    Ok(())
}

/// Parse one request line and answer it. `&mut` because `explain` resolves
/// (and may persist) the boot id. Run commands go to the supervisor, which
/// owns the process groups; everything else reads the ownership store.
fn dispatch(store: &mut RuntimeStore, supervisor: &Arc<Supervisor>, line: &str) -> String {
    let req: Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(e) => return err_json(&e.to_string()),
    };
    let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("");
    match cmd {
        "ping" => ok_json(json!({
            "pong": true,
            "version": env!("CARGO_PKG_VERSION"),
            "supervisor": supervisor.identity(),
        })),
        "capabilities" => ok_json(json!({ "capabilities": supervisor.capabilities() })),
        "get_capacity" => ok_json(json!({ "capacity": supervisor.capacity() })),
        "set_limits" => match set_limits(supervisor, &req) {
            Ok(v) => ok_json(v),
            Err(e) => err_json(&e.to_string()),
        },
        "start_run" => match start_run(supervisor, &req) {
            Ok(v) => ok_json(v),
            Err(e) => err_json(&e.to_string()),
        },
        "get_run" => match get_run(supervisor, &req) {
            Ok(v) => ok_json(v),
            Err(e) => err_json(&e.to_string()),
        },
        "read_run_output" => match read_run_output(supervisor, &req) {
            Ok(v) => ok_json(v),
            Err(e) => err_json(&e.to_string()),
        },
        "list_runs" => match list_runs(supervisor, &req) {
            Ok(v) => ok_json(v),
            Err(e) => err_json(&e.to_string()),
        },
        "cancel_run" => match cancel_run(supervisor, &req) {
            Ok(v) => ok_json(v),
            Err(e) => err_json(&e.to_string()),
        },
        "list_sessions" => match store.sessions() {
            Ok(s) => ok_json(json!({
                "sessions": s.iter().map(session_json).collect::<Vec<_>>()
            })),
            Err(e) => err_json(&e.to_string()),
        },
        "get_session" => {
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            let id = crate::model::session::RuntimeSessionId::from_u64(
                u64::from_str_radix(id, 16).unwrap_or(0),
            );
            match store.session_record(id) {
                Ok(Some(s)) => ok_json(json!({ "session": session_json(&s) })),
                Ok(None) => ok_json(json!({ "session": null })),
                Err(e) => err_json(&e.to_string()),
            }
        }
        "explain" => {
            let pid = req.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
            match explain(store, pid) {
                Ok(v) => ok_json(v),
                Err(e) => err_json(&e.to_string()),
            }
        }
        "session_start" => match session_start(store, &req) {
            Ok(v) => ok_json(v),
            Err(e) => err_json(&e.to_string()),
        },
        "session_end" => match session_end(store, &req) {
            Ok(v) => ok_json(v),
            Err(e) => err_json(&e.to_string()),
        },
        other => err_json(&format!("unknown command {other:?}")),
    }
}

/// Replace admission limits without touching active runs.
fn set_limits(supervisor: &Arc<Supervisor>, req: &Value) -> std::io::Result<Value> {
    let raw = req
        .get("limits")
        .ok_or_else(|| std::io::Error::other("set_limits needs a limits object"))?;
    let mut limits = supervisor.capacity().limits;
    if let Some(v) = raw.get("max_parallel").and_then(Value::as_u64) {
        limits.max_parallel = (v as usize).max(1);
    }
    if let Some(v) = raw.get("max_parallel_per_project").and_then(Value::as_u64) {
        limits.max_parallel_per_project = (v as usize).max(1);
    }
    if let Some(v) = raw.get("max_queued").and_then(Value::as_u64) {
        limits.max_queued = v as usize;
    }
    if let Some(v) = raw.get("queue_timeout_secs").and_then(Value::as_u64) {
        limits.queue_timeout_secs = v;
    }
    if let Some(v) = raw.get("memory_budget_bytes").and_then(Value::as_u64) {
        limits.memory_budget_bytes = v;
    }
    if let Some(v) = raw.get("default_run_memory_bytes").and_then(Value::as_u64) {
        limits.default_run_memory_bytes = v;
    }
    if let Some(v) = raw.get("starvation_after_secs").and_then(Value::as_u64) {
        limits.starvation_after_secs = v;
    }
    if let Some(v) = raw.get("cpu_budget_millicores") {
        limits.cpu_budget_millicores = v.as_u64().map(|v| v as u32);
    }
    if let Some(v) = raw.get("pids_budget") {
        limits.pids_budget = v.as_u64().map(|v| v as u32);
    }
    let applied = supervisor.set_limits(crate::runner::scheduler::Limits {
        max_parallel: limits.max_parallel,
        max_parallel_per_project: limits.max_parallel_per_project,
        max_queued: limits.max_queued,
        queue_timeout: Duration::from_secs(limits.queue_timeout_secs),
        memory_budget_bytes: limits.memory_budget_bytes,
        default_run_memory_bytes: limits.default_run_memory_bytes,
        starvation_after: Duration::from_secs(limits.starvation_after_secs),
        cpu_budget_millicores: limits.cpu_budget_millicores,
        pids_budget: limits.pids_budget,
    });
    Ok(json!({ "capacity": applied }))
}

fn start_run(supervisor: &Arc<Supervisor>, req: &Value) -> std::io::Result<Value> {
    let spec: crate::model::run::RunSpec = serde_json::from_value(
        req.get("spec")
            .cloned()
            .ok_or_else(|| std::io::Error::other("start_run needs a spec"))?,
    )
    .map_err(|e| std::io::Error::other(format!("bad spec: {e}")))?;
    let env: Vec<(String, String)> = match req.get("env") {
        Some(Value::Array(_)) => serde_json::from_value(req["env"].clone())
            .map_err(|e| std::io::Error::other(format!("bad env: {e}")))?,
        _ => Vec::new(),
    };
    let started = supervisor.start(spec, env)?;
    let view = supervisor.get(started.id, None, 0)?;
    Ok(json!({ "run": view, "created": started.created }))
}

fn run_id_arg(req: &Value) -> std::io::Result<crate::model::run::RunId> {
    let raw = req
        .get("run_id")
        .ok_or_else(|| std::io::Error::other("run_id is required"))?;
    let id: i64 = match raw {
        Value::String(s) => s.parse::<i64>().ok(),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
    .ok_or_else(|| std::io::Error::other("run_id must be a decimal id"))?;
    Ok(crate::model::run::RunId(id))
}

fn get_run(supervisor: &Arc<Supervisor>, req: &Value) -> std::io::Result<Value> {
    let id = run_id_arg(req)?;
    let after = req.get("after_revision").and_then(Value::as_i64);
    let wait_ms = req.get("wait_ms").and_then(Value::as_u64).unwrap_or(0);
    Ok(json!({ "run": supervisor.get(id, after, wait_ms)? }))
}

fn read_run_output(supervisor: &Arc<Supervisor>, req: &Value) -> std::io::Result<Value> {
    let id = run_id_arg(req)?;
    let stream = req
        .get("stream")
        .and_then(Value::as_str)
        .and_then(Stream::parse)
        .ok_or_else(|| std::io::Error::other("stream must be stdout or stderr"))?;
    let cursor = req.get("cursor").and_then(Value::as_u64).unwrap_or(0);
    // Bounded chunk: a client cannot ask for unbounded memory.
    let max_bytes = req
        .get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(64 * 1024)
        .clamp(1, 1024 * 1024) as usize;
    match supervisor.output(id, stream, cursor, max_bytes)? {
        Some(chunk) => Ok(json!({
            "stream": stream.as_str(),
            "data": chunk.data,
            "next_cursor": chunk.next_cursor,
            "truncated": chunk.truncated,
            "eof": chunk.eof,
        })),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("run {id} not found"),
        )),
    }
}

fn list_runs(supervisor: &Arc<Supervisor>, req: &Value) -> std::io::Result<Value> {
    let filter = req.get("filter").cloned().unwrap_or(json!({}));
    let mut f = crate::store::RunFilter {
        project: filter
            .get("project")
            .and_then(Value::as_str)
            .map(String::from),
        state: filter
            .get("state")
            .and_then(Value::as_str)
            .and_then(crate::model::run::RunState::parse),
        limit: filter
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize),
        session: None,
    };
    if let Some(sid) = filter.get("session").and_then(Value::as_str) {
        f.session = Some(crate::model::session::RuntimeSessionId::from_u64(
            u64::from_str_radix(sid, 16).unwrap_or(0),
        ));
    }
    Ok(json!({ "runs": supervisor.list(&f)? }))
}

fn cancel_run(supervisor: &Arc<Supervisor>, req: &Value) -> std::io::Result<Value> {
    let id = run_id_arg(req)?;
    Ok(json!({ "run": supervisor.cancel(id)? }))
}

/// Vendor registers an agent session (contract §17). Resolves the pid to a
/// Wyd session and records the vendor id as an alias. Errors (not `ok` JSON)
/// so the client gets `{"ok":false,"error":...}`.
fn session_start(store: &mut RuntimeStore, req: &Value) -> std::io::Result<Value> {
    let agent = req.get("agent").and_then(Value::as_str).unwrap_or("");
    let vendor = req.get("vendor").and_then(Value::as_str).unwrap_or("");
    let vendor_sid = req
        .get("vendor_session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let pid = req.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;

    if pid == 0 || agent.is_empty() || vendor.is_empty() || vendor_sid.is_empty() {
        return Err(std::io::Error::other(
            "session_start needs pid > 0, agent, vendor, vendor_session_id",
        ));
    }

    let boot = store.boot_id_for_epoch(SystemBoot.current_boot_epoch()?, now())?;
    let mut scanner = SysinfoProcessScanner::new();
    let processes = scanner
        .scan()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let Some(proc) = processes.iter().find(|p| p.pid == pid) else {
        return Err(std::io::Error::other("pid not running"));
    };
    let Some(id) = ProcessIdentity::from_process(&boot, proc) else {
        return Err(std::io::Error::other("pid has no stable identity"));
    };
    let now = now();
    let sid = store.ensure_session(&boot, agent, pid, id.start_time, now)?;
    store.register_alias(sid, vendor, vendor_sid, now)?;
    Ok(json!({ "session_id": sid.to_string(), "agent": agent }))
}

/// Vendor ends a session it previously registered. This is metadata on the
/// alias — it does not end the Wyd runtime session (process may still run).
fn session_end(store: &mut RuntimeStore, req: &Value) -> std::io::Result<Value> {
    let vendor = req.get("vendor").and_then(Value::as_str).unwrap_or("");
    let vendor_sid = req
        .get("vendor_session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    match store.session_id_for_alias(vendor, vendor_sid)? {
        Some(sid) => {
            store.end_vendor_alias(vendor, vendor_sid, now())?;
            Ok(json!({ "ended": true, "session_id": sid.to_string() }))
        }
        None => Ok(json!({ "ended": false, "note": "unknown alias" })),
    }
}

fn explain(store: &mut RuntimeStore, pid: u32) -> std::io::Result<Value> {
    let boot = store.boot_id_for_epoch(SystemBoot.current_boot_epoch()?, now())?;
    let mut scanner = SysinfoProcessScanner::new();
    let processes = scanner
        .scan()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let Some(proc) = processes.iter().find(|p| p.pid == pid) else {
        return Ok(json!({ "pid": pid, "origin_session": null, "note": "not running" }));
    };
    let Some(identity) = ProcessIdentity::from_process(&boot, proc) else {
        return Ok(json!({ "pid": pid, "origin_session": null, "note": "no stable identity" }));
    };
    match store.explain_process(&boot, pid, identity.start_time)? {
        Some(exp) => Ok(json!({
            "pid": pid,
            "name": proc.label(),
            "origin_session": session_json(&exp.session),
            "owner_session_id": exp.owner.to_string(),
        })),
        None => Ok(json!({ "pid": pid, "name": proc.label(), "origin_session": null })),
    }
}

/// Standalone explain (opens its own store) — shared by the MCP tool.
pub fn explain_pid(pid: u32) -> std::io::Result<Value> {
    let mut store = RuntimeStore::open(&RuntimeStore::default_path())?;
    explain(&mut store, pid)
}

fn session_json(s: &crate::store::SessionRecord) -> Value {
    json!({
        "id": s.id.to_string(),
        "agent": s.agent,
        "project": s.project,
        "state": if s.ended_at.is_some() { "ended" } else { "active" },
        "started_at": s.started_at,
        "last_seen_at": s.last_seen_at,
        "ended_at": s.ended_at,
    })
}

fn ok_json(data: Value) -> String {
    json!({ "ok": true, "data": data }).to_string()
}

fn err_json(msg: &str) -> String {
    json!({ "ok": false, "error": msg }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::boot::BootId;

    fn test_supervisor() -> Arc<Supervisor> {
        let root = std::env::temp_dir().join(format!("wyd-server-test-{}", std::process::id()));
        crate::runner::for_tests(&root).unwrap()
    }

    fn proc(
        pid: u32,
        ppid: Option<u32>,
        name: &str,
        cmd: &[&str],
        start: u64,
    ) -> crate::model::ProcessInfo {
        crate::model::ProcessInfo {
            pid,
            parent_pid: ppid,
            name: name.into(),
            command: cmd.iter().map(|s| (*s).to_string()).collect(),
            executable: None,
            cwd: None,
            cpu_percent: 0.0,
            memory_bytes: 40 << 20,
            start_time: start,
            tty: None,
        }
    }

    #[test]
    fn dispatch_lists_sessions() {
        let mut store = RuntimeStore::open_in_memory().unwrap();
        let sup = test_supervisor();
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(100, Some(1), "omp", &["omp"], 1000),
        ];
        let boot = BootId::from_u128(7);
        let items = crate::classify::group(&procs);
        let ids: std::collections::HashMap<u32, ProcessIdentity> = procs
            .iter()
            .filter_map(|p| ProcessIdentity::from_process(&boot, p).map(|id| (p.pid, id)))
            .collect();
        let out = crate::classify::ownership::derive_ownership(&items, &ids, 2000);
        store.apply_ownership(&out, 2000).unwrap();

        let resp = dispatch(&mut store, &sup, "{\"cmd\":\"list_sessions\"}");
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["sessions"][0]["agent"], "omp");
    }

    #[test]
    fn dispatch_rejects_unknown_command() {
        let mut store = RuntimeStore::open_in_memory().unwrap();
        let sup = test_supervisor();
        let resp = dispatch(&mut store, &sup, "{\"cmd\":\"rm -rf /\"}");
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], false);
    }
}
