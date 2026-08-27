//! Deterministic synthetic dataset for `wyd web --demo`. **No host I/O**, no
//! scanners, no Docker. Reuses `RuntimeSnapshot` so the same JSON shapes
//! apply, and the same `proposal` rules select correctly.
//!
//! Story: five coding-agent sessions on one dev machine. Three ended and
//! left leftovers behind; two are still active. Each agent runs MCP servers
//! and dev tooling; persistent services (postgres, redis, mysql) are
//! excluded from cleanup.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::model::{
    Category, ListeningPort, ProcessInfo, Project, Protocol, RuntimeItem, RuntimeSnapshot,
    RuntimeState, Suspicion, SuspicionReason,
    docker::DockerSnapshot,
    session::{RuntimeSessionId, SessionInfo},
};
use crate::store::SessionRecord;

/// Stable ids derived from the seed string so demo is reproducible across
/// runs and across judges' machines. FNV-1a 64.
fn stable_id(seed: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in seed.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn proc(pid: u32, name: &str, cmd: &[&str], memory: u64) -> ProcessInfo {
    ProcessInfo {
        pid,
        parent_pid: None,
        name: name.into(),
        command: cmd.iter().map(|s| (*s).to_string()).collect(),
        executable: None,
        cwd: None,
        cpu_percent: 0.0,
        memory_bytes: memory,
        start_time: 1,
        tty: None,
    }
}

fn port(p: u16) -> ListeningPort {
    ListeningPort {
        protocol: Protocol::Tcp,
        address: "127.0.0.1".parse().unwrap(),
        port: p,
        pid: 0,
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// (seed, agent, project, ago, active)
const SESSIONS: &[(&str, &str, &str, u64, bool)] = &[
    (
        "demo.opencode.wyd",
        "opencode",
        "~/Work/wyd",
        47 * 60,
        false,
    ),
    ("demo.claude.docs", "claude", "~/Work/docs", 2 * 3600, false),
    ("demo.cursor.site", "cursor", "~/Work/site", 3 * 3600, false),
    ("demo.codex.api", "codex", "~/Work/api", 11 * 60, true),
    (
        "demo.gemini.notes",
        "gemini-cli",
        "~/Work/notes",
        25 * 60,
        true,
    ),
];

pub fn session_infos() -> Vec<SessionInfo> {
    let n = now();
    SESSIONS
        .iter()
        .map(|(seed, agent, project, ago, active)| SessionInfo {
            id: RuntimeSessionId::from_u64(stable_id(seed)),
            agent: (*agent).into(),
            project: Some((*project).into()),
            started_at: n.saturating_sub(*ago),
            active: *active,
        })
        .collect()
}

pub fn session_record(id: u64) -> Option<SessionRecord> {
    let n = now();
    SESSIONS
        .iter()
        .find(|(seed, ..)| stable_id(seed) == id)
        .map(|(seed, agent, project, ago, active)| {
            let started = n.saturating_sub(*ago);
            SessionRecord {
                id: RuntimeSessionId::from_u64(stable_id(seed)),
                agent: (*agent).into(),
                project: Some((*project).into()),
                started_at: started,
                last_seen_at: if *active {
                    started + 60
                } else {
                    started + 1200
                },
                ended_at: if *active { None } else { Some(started + 1200) },
            }
        })
}

fn project(name: &str, root: &str) -> Option<Project> {
    Some(Project {
        name: name.into(),
        root: std::path::PathBuf::from(root),
    })
}

fn suspicious(score: u8, reasons: Vec<SuspicionReason>) -> Option<Suspicion> {
    Some(Suspicion { score, reasons })
}

pub fn snapshot() -> RuntimeSnapshot {
    let p_wyd = project("wyd", "/Users/me/Work/wyd");
    let p_docs = project("docs", "/Users/me/Work/docs");
    let p_site = project("site", "/Users/me/Work/site");
    let p_api = project("api", "/Users/me/Work/api");
    let p_notes = project("notes", "/Users/me/Work/notes");

    // ── opencode (ended): chrome-devtools-mcp + Chromium + vite ──
    let mut opencode_mcp = RuntimeItem {
        category: Category::Mcp,
        display_name: "chrome-devtools-mcp".into(),
        root_pid: Some(4101),
        process_ids: vec![4101],
        memory_bytes: 42 << 20,
        cpu_percent: 0.0,
        state: RuntimeState::Suspicious,
        suspicion: suspicious(60, vec![SuspicionReason::SessionOwnerEnded]),
        ports: vec![],
        project: p_wyd.clone(),
        children: vec![],
    };
    opencode_mcp.children.push(RuntimeItem {
        category: Category::Browser,
        display_name: "Chromium x8".into(),
        root_pid: Some(4102),
        process_ids: vec![4102],
        memory_bytes: (8u64 * 145) << 20,
        cpu_percent: 0.1,
        state: RuntimeState::Suspicious,
        suspicion: suspicious(
            80,
            vec![
                SuspicionReason::SessionOwnerEnded,
                SuspicionReason::HeadlessBrowserDetached,
            ],
        ),
        ports: vec![],
        project: p_wyd.clone(),
        children: vec![],
    });

    // ── claude (ended): playwright-mcp + Chromium + filesystem + next ──
    let mut playwright = RuntimeItem {
        category: Category::Mcp,
        display_name: "playwright-mcp".into(),
        root_pid: Some(4201),
        process_ids: vec![4201],
        memory_bytes: 58 << 20,
        cpu_percent: 0.0,
        state: RuntimeState::Suspicious,
        suspicion: suspicious(55, vec![SuspicionReason::SessionOwnerEnded]),
        ports: vec![],
        project: p_docs.clone(),
        children: vec![],
    };
    playwright.children.push(RuntimeItem {
        category: Category::Browser,
        display_name: "Chromium x3".into(),
        root_pid: Some(4202),
        process_ids: vec![4202],
        memory_bytes: (3u64 * 150) << 20,
        cpu_percent: 0.1,
        state: RuntimeState::Suspicious,
        suspicion: suspicious(
            75,
            vec![
                SuspicionReason::SessionOwnerEnded,
                SuspicionReason::HeadlessBrowserDetached,
            ],
        ),
        ports: vec![],
        project: p_docs.clone(),
        children: vec![],
    });
    let claude_mcp = RuntimeItem {
        category: Category::Mcp,
        display_name: "filesystem-mcp".into(),
        root_pid: Some(4203),
        process_ids: vec![4203],
        memory_bytes: 12 << 20,
        cpu_percent: 0.0,
        state: RuntimeState::Suspicious,
        suspicion: suspicious(45, vec![SuspicionReason::SessionOwnerEnded]),
        ports: vec![],
        project: p_docs.clone(),
        children: vec![],
    };
    let next_dev = RuntimeItem {
        category: Category::DevServer,
        display_name: "next :3000".into(),
        root_pid: Some(4204),
        process_ids: vec![4204],
        memory_bytes: 210 << 20,
        cpu_percent: 0.3,
        state: RuntimeState::Suspicious,
        suspicion: suspicious(
            65,
            vec![
                SuspicionReason::SessionOwnerEnded,
                SuspicionReason::LongRunningDevServer,
            ],
        ),
        ports: vec![port(3000)],
        project: p_docs.clone(),
        children: vec![],
    };

    // ── cursor (ended): github-mcp + vite ──
    let cursor_mcp = RuntimeItem {
        category: Category::Mcp,
        display_name: "github-mcp".into(),
        root_pid: Some(4301),
        process_ids: vec![4301],
        memory_bytes: 30 << 20,
        cpu_percent: 0.0,
        state: RuntimeState::Suspicious,
        suspicion: suspicious(50, vec![SuspicionReason::SessionOwnerEnded]),
        ports: vec![],
        project: p_site.clone(),
        children: vec![],
    };
    let cursor_vite = RuntimeItem {
        category: Category::DevServer,
        display_name: "vite :5173".into(),
        root_pid: Some(4302),
        process_ids: vec![4302],
        memory_bytes: 118 << 20,
        cpu_percent: 0.0,
        state: RuntimeState::Suspicious,
        suspicion: suspicious(
            60,
            vec![
                SuspicionReason::SessionOwnerEnded,
                SuspicionReason::LongRunningDevServer,
            ],
        ),
        ports: vec![port(5173)],
        project: p_site.clone(),
        children: vec![],
    };

    // ── codex (active): github-mcp + context7-mcp + rust-analyzer ──
    let codex_gh = RuntimeItem {
        category: Category::Mcp,
        display_name: "github-mcp".into(),
        root_pid: Some(5101),
        process_ids: vec![5101],
        memory_bytes: 22 << 20,
        cpu_percent: 0.1,
        state: RuntimeState::Active,
        suspicion: None,
        ports: vec![],
        project: p_api.clone(),
        children: vec![],
    };
    let codex_c7 = RuntimeItem {
        category: Category::Mcp,
        display_name: "context7-mcp".into(),
        root_pid: Some(5102),
        process_ids: vec![5102],
        memory_bytes: 18 << 20,
        cpu_percent: 0.1,
        state: RuntimeState::Active,
        suspicion: None,
        ports: vec![],
        project: p_api.clone(),
        children: vec![],
    };
    let rust_analyzer = RuntimeItem {
        category: Category::LanguageServer,
        display_name: "rust-analyzer".into(),
        root_pid: Some(5103),
        process_ids: vec![5103],
        memory_bytes: 220 << 20,
        cpu_percent: 1.2,
        state: RuntimeState::Active,
        suspicion: None,
        ports: vec![],
        project: p_api.clone(),
        children: vec![],
    };
    let cargo_watch = RuntimeItem {
        category: Category::Worker,
        display_name: "cargo-watch".into(),
        root_pid: Some(5104),
        process_ids: vec![5104],
        memory_bytes: 35 << 20,
        cpu_percent: 0.0,
        state: RuntimeState::Active,
        suspicion: None,
        ports: vec![],
        project: p_api.clone(),
        children: vec![],
    };

    // ── gemini-cli (active): sequential-thinking + fetch ──
    let gemini_st = RuntimeItem {
        category: Category::Mcp,
        display_name: "sequential-thinking".into(),
        root_pid: Some(5201),
        process_ids: vec![5201],
        memory_bytes: 24 << 20,
        cpu_percent: 0.0,
        state: RuntimeState::Active,
        suspicion: None,
        ports: vec![],
        project: p_notes.clone(),
        children: vec![],
    };
    let gemini_fetch = RuntimeItem {
        category: Category::Mcp,
        display_name: "fetch-mcp".into(),
        root_pid: Some(5202),
        process_ids: vec![5202],
        memory_bytes: 20 << 20,
        cpu_percent: 0.0,
        state: RuntimeState::Active,
        suspicion: None,
        ports: vec![],
        project: p_notes.clone(),
        children: vec![],
    };

    // ── persistent services (never proposed for cleanup) ──
    let postgres = RuntimeItem {
        category: Category::Database,
        display_name: "postgres".into(),
        root_pid: Some(9100),
        process_ids: vec![9100],
        memory_bytes: 320 << 20,
        cpu_percent: 0.1,
        state: RuntimeState::Persistent,
        suspicion: None,
        ports: vec![port(5432)],
        project: None,
        children: vec![],
    };
    let redis = RuntimeItem {
        category: Category::Database,
        display_name: "redis".into(),
        root_pid: Some(9101),
        process_ids: vec![9101],
        memory_bytes: 18 << 20,
        cpu_percent: 0.0,
        state: RuntimeState::Persistent,
        suspicion: None,
        ports: vec![port(6379)],
        project: None,
        children: vec![],
    };
    let mysql = RuntimeItem {
        category: Category::Database,
        display_name: "mysql".into(),
        root_pid: Some(9102),
        process_ids: vec![9102],
        memory_bytes: 260 << 20,
        cpu_percent: 0.1,
        state: RuntimeState::Persistent,
        suspicion: None,
        ports: vec![port(3306)],
        project: None,
        children: vec![],
    };

    let procs = vec![
        proc(4100, "opencode", &["opencode", "~/Work/wyd"], 140 << 20),
        proc(
            4101,
            "chrome-devtools-mcp",
            &["chrome-devtools-mcp"],
            42 << 20,
        ),
        proc(4102, "Chromium", &["Chromium", "--headless"], 145 << 20),
        proc(4103, "vite", &["vite", "--port", "5173"], 118 << 20),
        proc(4200, "claude", &["claude", "~/Work/docs"], 160 << 20),
        proc(4201, "playwright-mcp", &["playwright-mcp"], 58 << 20),
        proc(4202, "Chromium", &["Chromium", "--headless"], 150 << 20),
        proc(4203, "filesystem-mcp", &["filesystem-mcp"], 12 << 20),
        proc(4204, "next", &["next", "dev"], 210 << 20),
        proc(4300, "cursor", &["cursor", "~/Work/site"], 150 << 20),
        proc(4301, "github-mcp", &["github-mcp"], 30 << 20),
        proc(4302, "vite", &["vite", "--port", "5173"], 118 << 20),
        proc(5100, "codex", &["codex", "~/Work/api"], 90 << 20),
        proc(5101, "github-mcp", &["github-mcp"], 22 << 20),
        proc(5102, "context7-mcp", &["context7-mcp"], 18 << 20),
        proc(5103, "rust-analyzer", &["rust-analyzer"], 220 << 20),
        proc(5104, "cargo-watch", &["cargo-watch"], 35 << 20),
        proc(
            5200,
            "gemini-cli",
            &["gemini-cli", "~/Work/notes"],
            110 << 20,
        ),
        proc(
            5201,
            "sequential-thinking",
            &["sequential-thinking"],
            24 << 20,
        ),
        proc(5202, "fetch-mcp", &["fetch-mcp"], 20 << 20),
        proc(9100, "postgres", &["postgres"], 320 << 20),
        proc(9101, "redis-server", &["redis-server"], 18 << 20),
        proc(9102, "mysqld", &["mysqld"], 260 << 20),
    ];

    let items = vec![
        RuntimeItem {
            category: Category::Agent,
            display_name: "opencode".into(),
            root_pid: Some(4100),
            process_ids: vec![4100],
            memory_bytes: 140 << 20,
            cpu_percent: 0.5,
            state: RuntimeState::Suspicious,
            suspicion: suspicious(70, vec![SuspicionReason::SessionOwnerEnded]),
            ports: vec![],
            project: p_wyd.clone(),
            children: vec![opencode_mcp],
        },
        RuntimeItem {
            category: Category::Agent,
            display_name: "claude".into(),
            root_pid: Some(4200),
            process_ids: vec![4200],
            memory_bytes: 160 << 20,
            cpu_percent: 0.3,
            state: RuntimeState::Suspicious,
            suspicion: suspicious(70, vec![SuspicionReason::SessionOwnerEnded]),
            ports: vec![],
            project: p_docs.clone(),
            children: vec![playwright, claude_mcp, next_dev],
        },
        RuntimeItem {
            category: Category::Agent,
            display_name: "cursor".into(),
            root_pid: Some(4300),
            process_ids: vec![4300],
            memory_bytes: 150 << 20,
            cpu_percent: 0.2,
            state: RuntimeState::Suspicious,
            suspicion: suspicious(70, vec![SuspicionReason::SessionOwnerEnded]),
            ports: vec![],
            project: p_site.clone(),
            children: vec![cursor_mcp, cursor_vite],
        },
        RuntimeItem {
            category: Category::Agent,
            display_name: "codex".into(),
            root_pid: Some(5100),
            process_ids: vec![5100],
            memory_bytes: 90 << 20,
            cpu_percent: 0.8,
            state: RuntimeState::Active,
            suspicion: None,
            ports: vec![],
            project: p_api.clone(),
            children: vec![codex_gh, codex_c7, rust_analyzer, cargo_watch],
        },
        RuntimeItem {
            category: Category::Agent,
            display_name: "gemini-cli".into(),
            root_pid: Some(5200),
            process_ids: vec![5200],
            memory_bytes: 110 << 20,
            cpu_percent: 0.4,
            state: RuntimeState::Active,
            suspicion: None,
            ports: vec![],
            project: p_notes.clone(),
            children: vec![gemini_st, gemini_fetch],
        },
        postgres,
        redis,
        mysql,
    ];

    RuntimeSnapshot {
        processes: procs,
        logical_items: items,
        docker: Arc::new(DockerSnapshot::default()),
        total_memory_bytes: 16u64 << 30,
        used_memory_bytes: 6u64 << 30,
        cpu_percent: 6.8,
        sessions: session_infos(),
        version: 1,
    }
}

pub fn explain(pid: u32) -> Option<Value> {
    let snap = snapshot();
    fn find_item(items: &[RuntimeItem], pid: u32) -> Option<&RuntimeItem> {
        for i in items {
            if i.root_pid == Some(pid) {
                return Some(i);
            }
            if let Some(found) = find_item(&i.children, pid) {
                return Some(found);
            }
        }
        None
    }
    let item = find_item(&snap.logical_items, pid)?;
    // Agent roots → their own session; otherwise fall back to the project's agent.
    let session_seed = match item.display_name.as_str() {
        "opencode" => "demo.opencode.wyd",
        "claude" => "demo.claude.docs",
        "cursor" => "demo.cursor.site",
        "codex" => "demo.codex.api",
        "gemini-cli" => "demo.gemini.notes",
        "postgres" | "redis" | "mysql" => return None,
        _ => {
            // child resource: find its owning agent by project
            let proj = item
                .project
                .as_ref()
                .map(|p| p.root.to_string_lossy().to_string());
            match proj.as_deref() {
                Some(p) if p.contains("wyd") => "demo.opencode.wyd",
                Some(p) if p.contains("docs") => "demo.claude.docs",
                Some(p) if p.contains("site") => "demo.cursor.site",
                Some(p) if p.contains("api") => "demo.codex.api",
                Some(p) if p.contains("notes") => "demo.gemini.notes",
                _ => "demo.opencode.wyd",
            }
        }
    };
    let session_id = stable_id(session_seed);
    let cwd_value = item
        .project
        .as_ref()
        .map(|p| p.root.display().to_string())
        .unwrap_or_default();
    let owner_hex = format!("{:016x}", session_id);
    let owned_value = "session ".to_string() + &owner_hex + ":0";
    let session_json = session_record(session_id).map(|r| {
        json!({
            "id": owner_hex,
            "agent": r.agent,
            "project": r.project,
            "started_at": r.started_at,
            "ended_at": r.ended_at,
            "active": r.ended_at.is_none(),
        })
    });
    Some(json!({
        "pid": pid,
        "name": item.display_name,
        "owner_session": owner_hex,
        "exact": true,
        "verdict": "owned",
        "resolver_version": 1,
        "evidence": [
            { "kind": "cwd match", "value": cwd_value },
            { "kind": "persisted ownership", "value": owned_value }
        ],
        "session": session_json,
    }))
}
