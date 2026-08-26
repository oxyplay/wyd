//! Shared realistic process fixtures for classify / ownership / resolver
//! tests: one coding-agent session (omp) with its MCP server, three headless
//! Chromium helpers, a Vite dev server and an ad-hoc PostgreSQL — as a live
//! tree, and after the agent exits (everything reparented to init).
//!
//! Built once here so every test exercises the same real-world shapes
//! instead of bespoke one-off process lists.

use std::collections::HashMap;

use crate::model::ProcessInfo;
use crate::model::boot::BootId;
use crate::model::process::ProcessIdentity;

/// Stable per-boot UUID for the fixture machine (Linux-style, so it is used
/// directly with no persisted mapping).
pub const BOOT_UUID: u128 = 0x6fbe_5d5c_a1a2_4b3c_9d4e_5f6a_7b8c_9d0e;

pub const LAUNCHD_PID: u32 = 1;
pub const AGENT_PID: u32 = 100;
pub const AGENT_START: u64 = 1000;
pub const MCP_PID: u32 = 110;
pub const MCP_START: u64 = 1004;
pub const CHROME_PIDS: [u32; 3] = [120, 121, 122];
pub const CHROME_START: u64 = 1007;
pub const VITE_PID: u32 = 130;
pub const VITE_START: u64 = 1010;
pub const PG_PID: u32 = 140;
pub const PG_START: u64 = 1012;

pub fn boot() -> BootId {
    BootId::from_u128(BOOT_UUID)
}

pub fn proc(
    pid: u32,
    ppid: Option<u32>,
    name: &str,
    cmd: &[&str],
    start: u64,
    tty: Option<&str>,
) -> ProcessInfo {
    ProcessInfo {
        pid,
        parent_pid: ppid,
        name: name.into(),
        command: cmd.iter().map(|s| (*s).to_string()).collect(),
        executable: None,
        cwd: None,
        cpu_percent: 0.0,
        memory_bytes: 40 << 20,
        start_time: start,
        tty: tty.map(str::to_string),
    }
}

/// The session while the agent is alive: omp → chrome-devtools-mcp → three
/// Chromium helpers, plus Vite and an ad-hoc PostgreSQL under the agent.
pub fn live_agent_session() -> Vec<ProcessInfo> {
    let mut v = vec![proc(LAUNCHD_PID, None, "launchd", &["launchd"], 1, None)];
    v.push(proc(
        AGENT_PID,
        Some(LAUNCHD_PID),
        "omp",
        &["omp"],
        AGENT_START,
        None,
    ));
    v.push(proc(
        MCP_PID,
        Some(AGENT_PID),
        "node",
        &["node", "chrome-devtools-mcp"],
        MCP_START,
        None,
    ));
    for pid in CHROME_PIDS {
        v.push(proc(
            pid,
            Some(MCP_PID),
            "Chromium",
            &["Chromium", "--headless"],
            CHROME_START,
            None,
        ));
    }
    v.push(proc(
        VITE_PID,
        Some(AGENT_PID),
        "node",
        &["node", "node_modules/.bin/vite"],
        VITE_START,
        None,
    ));
    v.push(proc(
        PG_PID,
        Some(AGENT_PID),
        "postgres",
        &["postgres", "-D", "/tmp/wyd-test-db"],
        PG_START,
        None,
    ));
    v
}

/// The same runtime after the agent exited: the MCP, Vite and the DB were
/// reparented to init (pid 1); Chromium still hangs off its MCP.
pub fn orphaned_agent_session() -> Vec<ProcessInfo> {
    let mut v = vec![proc(LAUNCHD_PID, None, "launchd", &["launchd"], 1, None)];
    v.push(proc(
        MCP_PID,
        Some(LAUNCHD_PID),
        "node",
        &["node", "chrome-devtools-mcp"],
        MCP_START,
        None,
    ));
    for pid in CHROME_PIDS {
        v.push(proc(
            pid,
            Some(MCP_PID),
            "Chromium",
            &["Chromium", "--headless"],
            CHROME_START,
            None,
        ));
    }
    v.push(proc(
        VITE_PID,
        Some(LAUNCHD_PID),
        "node",
        &["node", "node_modules/.bin/vite"],
        VITE_START,
        None,
    ));
    v.push(proc(
        PG_PID,
        Some(LAUNCHD_PID),
        "postgres",
        &["postgres", "-D", "/tmp/wyd-test-db"],
        PG_START,
        None,
    ));
    v
}

/// Valid identities (start_time != 0) for every pid in the snapshot.
pub fn identities(procs: &[ProcessInfo]) -> HashMap<u32, ProcessIdentity> {
    let b = boot();
    procs
        .iter()
        .filter_map(|p| ProcessIdentity::from_process(&b, p).map(|id| (p.pid, id)))
        .collect()
}
