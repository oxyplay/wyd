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
    docker::{DockerKind, DockerResource, DockerSnapshot},
    run::{
        Capacity, CleanupState, EffectiveLimits, Enforcement, LogState, ProjectUsage, QueueEntry,
        QueueInfo, ResourceRequest, RunOutcome, RunState, backend_capabilities,
    },
    session::{RuntimeSessionId, SessionInfo},
};
use crate::runner::RunView;
use crate::runner::logs::Stream;
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

fn proc(pid: u32, name: &str, cmd: &[&str], memory: u64, ago: u64) -> ProcessInfo {
    ProcessInfo {
        pid,
        parent_pid: None,
        name: name.into(),
        command: cmd.iter().map(|s| (*s).to_string()).collect(),
        executable: None,
        cwd: None,
        cpu_percent: 0.0,
        memory_bytes: memory,
        start_time: now().saturating_sub(ago),
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

/// Deterministic docker snapshot: a running container, a stopped one, an
/// anonymous volume, and a persistent named volume — enough to exercise the
/// web/TUI Docker sections without a live daemon.
fn demo_docker() -> DockerSnapshot {
    let n = now();
    let c = |id: &str, name: &str, detail: &str, size: u64, compose: Option<&str>, created: u64| {
        DockerResource {
            kind: DockerKind::Container,
            id: id.into(),
            name: name.into(),
            detail: detail.into(),
            size_bytes: size,
            compose: compose.map(str::to_string),
            persistent: false,
            anonymous: false,
            created: created as i64,
        }
    };
    DockerSnapshot {
        ok: true,
        note: String::new(),
        disk_bytes: 9 << 30,
        reclaimable_bytes: 103 << 20,
        resources: vec![
            c(
                "a1b2c3",
                "wyd-test-web",
                "running",
                42 << 20,
                Some("testapp"),
                n - 3600,
            ),
            c("d4e5f6", "old_worker", "exited", 12 << 20, None, n - 86400),
            DockerResource {
                kind: DockerKind::Volume,
                id: "v-abc".into(),
                name: "anonymous-1".into(),
                detail: "unused".into(),
                size_bytes: 103 << 20,
                compose: None,
                persistent: false,
                anonymous: true,
                created: (n - 7200) as i64,
            },
            DockerResource {
                kind: DockerKind::Volume,
                id: "v-pg".into(),
                name: "pgdata".into(),
                detail: "unused".into(),
                size_bytes: 2 << 30,
                compose: Some("pg".into()),
                persistent: true,
                anonymous: false,
                created: (n - 5 * 86400) as i64,
            },
        ],
    }
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
    let vite_wyd = RuntimeItem {
        category: Category::DevServer,
        display_name: "vite :5173".into(),
        root_pid: Some(4103),
        process_ids: vec![4103],
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
        project: p_wyd.clone(),
        children: vec![],
    };

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
        proc(
            4100,
            "opencode",
            &["opencode", "~/Work/wyd"],
            140 << 20,
            47 * 60,
        ),
        proc(
            4101,
            "chrome-devtools-mcp",
            &["chrome-devtools-mcp"],
            42 << 20,
            45 * 60,
        ),
        proc(
            4102,
            "Chromium",
            &["Chromium", "--headless"],
            145 << 20,
            30 * 60,
        ),
        proc(
            4103,
            "vite",
            &["vite", "--port", "5173"],
            118 << 20,
            25 * 60,
        ),
        proc(
            4200,
            "claude",
            &["claude", "~/Work/docs"],
            160 << 20,
            2 * 3600,
        ),
        proc(
            4201,
            "playwright-mcp",
            &["playwright-mcp"],
            58 << 20,
            110 * 60,
        ),
        proc(
            4202,
            "Chromium",
            &["Chromium", "--headless"],
            150 << 20,
            90 * 60,
        ),
        proc(
            4203,
            "filesystem-mcp",
            &["filesystem-mcp"],
            12 << 20,
            115 * 60,
        ),
        proc(4204, "next", &["next", "dev"], 210 << 20, 105 * 60),
        proc(
            4300,
            "cursor",
            &["cursor", "~/Work/site"],
            150 << 20,
            3 * 3600,
        ),
        proc(4301, "github-mcp", &["github-mcp"], 30 << 20, 170 * 60),
        proc(
            4302,
            "vite",
            &["vite", "--port", "5173"],
            118 << 20,
            160 * 60,
        ),
        proc(5100, "codex", &["codex", "~/Work/api"], 90 << 20, 11 * 60),
        proc(5101, "github-mcp", &["github-mcp"], 22 << 20, 10 * 60),
        proc(5102, "context7-mcp", &["context7-mcp"], 18 << 20, 9 * 60),
        proc(
            5103,
            "rust-analyzer",
            &["rust-analyzer"],
            220 << 20,
            11 * 60,
        ),
        proc(5104, "cargo-watch", &["cargo-watch"], 35 << 20, 8 * 60),
        proc(
            5200,
            "gemini-cli",
            &["gemini-cli", "~/Work/notes"],
            110 << 20,
            25 * 60,
        ),
        proc(
            5201,
            "sequential-thinking",
            &["sequential-thinking"],
            24 << 20,
            24 * 60,
        ),
        proc(5202, "fetch-mcp", &["fetch-mcp"], 20 << 20, 20 * 60),
        proc(9100, "postgres", &["postgres"], 320 << 20, 10 * 86400),
        proc(
            9101,
            "redis-server",
            &["redis-server"],
            18 << 20,
            20 * 86400,
        ),
        proc(9102, "mysqld", &["mysqld"], 260 << 20, 30 * 86400),
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
            children: vec![opencode_mcp, vite_wyd],
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
        docker: Arc::new(demo_docker()),
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
        "ownership": "owned",
        "resolver_version": 1,
        "evidence": [
            { "kind": "cwd match", "value": cwd_value },
            { "kind": "persisted ownership", "value": owned_value }
        ],
        "session": session_json,
    }))
}

/// One synthetic managed run. Times are stored as offsets from "now" so the
/// dashboard always shows plausible, freshly-aged durations.
struct DemoRun {
    id: i64,
    request_id: &'static str,
    argv: &'static [&'static str],
    cwd: &'static str,
    project_root: &'static str,
    session_seed: Option<&'static str>,
    state: RunState,
    outcome: Option<RunOutcome>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    cleanup: CleanupState,
    ago_created: u64,
    ago_started: Option<u64>,
    ago_finished: Option<u64>,
    duration_ms: Option<u64>,
    detail: Option<&'static str>,
    stdout: &'static str,
    stderr: &'static str,
    /// Reserved against the memory budget at admission (None = supervisor
    /// default). A reservation is a budget, never measured RAM.
    memory_bytes: Option<u64>,
    enforcement: Enforcement,
    /// Present only while the request is waiting for a slot.
    queue_position: Option<usize>,
    queue_reason: Option<&'static str>,
    /// Measurement of the run's process group, kept distinct from the
    /// reservation above.
    observed_memory_bytes: Option<u64>,
    observed_processes: Option<usize>,
    limit_event: Option<&'static str>,
}

/// Eight runs: two live, one queued, and one per terminal outcome including a
/// resource-limit stop. Nothing here starts, signals or inspects a process.
const RUNS: &[DemoRun] = &[
    DemoRun {
        id: 412,
        request_id: "run-7f3a-web-tests",
        argv: &["pnpm", "test", "--filter", "web"],
        cwd: "/Users/me/Work/wyd",
        project_root: "/Users/me/Work/wyd",
        session_seed: Some("demo.opencode.wyd"),
        state: RunState::Running,
        outcome: None,
        exit_code: None,
        signal: None,
        cleanup: CleanupState::Pending,
        ago_created: 12,
        ago_started: Some(11),
        ago_finished: None,
        duration_ms: None,
        detail: None,
        stdout: "web: running 3 test files...\nweb: PASS src/app.test.ts\n",
        stderr: "",
        memory_bytes: None,
        enforcement: Enforcement::Monitored,
        queue_position: None,
        queue_reason: None,
        observed_memory_bytes: Some(188_743_680),
        observed_processes: Some(4),
        limit_event: None,
    },
    DemoRun {
        id: 407,
        request_id: "run-b5d2-nextest",
        argv: &["cargo", "nextest", "run", "--workspace"],
        cwd: "/Users/me/Work/wyd",
        project_root: "/Users/me/Work/wyd",
        session_seed: Some("demo.codex.wyd"),
        state: RunState::Queued,
        outcome: None,
        exit_code: None,
        signal: None,
        cleanup: CleanupState::Pending,
        ago_created: 45,
        ago_started: None,
        ago_finished: None,
        duration_ms: None,
        detail: Some("waiting for a slot"),
        stdout: "",
        stderr: "",
        memory_bytes: Some(512 * 1024 * 1024),
        enforcement: Enforcement::None,
        queue_position: Some(1),
        queue_reason: Some("project /Users/me/Work/wyd is at its 2-run limit"),
        observed_memory_bytes: None,
        observed_processes: None,
        limit_event: None,
    },
    DemoRun {
        id: 405,
        request_id: "run-3e8d-dev-server",
        argv: &["pnpm", "run", "dev"],
        cwd: "/Users/me/Work/wyd",
        project_root: "/Users/me/Work/wyd",
        session_seed: Some("demo.opencode.wyd"),
        state: RunState::Running,
        outcome: None,
        exit_code: None,
        signal: None,
        cleanup: CleanupState::Pending,
        ago_created: 300,
        ago_started: Some(299),
        ago_finished: None,
        duration_ms: None,
        detail: None,
        stdout: "vite v6.0.0  ready in 412 ms\n  ➜  Local:   http://localhost:5173/\n",
        stderr: "",
        memory_bytes: None,
        enforcement: Enforcement::Monitored,
        queue_position: None,
        queue_reason: None,
        observed_memory_bytes: Some(402_653_184),
        observed_processes: Some(7),
        limit_event: None,
    },
    DemoRun {
        id: 411,
        request_id: "run-2c9b-nextest",
        argv: &["cargo", "nextest", "run"],
        cwd: "/Users/me/Work/api",
        project_root: "/Users/me/Work/api",
        session_seed: Some("demo.codex.api"),
        state: RunState::Finished,
        outcome: Some(RunOutcome::Exited),
        exit_code: Some(0),
        signal: None,
        cleanup: CleanupState::Complete,
        ago_created: 900,
        ago_started: Some(899),
        ago_finished: Some(858),
        duration_ms: Some(41_300),
        detail: None,
        stdout: "   Compiling api v0.9.0\n    Finished test [ 41.2s]\n",
        stderr: "",
        memory_bytes: None,
        enforcement: Enforcement::None,
        queue_position: None,
        queue_reason: None,
        observed_memory_bytes: None,
        observed_processes: None,
        limit_event: None,
    },
    DemoRun {
        id: 406,
        request_id: "run-77a1-site-build",
        argv: &["node", "--max-old-space-size=256", "./scripts/build.mjs"],
        cwd: "/Users/me/Work/site",
        project_root: "/Users/me/Work/site",
        session_seed: Some("demo.cursor.site"),
        state: RunState::Finished,
        outcome: Some(RunOutcome::ResourceLimit),
        exit_code: Some(137),
        signal: Some(9),
        cleanup: CleanupState::Complete,
        ago_created: 1800,
        ago_started: Some(1799),
        ago_finished: Some(1710),
        duration_ms: Some(89_000),
        detail: Some("stopped by the monitored memory threshold"),
        stdout: "building site...\n",
        stderr: "memory threshold crossed; stopping process group\n",
        memory_bytes: Some(256 * 1024 * 1024),
        enforcement: Enforcement::Monitored,
        queue_position: None,
        queue_reason: None,
        observed_memory_bytes: Some(298_844_160),
        observed_processes: Some(6),
        limit_event: Some(
            "observed 285.0 MiB over the 256 MiB monitored threshold (sum of RSS over the process group, sampled every 200 ms); run stopped",
        ),
    },
    DemoRun {
        id: 410,
        request_id: "run-91de-e2e",
        argv: &["npm", "run", "e2e"],
        cwd: "/Users/me/Work/site",
        project_root: "/Users/me/Work/site",
        session_seed: Some("demo.cursor.site"),
        state: RunState::Finished,
        outcome: Some(RunOutcome::TimedOut),
        exit_code: None,
        signal: Some(15),
        cleanup: CleanupState::Complete,
        ago_created: 3600,
        ago_started: Some(3599),
        ago_finished: Some(3479),
        duration_ms: Some(120_000),
        detail: Some("exceeded 120s timeout; process group terminated"),
        stdout: "running e2e suite...\n",
        stderr: "timeout: no progress after 120s\n",
        memory_bytes: None,
        enforcement: Enforcement::None,
        queue_position: None,
        queue_reason: None,
        observed_memory_bytes: None,
        observed_processes: None,
        limit_event: None,
    },
    DemoRun {
        id: 409,
        request_id: "run-4a17-http",
        argv: &["python", "-m", "http.server", "8080"],
        cwd: "/Users/me/Work/docs",
        project_root: "/Users/me/Work/docs",
        session_seed: Some("demo.claude.docs"),
        state: RunState::Finished,
        outcome: Some(RunOutcome::Cancelled),
        exit_code: None,
        signal: Some(15),
        cleanup: CleanupState::Complete,
        ago_created: 7200,
        ago_started: Some(7199),
        ago_finished: Some(7180),
        duration_ms: Some(19_000),
        detail: Some("cancelled by operator"),
        stdout: "Serving HTTP on 127.0.0.1 port 8080...\n",
        stderr: "",
        memory_bytes: None,
        enforcement: Enforcement::None,
        queue_position: None,
        queue_reason: None,
        observed_memory_bytes: None,
        observed_processes: None,
        limit_event: None,
    },
    DemoRun {
        id: 408,
        request_id: "run-e08c-migrate",
        argv: &["./scripts/migrate.sh"],
        cwd: "/Users/me/Work/notes",
        project_root: "/Users/me/Work/notes",
        session_seed: None,
        state: RunState::Finished,
        outcome: Some(RunOutcome::SpawnFailed),
        exit_code: None,
        signal: None,
        cleanup: CleanupState::Unknown,
        ago_created: 86_400,
        ago_started: None,
        ago_finished: Some(86_400),
        duration_ms: Some(0),
        detail: Some("No such file or directory (os error 2)"),
        stdout: "",
        stderr: "",
        memory_bytes: None,
        enforcement: Enforcement::None,
        queue_position: None,
        queue_reason: None,
        observed_memory_bytes: None,
        observed_processes: None,
        limit_event: None,
    },
];

/// Reservation the supervisor applies when a run asks for no particular
/// amount. Kept in step with `scheduler::Limits::default()`.
const DEFAULT_RUN_MEMORY_BYTES: u64 = 512 * 1024 * 1024;

/// What the memory observation actually measures. The wording matches the
/// backend capability so the UI never reads a measurement as a kernel limit.
const OBSERVED_METRIC: &str = "sum of RSS over the run's process group, sampled every 200 ms; shared pages can be counted more than once";

/// Why `reserved_memory_bytes` must not be read as RAM in use.
const RESERVATION_NOTE: &str = "reserved_memory_bytes is a reservation (budget) held for admitted runs, not measured RAM; \
     a run's observed_memory_bytes is the measurement";

fn demo_view(r: &DemoRun, n: u64) -> RunView {
    let session_id = r.session_seed.map(|s| format!("{:016x}", stable_id(s)));
    RunView {
        run_id: r.id.to_string(),
        request_id: r.request_id.into(),
        argv: r.argv.iter().map(|s| (*s).to_string()).collect(),
        cwd: r.cwd.into(),
        project_root: Some(r.project_root.into()),
        session_id,
        state: r.state,
        outcome: r.outcome,
        exit_code: r.exit_code,
        signal: r.signal,
        cleanup: r.cleanup,
        revision: if r.state.is_terminal() { 4 } else { 2 },
        created_at: n.saturating_sub(r.ago_created),
        started_at: r.ago_started.map(|ago| n.saturating_sub(ago)),
        finished_at: r.ago_finished.map(|ago| n.saturating_sub(ago)),
        duration_ms: r.duration_ms,
        detail: r.detail.map(str::to_string),
        logs: LogState {
            stdout_bytes: r.stdout.len() as u64,
            stderr_bytes: r.stderr.len() as u64,
            stdout_truncated: false,
            stderr_truncated: false,
        },
        supervisor: Some("demo:0".into()),
        requested: ResourceRequest {
            memory_bytes: r.memory_bytes,
            cpu_millicores: None,
            processes: None,
            enforcement: r.enforcement,
            queue_timeout: None,
        },
        effective: EffectiveLimits {
            // A request with no explicit amount still gets the configured
            // default reserved, so requested and effective legitimately differ.
            memory_bytes: Some(r.memory_bytes.unwrap_or(DEFAULT_RUN_MEMORY_BYTES)),
            enforcement: r.enforcement,
            metric: (r.enforcement == Enforcement::Monitored).then(|| OBSERVED_METRIC.to_string()),
            backend: "process_group".into(),
        },
        queue: QueueInfo {
            position: r.queue_position,
            // Only a waiting request has spent time in the queue.
            waiting_ms: if r.state == RunState::Queued {
                r.ago_created.saturating_mul(1000)
            } else {
                0
            },
            reason: r.queue_reason.map(str::to_string),
        },
        observed_memory_bytes: r.observed_memory_bytes,
        observed_processes: r.observed_processes,
        limit_event: r.limit_event.map(str::to_string),
        capabilities: backend_capabilities(),
        events: Vec::new(),
    }
}

/// Synthetic managed runs, newest first. No host I/O.
pub fn runs() -> Vec<RunView> {
    let n = now();
    RUNS.iter().map(|r| demo_view(r, n)).collect()
}

pub fn run(id: i64) -> Option<RunView> {
    let n = now();
    RUNS.iter().find(|r| r.id == id).map(|r| demo_view(r, n))
}

/// Synthetic capacity derived from the same `RUNS` the dashboard lists, so
/// the two views cannot disagree. Limits are the built-in defaults; nothing
/// here inspects the host.
pub fn capacity() -> Capacity {
    let limits = crate::runner::scheduler::Limits::default().summary();
    let running: Vec<&DemoRun> = RUNS
        .iter()
        .filter(|r| r.state == RunState::Running)
        .collect();
    let queued: Vec<&DemoRun> = RUNS
        .iter()
        .filter(|r| r.state == RunState::Queued)
        .collect();

    let mut projects: Vec<ProjectUsage> = Vec::new();
    for r in running.iter().copied() {
        match projects.iter_mut().find(|p| p.project == r.project_root) {
            Some(p) => {
                p.running += 1;
                p.reserved_memory_bytes += demo_memory(r);
            }
            None => projects.push(ProjectUsage {
                project: r.project_root.to_string(),
                running: 1,
                reserved_memory_bytes: demo_memory(r),
            }),
        }
    }
    projects.sort_by(|a, b| a.project.cmp(&b.project));

    let queue: Vec<QueueEntry> = queued
        .iter()
        .copied()
        .enumerate()
        .map(|(i, r)| QueueEntry {
            run_id: r.id.to_string(),
            project: r.project_root.to_string(),
            position: r.queue_position.unwrap_or(i + 1),
            waiting_ms: r.ago_created.saturating_mul(1000),
            memory_bytes: demo_memory(r),
            reason: r.queue_reason.unwrap_or("waiting for a slot").to_string(),
        })
        .collect();

    let slots_free = limits.max_parallel.saturating_sub(running.len());
    Capacity {
        limits,
        running: running.len(),
        queued: queued.len(),
        slots_free,
        reserved_memory_bytes: running.iter().copied().map(demo_memory).sum(),
        projects,
        queue,
        over_parallel_limit: false,
        reservation_note: RESERVATION_NOTE.to_string(),
        capabilities: backend_capabilities(),
    }
}

/// Reservation a demo run holds; a run with no explicit amount gets the
/// configured default, exactly like the real scheduler.
fn demo_memory(r: &DemoRun) -> u64 {
    r.memory_bytes.unwrap_or(DEFAULT_RUN_MEMORY_BYTES)
}

/// Synthetic output chunk with the same shape as `read_run_output`. Sliced
/// from canned text — demo never opens a log file.
pub fn run_output(id: i64, stream: Stream, cursor: u64, max_bytes: usize) -> Option<Value> {
    let r = RUNS.iter().find(|r| r.id == id)?;
    let src = match stream {
        Stream::Stdout => r.stdout,
        Stream::Stderr => r.stderr,
    };
    let start = usize::try_from(cursor).unwrap_or(usize::MAX).min(src.len());
    let end = start.saturating_add(max_bytes).min(src.len());
    Some(json!({
        "stream": stream.as_str(),
        "data": &src[start..end],
        "next_cursor": end,
        "truncated": false,
        "eof": end >= src.len(),
    }))
}

/// Simulated cancel: the view is recomputed as cancelled and nothing on the
/// host is touched — the same contract as the demo kill/docker actions.
pub fn cancel_run(id: i64) -> Option<RunView> {
    let n = now();
    let r = RUNS.iter().find(|r| r.id == id)?;
    let mut view = demo_view(r, n);
    if !view.state.is_terminal() {
        let started = view.started_at.unwrap_or(n);
        view.state = RunState::Finished;
        view.outcome = Some(RunOutcome::Cancelled);
        view.signal = Some(15); // SIGTERM
        view.cleanup = CleanupState::Complete;
        view.finished_at = Some(n);
        view.duration_ms = Some(n.saturating_sub(started).saturating_mul(1000));
        view.revision += 1;
        view.detail = Some("cancelled by operator (demo)".into());
    }
    Some(view)
}
