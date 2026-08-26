//! `wyd serve`: a local, read-only Unix-socket API over the ownership store
//! (contract §15–16). Keeps the store fresh by running the collector on a
//! loop, and serves line-delimited JSON requests to local clients.
//!
//! Phase 3 slice 1: daemon + read-only IPC. No MCP, no vendor protocol yet.

use crate::collect::{self, OwnershipTracker};
use crate::model::process::ProcessIdentity;
use crate::platform::{BootIdentityProvider, SystemBoot};
use crate::scanner::{ProcessScanner, processes::SysinfoProcessScanner};
use crate::store::RuntimeStore;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

const REFRESH: Duration = Duration::from_secs(2);

/// The Unix socket lives next to the state database.
fn socket_path() -> PathBuf {
    RuntimeStore::default_path().with_file_name("wyd.sock")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run the daemon: a background collector loop plus a Unix-socket acceptor.
pub fn serve() -> std::io::Result<()> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path); // clear a stale socket from a dead run
    let listener = UnixListener::bind(&path)?;
    eprintln!("wyd serve on {} (read-only)", path.display());

    thread::spawn(collect_loop);

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let _ = handle(s);
            }
            Err(e) => eprintln!("accept: {e}"),
        }
    }
    Ok(())
}

/// Collect + persist on a loop so the API stays fresh even with no TUI open.
fn collect_loop() {
    let mut tracker = OwnershipTracker::new();
    loop {
        let mut snap = collect::snapshot();
        tracker.record(&snap.processes, &snap.logical_items);
        tracker.layer_session_leftovers(&mut snap.logical_items, &snap.processes);
        thread::sleep(REFRESH);
    }
}

fn handle(stream: UnixStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(()); // client closed
    }
    let mut store = match RuntimeStore::open(&RuntimeStore::default_path()) {
        Ok(s) => s,
        Err(e) => {
            let mut w = stream;
            writeln!(w, "{}", err_json(&e.to_string()))?;
            return Ok(());
        }
    };
    let resp = dispatch(&mut store, &line);
    let mut w = stream;
    writeln!(w, "{resp}")?;
    Ok(())
}

/// Parse one request line and answer from the store. `&mut` because `explain`
/// resolves (and may persist) the boot id.
fn dispatch(store: &mut RuntimeStore, line: &str) -> String {
    let req: Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(e) => return err_json(&e.to_string()),
    };
    let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("");
    match cmd {
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

/// Vendor registers an agent session (contract §17). Resolves the pid to a
/// Wyd session and records the vendor id as an alias.
fn session_start(store: &mut RuntimeStore, req: &Value) -> std::io::Result<Value> {
    let agent = req.get("agent").and_then(Value::as_str).unwrap_or("");
    let vendor = req
        .get("vendor")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let vendor_sid = req
        .get("vendor_session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let pid = req.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;

    let boot = store.boot_id_for_epoch(SystemBoot.current_boot_epoch()?, now())?;
    let mut scanner = SysinfoProcessScanner::new();
    let processes = scanner
        .scan()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let Some(proc) = processes.iter().find(|p| p.pid == pid) else {
        return Ok(json!({ "error": "pid not running" }));
    };
    let Some(id) = ProcessIdentity::from_process(&boot, proc) else {
        return Ok(json!({ "error": "no stable identity" }));
    };
    let sid = store.ensure_session(&boot, agent, pid, id.start_time, now())?;
    if !vendor_sid.is_empty() {
        store.register_alias(sid, vendor, vendor_sid)?;
    }
    Ok(json!({ "session_id": sid.to_string(), "agent": agent }))
}

/// Vendor ends a session it previously registered.
fn session_end(store: &mut RuntimeStore, req: &Value) -> std::io::Result<Value> {
    let vendor = req.get("vendor").and_then(Value::as_str).unwrap_or("");
    let vendor_sid = req
        .get("vendor_session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    match store.session_id_for_alias(vendor, vendor_sid)? {
        Some(sid) => {
            store.end_session(sid, now())?;
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

        let resp = dispatch(&mut store, "{\"cmd\":\"list_sessions\"}");
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["sessions"][0]["agent"], "omp");
    }

    #[test]
    fn dispatch_rejects_unknown_command() {
        let mut store = RuntimeStore::open_in_memory().unwrap();
        let resp = dispatch(&mut store, "{\"cmd\":\"rm -rf /\"}");
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], false);
    }
}
