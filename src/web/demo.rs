//! Deterministic synthetic dataset for `wyd web --demo`. **No host I/O**, no
//! scanners, no Docker. Reuses `RuntimeSnapshot` so the same JSON shapes
//! apply, and the same `proposal` rules select correctly.

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

pub fn session_infos() -> Vec<SessionInfo> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let opencode_started = now.saturating_sub(47 * 60);
    let codex_started = now.saturating_sub(11 * 60);
    vec![
        SessionInfo {
            id: RuntimeSessionId::from_u64(stable_id("demo.opencode.wyd")),
            agent: "opencode".into(),
            project: Some("~/Work/wyd".into()),
            started_at: opencode_started,
            active: false,
        },
        SessionInfo {
            id: RuntimeSessionId::from_u64(stable_id("demo.codex.api")),
            agent: "codex".into(),
            project: Some("~/Work/api".into()),
            started_at: codex_started,
            active: true,
        },
    ]
}

pub fn session_record(id: u64) -> Option<SessionRecord> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let opencode_started = now.saturating_sub(47 * 60);
    let codex_started = now.saturating_sub(11 * 60);
    let a = stable_id("demo.opencode.wyd");
    let b = stable_id("demo.codex.api");
    if id == a {
        Some(SessionRecord {
            id: RuntimeSessionId::from_u64(a),
            agent: "opencode".into(),
            project: Some("~/Work/wyd".into()),
            started_at: opencode_started,
            last_seen_at: opencode_started + 600,
            ended_at: Some(opencode_started + 600),
        })
    } else if id == b {
        Some(SessionRecord {
            id: RuntimeSessionId::from_u64(b),
            agent: "codex".into(),
            project: Some("~/Work/api".into()),
            started_at: codex_started,
            last_seen_at: codex_started,
            ended_at: None,
        })
    } else {
        None
    }
}

pub fn snapshot() -> RuntimeSnapshot {
    let procs = vec![
        proc(4100, "opencode", &["opencode", "~/Work/wyd"], 120 << 20),
        proc(
            4101,
            "chrome-devtools-mcp",
            &["chrome-devtools-mcp"],
            60 << 20,
        ),
        proc(4102, "Chromium", &["Chromium", "--headless"], 150 << 20),
        proc(4103, "vite", &["vite", "--port", "5173"], 120 << 20),
        proc(5100, "codex", &["codex", "~/Work/api"], 90 << 20),
        proc(5101, "rust-analyzer", &["rust-analyzer"], 220 << 20),
        proc(5102, "cargo-watch", &["cargo-watch"], 35 << 20),
        proc(9100, "postgres", &["postgres"], 320 << 20),
        proc(9101, "redis-server", &["redis-server"], 18 << 20),
    ];

    let project_wyd = Some(Project {
        name: "wyd".into(),
        root: std::path::PathBuf::from("/Users/me/Work/wyd"),
    });
    let project_api = Some(Project {
        name: "api".into(),
        root: std::path::PathBuf::from("/Users/me/Work/api"),
    });

    let mut items = vec![
        RuntimeItem {
            category: Category::Browser,
            display_name: "Chromium x8".into(),
            root_pid: Some(4102),
            process_ids: vec![4102],
            memory_bytes: (8u64 * 150) << 20,
            cpu_percent: 0.1,
            state: RuntimeState::Suspicious,
            suspicion: Some(Suspicion {
                score: 80,
                reasons: vec![
                    SuspicionReason::SessionOwnerEnded,
                    SuspicionReason::HeadlessBrowserDetached,
                ],
            }),
            ports: vec![],
            project: project_wyd.clone(),
            children: vec![],
        },
        RuntimeItem {
            category: Category::DevServer,
            display_name: "vite :5173".into(),
            root_pid: Some(4103),
            process_ids: vec![4103],
            memory_bytes: 120 << 20,
            cpu_percent: 0.0,
            state: RuntimeState::Suspicious,
            suspicion: Some(Suspicion {
                score: 60,
                reasons: vec![
                    SuspicionReason::SessionOwnerEnded,
                    SuspicionReason::LongRunningDevServer,
                ],
            }),
            ports: vec![port(5173)],
            project: project_wyd.clone(),
            children: vec![],
        },
        RuntimeItem {
            category: Category::Mcp,
            display_name: "chrome-devtools-mcp".into(),
            root_pid: Some(4101),
            process_ids: vec![4101],
            memory_bytes: 60 << 20,
            cpu_percent: 0.0,
            state: RuntimeState::Suspicious,
            suspicion: Some(Suspicion {
                score: 50,
                reasons: vec![SuspicionReason::SessionOwnerEnded],
            }),
            ports: vec![],
            project: project_wyd.clone(),
            children: vec![],
        },
        RuntimeItem {
            category: Category::LanguageServer,
            display_name: "rust-analyzer".into(),
            root_pid: Some(5101),
            process_ids: vec![5101],
            memory_bytes: 220 << 20,
            cpu_percent: 1.2,
            state: RuntimeState::Active,
            suspicion: None,
            ports: vec![],
            project: project_api.clone(),
            children: vec![],
        },
        RuntimeItem {
            category: Category::Worker,
            display_name: "cargo-watch".into(),
            root_pid: Some(5102),
            process_ids: vec![5102],
            memory_bytes: 35 << 20,
            cpu_percent: 0.0,
            state: RuntimeState::Active,
            suspicion: None,
            ports: vec![],
            project: project_api.clone(),
            children: vec![],
        },
        RuntimeItem {
            category: Category::Database,
            display_name: "postgres".into(),
            root_pid: Some(9100),
            process_ids: vec![9100],
            memory_bytes: 320 << 20,
            cpu_percent: 0.0,
            state: RuntimeState::Persistent,
            suspicion: None,
            ports: vec![port(5432)],
            project: None,
            children: vec![],
        },
        RuntimeItem {
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
        },
    ];

    // Nest Chromium under chrome-devtools-mcp to mirror real topology.
    let chromium_pos = items
        .iter()
        .position(|i| i.display_name == "Chromium x8")
        .unwrap();
    let chromium = items.remove(chromium_pos);
    let mcp_pos = items
        .iter()
        .position(|i| i.display_name == "chrome-devtools-mcp")
        .unwrap();
    items[mcp_pos].children.push(chromium);

    RuntimeSnapshot {
        processes: procs,
        logical_items: items,
        docker: Arc::new(DockerSnapshot::default()),
        total_memory_bytes: 16u64 << 30,
        used_memory_bytes: 4u64 << 30,
        cpu_percent: 4.2,
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
    let session_id = match item.display_name.as_str() {
        "rust-analyzer" | "cargo-watch" => stable_id("demo.codex.api"),
        "postgres" | "redis" => 0,
        _ => stable_id("demo.opencode.wyd"),
    };
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
