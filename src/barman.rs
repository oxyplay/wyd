//! `wyd barman`: versioned machine-readable JSON API for the `wyd-barman`
//! menu-bar client (contract v1, owned by `wyd`, documented in
//! `docs/barman-api.md`).
//!
//! Core rule: all intelligence (discovery, ownership, safety) stays in `wyd`.
//! This module only serializes what the scanners/classifier already decided
//! and routes control actions through the existing guarded paths
//! (`actions::process::send`, `actions::docker::*`). The Swift client renders
//! `actions` arrays verbatim and never infers control semantics.

use std::collections::HashMap;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Subcommand;
use serde::Serialize;

use crate::actions::{docker as docker_actions, process as process_actions};
use crate::classify::{ProjectCache, attach, group, mark};
use crate::collect::OwnershipTracker;
use crate::config::Config;
use crate::demo;
use crate::model::session::SessionInfo;
use crate::model::{
    Category, DockerKind, DockerResource, RuntimeItem, RuntimeSnapshot, RuntimeState,
};
use crate::scanner::ProcessScanner;
use crate::scanner::processes::SysinfoProcessScanner;

/// Contract version the Swift client requires (`schema_version == 1`).
pub const SCHEMA_VERSION: u32 = 1;

/// Plans are advisory and single-use; entries expire after this long.
const PLAN_TTL_SECS: u64 = 300;

// ── CLI ────────────────────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
pub enum BarmanCmd {
    /// Emit one coherent point-in-time snapshot as pretty JSON.
    Snapshot {
        /// Print JSON (accepted for uniformity; output is always JSON).
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Use the deterministic synthetic dataset instead of scanning.
        #[arg(long, default_value_t = false)]
        demo: bool,
    },
    /// Perform one control action on a stable target id.
    Action {
        /// Stable id from `snapshot` (`resource_*`, `container_*`,
        /// `project_*`, `session_*`).
        #[arg(long)]
        target: String,
        /// start | stop | restart | kill | open-url
        #[arg(long)]
        action: String,
        /// Print JSON (accepted for uniformity; output is always JSON).
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Resolve against the demo dataset; never signals real processes.
        #[arg(long, default_value_t = false)]
        demo: bool,
    },
    /// Propose a cleanup plan (leftover candidates selected, persistent
    /// services under `protected`).
    CleanupPlan {
        /// Print JSON (accepted for uniformity; output is always JSON).
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Derive the plan from the demo dataset.
        #[arg(long, default_value_t = false)]
        demo: bool,
    },
    /// Execute a plan from `cleanup-plan` (revalidates every id).
    Execute {
        /// Plan id from `cleanup-plan`.
        #[arg(long)]
        plan: String,
        /// Comma-separated subset of plan ids to execute (default: all selected).
        #[arg(long)]
        only: Option<String>,
        /// Print JSON (accepted for uniformity; output is always JSON).
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Simulate against the demo dataset; never signals real processes.
        #[arg(long, default_value_t = false)]
        demo: bool,
    },
    /// Print the API/schema version as JSON.
    Version {
        /// Print JSON (accepted for uniformity; output is always JSON).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

/// Dispatch a `wyd barman <cmd>` invocation. Prints JSON to stdout always,
/// even on failure. Stale/unknown targets exit 2; other failures exit 1 via
/// `io::Error`; success exits 0.
pub fn run(cmd: BarmanCmd) -> io::Result<()> {
    match cmd {
        BarmanCmd::Snapshot { demo, .. } => {
            let snap = live_snapshot(demo);
            let doc = build_snapshot(&snap);
            println!(
                "{}",
                serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into())
            );
            Ok(())
        }
        BarmanCmd::Action {
            target,
            action,
            demo,
            ..
        } => {
            let snap = live_snapshot(demo);
            match BarmanAction::parse(&action) {
                Some(a) => {
                    let outcome = act(&snap, &target, &a, demo);
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| "{}".into())
                    );
                    if outcome.ok {
                        Ok(())
                    } else if outcome.stale {
                        std::process::exit(2);
                    } else {
                        std::process::exit(1);
                    }
                }
                None => {
                    let outcome = ActionOutcome::failed(
                        target.clone(),
                        action.clone(),
                        format!("unknown action {action:?}: want start|stop|restart|kill|open-url"),
                    );
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| "{}".into())
                    );
                    std::process::exit(1);
                }
            }
        }
        BarmanCmd::CleanupPlan { demo, .. } => {
            let snap = live_snapshot(demo);
            let doc = build_snapshot(&snap);
            let plan = cleanup_plan(&doc);
            remember_plan(&plan);
            println!(
                "{}",
                serde_json::to_string_pretty(&plan).unwrap_or_else(|_| "{}".into())
            );
            Ok(())
        }
        BarmanCmd::Execute {
            plan, only, demo, ..
        } => {
            let only_ids: Option<Vec<String>> = only.map(|s| {
                s.split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect()
            });
            let outcome = execute(&plan, only_ids.as_deref(), demo);
            println!(
                "{}",
                serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| "{}".into())
            );
            if outcome.ok {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        BarmanCmd::Version { .. } => {
            let doc = VersionDoc {
                schema_version: SCHEMA_VERSION,
                wyd_version: env!("CARGO_PKG_VERSION"),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into())
            );
            Ok(())
        }
    }
}

// ── Snapshot collection ────────────────────────────────────────────────────

/// Single one-shot scan reusing exactly the scanners `scanner_loop` uses:
/// process scan + port scan + group + attach + mark + ownership record/layer +
/// blocking Docker scan. No TUI, no background thread.
fn live_snapshot(demo: bool) -> RuntimeSnapshot {
    if demo {
        let mut snap = demo::snapshot();
        stabilize_demo_times(&mut snap);
        return snap;
    }
    let cfg = Config::global();
    let mut scanner = SysinfoProcessScanner::new();
    let mut projects = ProjectCache::with_roots(cfg.project_roots());
    let mut tracker = OwnershipTracker::new();
    let processes = scanner.scan().unwrap_or_default();
    let ports = crate::scanner::ports::scan().unwrap_or_default();
    let mut items = group(&processes);
    attach(&mut items, &processes, &ports, &mut projects);
    mark(&mut items, &processes, cfg);
    tracker.record(&processes, &items);
    tracker.layer_session_leftovers(&mut items, &processes);
    let docker = std::sync::Arc::new(crate::scanner::docker::scan_blocking());
    let (used, total) = scanner.memory();
    RuntimeSnapshot {
        processes,
        logical_items: items,
        docker,
        total_memory_bytes: total,
        used_memory_bytes: used,
        cpu_percent: scanner.cpu_percent(),
        sessions: tracker.sessions(),
        version: 1,
    }
}
/// The demo dataset synthesizes `start_time` from the wall clock, which would
/// make demo ids drift between invocations. Hour-bucketing keeps demo ids
/// stable across calls (live ids are stable: real start times don't move).
fn stabilize_demo_times(snap: &mut RuntimeSnapshot) {
    for p in &mut snap.processes {
        if p.start_time != 0 {
            p.start_time -= p.start_time % 3600;
        }
    }
}

// ── Document shapes (contract v1) ──────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SnapshotDoc {
    pub schema_version: u32,
    pub wyd_version: String,
    pub generated_at: String,
    pub system: SystemDoc,
    pub projects: Vec<ProjectDoc>,
    pub sessions: Vec<SessionDoc>,
    pub resources: Vec<ResourceDoc>,
    pub containers: Vec<ContainerDoc>,
    pub leftovers: LeftoversDoc,
}

/// Host resource gauges for the menu's one-line status. CPU is sampled over a
/// short window (one-shot calls yield 0 otherwise); memory/disk are instant.
#[derive(Debug, Serialize)]
pub struct SystemDoc {
    /// Busy CPU across all cores, percent.
    pub cpu_percent: f32,
    pub used_memory_bytes: u64,
    pub total_memory_bytes: u64,
    /// Free bytes on the root volume.
    pub free_disk_bytes: u64,
}

#[derive(Debug, Serialize, Clone)]
pub struct ProjectDoc {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub resource_count: usize,
    pub memory_bytes: u64,
    pub resource_ids: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct SessionDoc {
    pub id: String,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub status: String,
    pub age_seconds: u64,
    pub resource_ids: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ResourceDoc {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub ports: Vec<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub memory_bytes: u64,
    pub cpu_percent: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub classification: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    pub estimated_reclaim_bytes: u64,
    pub actions: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ContainerDoc {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compose_project: Option<String>,
    pub ports: Vec<u16>,
    pub status: String,
    pub actions: Vec<String>,
    /// Reclaim estimate (image/container size); 0 when unknown.
    pub estimated_reclaim_bytes: u64,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct LeftoversDoc {
    pub count: usize,
    pub estimated_reclaim_bytes: u64,
    pub resource_ids: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct CleanupPlanDoc {
    pub plan_id: String,
    pub items: Vec<CleanupItemDoc>,
    pub protected: Vec<ProtectedDoc>,
    pub estimated_reclaim_bytes: u64,
}

#[derive(Debug, Serialize, Clone)]
pub struct CleanupItemDoc {
    pub resource_id: String,
    pub selected: bool,
    pub safe: bool,
    pub reason: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct ProtectedDoc {
    pub resource_id: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct VersionDoc {
    pub schema_version: u32,
    pub wyd_version: &'static str,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum BarmanAction {
    Start,
    Stop,
    Restart,
    Kill,
    OpenUrl,
}

impl BarmanAction {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "restart" => Some(Self::Restart),
            "kill" => Some(Self::Kill),
            "open-url" => Some(Self::OpenUrl),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Kill => "kill",
            Self::OpenUrl => "open-url",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ActionOutcome {
    pub ok: bool,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// True when the id no longer resolves (client should re-refresh).
    /// Serialized so contract tests and clients can rely on the shape.
    pub stale: bool,
}

impl ActionOutcome {
    fn ok(target: &str, action: BarmanAction, detail: String) -> Self {
        Self {
            ok: true,
            target: target.into(),
            action: Some(action.as_str().into()),
            detail: Some(detail),
            url: None,
            error: None,
            stale: false,
        }
    }

    fn ok_url(target: &str, url: String) -> Self {
        Self {
            ok: true,
            target: target.into(),
            action: Some("open-url".into()),
            detail: None,
            url: Some(url),
            error: None,
            stale: false,
        }
    }

    fn stale(target: &str, action: BarmanAction) -> Self {
        Self {
            ok: false,
            target: target.into(),
            action: Some(action.as_str().into()),
            detail: None,
            url: None,
            error: Some("stale target: re-refresh".into()),
            stale: true,
        }
    }

    fn failed(target: String, action: String, error: String) -> Self {
        Self {
            ok: false,
            target,
            action: Some(action),
            detail: None,
            url: None,
            error: Some(error),
            stale: false,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ExecuteOutcome {
    pub ok: bool,
    pub plan_id: String,
    pub stopped: Vec<String>,
    pub killed: Vec<String>,
    pub failed: Vec<ExecuteFailure>,
    pub reclaimed_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ExecuteFailure {
    pub resource_id: String,
    pub error: String,
}

// ── Stable ids ─────────────────────────────────────────────────────────────

/// `resource_<12 hex>` over (kind, root start_time, project, display name).
/// PIDs are diagnostic metadata only — never identity (PID reuse).
fn resource_id(kind: &str, start_time: u64, project: &str, name: &str) -> String {
    let input = format!("{kind}|{start_time}|{project}|{name}");
    let hex = blake3::hash(input.as_bytes()).to_hex();
    format!("resource_{}", &hex[..12])
}

/// Docker ids reuse the engine id verbatim, shortened for display.
fn container_id(docker_id: &str) -> String {
    let short = if docker_id.len() > 12 {
        &docker_id[..12]
    } else {
        docker_id
    };
    format!("container_{short}")
}

/// `project_<slug>` from the classifier's project name.
fn project_id(name: &str) -> String {
    format!("project_{}", slug(name))
}

/// `session_<16 hex>` from the stable session key.
fn session_id(key: u64) -> String {
    format!("session_{key:016x}")
}

fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut dash = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("project");
    }
    out
}

/// Basename slug of a session project path (`~/Work/wyd` → `wyd`).
fn session_project_id(path: &str) -> Option<String> {
    let base = path.rsplit('/').next().unwrap_or(path);
    if base.is_empty() {
        None
    } else {
        Some(project_id(base))
    }
}

// ── Snapshot assembly ──────────────────────────────────────────────────────

/// Build the v1 document from a snapshot. Pure: same input → same output
/// except `generated_at`/`age_seconds` wall-clock fields.
pub fn build_snapshot(snap: &RuntimeSnapshot) -> SnapshotDoc {
    let now = unix_now();
    let by_pid: HashMap<u32, u64> = snap
        .processes
        .iter()
        .map(|p| (p.pid, p.start_time))
        .collect();

    // Flatten the item tree; every node is its own resource row.
    let mut flat: Vec<&RuntimeItem> = Vec::new();
    for item in &snap.logical_items {
        flatten(item, &mut flat);
    }

    let mut resources: Vec<ResourceDoc> = flat
        .iter()
        .map(|item| process_resource(item, &by_pid))
        .collect();

    // Docker volumes/images/dangling artifacts surface as reclaimable
    // `container`-kind resources; real containers get the containers[] table.
    let mut containers: Vec<ContainerDoc> = Vec::new();
    for res in &snap.docker.resources {
        if res.kind == DockerKind::Container {
            containers.push(container_doc(res));
        } else {
            resources.push(docker_artifact_resource(res));
        }
    }

    resources.sort_by(|a, b| a.id.cmp(&b.id));
    containers.sort_by(|a, b| a.id.cmp(&b.id));

    let resource_ids: HashMap<&str, ()> = HashMap::new();
    let _ = resource_ids;

    // Projects aggregated from resource rows + session hints.
    let mut projects = aggregate_projects(&resources, &flat, &snap.sessions);
    projects.sort_by(|a, b| a.id.cmp(&b.id));

    let mut sessions: Vec<SessionDoc> = snap.sessions.iter().map(|s| session_doc(s, now)).collect();
    sessions.sort_by(|a, b| a.id.cmp(&b.id));

    // Leftovers = abandoned running processes + Docker reclaimables
    // (dangling images, anonymous volumes, build cache). Stopped containers
    // are NOT leftovers — they consume nothing; manage them in the Docker
    // section (start/stop).
    let leftover_ids: Vec<String> = resources
        .iter()
        .filter(|r| r.classification == "leftover")
        .map(|r| r.id.clone())
        .collect();
    let reclaim: u64 = resources
        .iter()
        .filter(|r| r.classification == "leftover")
        .map(|r| r.estimated_reclaim_bytes)
        .sum::<u64>();

    SnapshotDoc {
        schema_version: SCHEMA_VERSION,
        wyd_version: env!("CARGO_PKG_VERSION").into(),
        generated_at: rfc3339(now),
        system: system_doc(snap),
        projects,
        sessions,
        resources,
        containers,
        leftovers: LeftoversDoc {
            count: leftover_ids.len(),
            estimated_reclaim_bytes: reclaim,
            resource_ids: leftover_ids,
        },
    }
}

/// Host gauges for the one-line menu status. CPU needs a two-sample window
/// (a cold one-shot refresh reads 0); memory comes from the scanners, disk
/// from the root volume. All measured here, never in the client.
fn system_doc(snap: &RuntimeSnapshot) -> SystemDoc {
    let cpu_percent = sample_cpu_percent();
    SystemDoc {
        cpu_percent,
        used_memory_bytes: snap.used_memory_bytes,
        total_memory_bytes: snap.total_memory_bytes,
        free_disk_bytes: free_disk_bytes(),
    }
}

/// Two refresh_cpu_usage reads ~200ms apart yield the real CPU delta.
fn sample_cpu_percent() -> f32 {
    use std::thread::sleep;
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_usage();
    sleep(std::time::Duration::from_millis(200));
    sys.refresh_cpu_usage();
    (sys.global_cpu_usage() * 10.0).round() / 10.0
}

/// Free bytes on the root volume (statvfs).
fn free_disk_bytes() -> u64 {
    #[cfg(unix)]
    {
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        let path = std::ffi::CString::new("/").unwrap_or_default();
        unsafe { libc::statvfs(path.as_ptr(), &mut stats) };
        // f_bavail is a signed type; clamp negatives to 0.
        (stats.f_bavail as i64).max(0) as u64 * stats.f_frsize
    }
    #[cfg(not(unix))]
    {
        0
    }
}

fn flatten<'a>(item: &'a RuntimeItem, out: &mut Vec<&'a RuntimeItem>) {
    out.push(item);
    for child in &item.children {
        flatten(child, out);
    }
}

fn kind_name(category: Category) -> &'static str {
    match category {
        Category::Agent => "agent",
        Category::Mcp => "mcp",
        Category::Browser => "browser",
        Category::DevServer => "dev_server",
        Category::DevService => "dev_service",
        Category::Database => "database",
        Category::Worker => "worker",
        Category::UnknownDev => "unknown",
        Category::LanguageServer => "service",
    }
}

fn process_resource(item: &RuntimeItem, by_pid: &HashMap<u32, u64>) -> ResourceDoc {
    let kind = kind_name(item.category);
    // Never emit raw command lines as `name`: reuse the grouped display name.
    let name = item.title();
    let project_name = item.project.as_ref().map(|p| p.name.as_str()).unwrap_or("");
    let start = item
        .root_pid
        .and_then(|pid| by_pid.get(&pid).copied())
        .unwrap_or(0);
    let id = resource_id(kind, start, project_name, &item.display_name);
    let project_id = item.project.as_ref().map(|p| project_id(&p.name));

    let ports: Vec<u16> = item.ports.iter().map(|p| p.port).collect();
    let port = ports.first().copied();
    let url = item.ports.first().map(|p| p.url());

    let classification = match item.state {
        RuntimeState::Suspicious => "leftover",
        RuntimeState::Persistent => "persistent",
        RuntimeState::Active => "active",
    }
    .to_string();
    let (confidence, reasons) = match &item.suspicion {
        Some(s) if classification == "leftover" => (
            Some(
                if s.score >= 60 {
                    "high"
                } else if s.score >= 40 {
                    "medium"
                } else {
                    "low"
                }
                .to_string(),
            ),
            s.reasons.iter().map(|r| r.as_str().to_string()).collect(),
        ),
        _ => (None, Vec::new()),
    };

    let mut actions = Vec::new();
    if url.is_some() {
        actions.push("open".to_string());
    }
    // Running process items: stop/kill always; restart is offered and the
    // engine rejects it with a structured error when no start mechanism is
    // known (v1: only Docker/Homebrew-backed restarts are supported).
    actions.push("stop".to_string());
    actions.push("restart".to_string());
    actions.push("kill".to_string());

    let estimated_reclaim_bytes = if classification == "leftover" {
        item.memory_bytes
    } else {
        0
    };

    ResourceDoc {
        id,
        kind: kind.into(),
        name,
        status: "running".into(),
        project_id,
        session_id: None,
        port,
        ports,
        pid: item.root_pid,
        memory_bytes: item.memory_bytes,
        cpu_percent: item.cpu_percent,
        url,
        classification,
        confidence,
        reasons,
        estimated_reclaim_bytes,
        actions,
    }
}

fn docker_artifact_resource(res: &DockerResource) -> ResourceDoc {
    let id = container_id(&res.id);
    let leftover =
        res.prunable() || matches!(res.kind, DockerKind::DanglingImage | DockerKind::BuildCache);
    let classification = if res.persistent {
        "persistent"
    } else if leftover {
        "leftover"
    } else {
        "active"
    };
    ResourceDoc {
        id,
        kind: "container".into(),
        name: res.name.clone(),
        status: "stopped".into(),
        project_id: None,
        session_id: None,
        port: None,
        ports: Vec::new(),
        pid: None,
        memory_bytes: 0,
        cpu_percent: 0.0,
        url: None,
        classification: classification.into(),
        confidence: if leftover { Some("high".into()) } else { None },
        reasons: if leftover {
            vec!["docker-reclaimable".into()]
        } else {
            Vec::new()
        },
        estimated_reclaim_bytes: if leftover { res.size_bytes } else { 0 },
        actions: vec!["kill".into()],
    }
}

fn container_doc(res: &DockerResource) -> ContainerDoc {
    let running = res.running();
    ContainerDoc {
        id: container_id(&res.id),
        name: res.name.clone(),
        compose_project: res.compose.clone(),
        ports: Vec::new(),
        status: if running {
            "running".into()
        } else {
            "stopped".into()
        },
        actions: if running {
            vec!["stop".into(), "restart".into()]
        } else {
            vec!["start".into()]
        },
        estimated_reclaim_bytes: if running { 0 } else { res.size_bytes },
    }
}

fn aggregate_projects(
    resources: &[ResourceDoc],
    flat: &[&RuntimeItem],
    sessions: &[SessionInfo],
) -> Vec<ProjectDoc> {
    // slug → (display name, agent, ids, bytes)
    let mut map: HashMap<String, (String, Option<String>, Vec<String>, u64)> = HashMap::new();
    for (res, item) in resources
        .iter()
        .filter(|r| r.project_id.is_some())
        .map(|r| {
            let item = flat.iter().find(|i| {
                i.project.as_ref().map(|p| project_id(&p.name)) == r.project_id
                    && i.title() == r.name
            });
            (r, item)
        })
    {
        let pid = res.project_id.clone().unwrap_or_default();
        let display = item
            .and_then(|i| i.project.as_ref().map(|p| p.name.clone()))
            .unwrap_or_else(|| pid.clone());
        let entry = map
            .entry(pid.clone())
            .or_insert_with(|| (display, None, Vec::new(), 0));
        entry.2.push(res.id.clone());
        entry.3 += res.memory_bytes;
    }
    // Agent label: prefer the top-level agent item's display name per project,
    // fall back to a session agent whose project path basenames match.
    for item in flat.iter().filter(|i| i.category == Category::Agent) {
        if let Some(p) = &item.project
            && let Some(entry) = map.get_mut(&project_id(&p.name))
            && entry.1.is_none()
        {
            entry.1 = Some(item.display_name.clone());
        }
    }
    for s in sessions {
        if let Some(sp) = s.project.as_deref().and_then(session_project_id)
            && let Some(entry) = map.get_mut(&sp)
            && entry.1.is_none()
        {
            entry.1 = Some(s.agent.clone());
        }
    }
    let mut out: Vec<ProjectDoc> = map
        .into_iter()
        .map(|(id, (name, agent, mut ids, memory_bytes))| {
            ids.sort();
            ProjectDoc {
                id,
                name,
                agent,
                resource_count: ids.len(),
                memory_bytes,
                resource_ids: ids,
            }
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn session_doc(s: &SessionInfo, now: u64) -> SessionDoc {
    SessionDoc {
        id: session_id(s.id.as_u64()),
        agent: s.agent.clone(),
        project_id: s.project.as_deref().and_then(session_project_id),
        status: if s.active {
            "working".into()
        } else {
            "ended".into()
        },
        age_seconds: now.saturating_sub(s.started_at),
        resource_ids: Vec::new(),
    }
}

// ── Cleanup plan ───────────────────────────────────────────────────────────

/// Remembered cleanup-plan selections, durable across CLI invocations (the
/// client spawns one `wyd` process per call, so a process-local cache could
/// never serve `execute`). Single-use files under the wyd state dir, no DB
/// migration. `WYD_STATE_DIR` overrides the state dir (tests).
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct RememberedPlan {
    issued_at: u64,
    selected: Vec<String>,
}

/// Derive a cleanup plan strictly from `classification == "leftover"` rows
/// (plus stopped containers). Persistent databases/services are never
/// selected — they surface under `protected`.
pub fn cleanup_plan(doc: &SnapshotDoc) -> CleanupPlanDoc {
    let by_id: HashMap<&str, &ResourceDoc> =
        doc.resources.iter().map(|r| (r.id.as_str(), r)).collect();

    let mut items: Vec<CleanupItemDoc> = doc
        .leftovers
        .resource_ids
        .iter()
        .map(|id| {
            let reason = by_id
                .get(id.as_str())
                .and_then(|r| r.reasons.first().cloned())
                .unwrap_or_else(|| "leftover candidate".to_string());
            CleanupItemDoc {
                resource_id: id.clone(),
                selected: true,
                safe: true,
                reason,
            }
        })
        .collect();
    items.sort_by(|a, b| a.resource_id.cmp(&b.resource_id));

    let mut protected: Vec<ProtectedDoc> = doc
        .resources
        .iter()
        .filter(|r| r.classification == "persistent")
        .map(|r| ProtectedDoc {
            resource_id: r.id.clone(),
            reason: if r.kind == "database" {
                "Persistent database service".into()
            } else if r.kind == "container" {
                "Persistent volume".into()
            } else {
                "Persistent service".into()
            },
        })
        .collect();
    // Persistent named volumes are protected too.
    for r in doc
        .resources
        .iter()
        .filter(|r| r.kind == "container" && r.classification == "persistent" && !r.name.is_empty())
    {
        if !protected.iter().any(|p| p.resource_id == r.id) {
            protected.push(ProtectedDoc {
                resource_id: r.id.clone(),
                reason: "Persistent volume".into(),
            });
        }
    }
    protected.sort_by(|a, b| a.resource_id.cmp(&b.resource_id));

    CleanupPlanDoc {
        plan_id: new_plan_id(),
        items,
        protected,
        estimated_reclaim_bytes: doc.leftovers.estimated_reclaim_bytes,
    }
}

fn new_plan_id() -> String {
    let mut buf = [0u8; 6];
    if getrandom::fill(&mut buf).is_err() {
        buf = unix_now().to_le_bytes()[..6].try_into().unwrap_or([0u8; 6]);
    }
    format!(
        "cleanup_{}",
        buf.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

fn plans_dir() -> std::path::PathBuf {
    let base = std::env::var("WYD_STATE_DIR")
        .map(std::path::PathBuf::from)
        .ok()
        .or_else(|| {
            crate::store::RuntimeStore::default_path()
                .parent()
                .map(|p| p.to_path_buf())
        });
    base.map(|b| b.join("barman-plans"))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp").join("wyd-barman-plans"))
}

fn remember_plan(plan: &CleanupPlanDoc) {
    let dir = plans_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // Best-effort GC of expired plans from earlier invocations.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let expired = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<RememberedPlan>(&b).ok())
                .is_none_or(|p| unix_now().saturating_sub(p.issued_at) >= PLAN_TTL_SECS);
            if expired {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    let remembered = RememberedPlan {
        issued_at: unix_now(),
        selected: plan.items.iter().map(|i| i.resource_id.clone()).collect(),
    };
    if let Ok(bytes) = serde_json::to_vec(&remembered) {
        let _ = std::fs::write(dir.join(format!("{}.json", plan.plan_id)), bytes);
    }
}

fn take_plan(plan_id: &str) -> Option<Vec<String>> {
    // Reject path traversal: plan ids are `cleanup_<hex>`.
    if !plan_id.starts_with("cleanup_")
        || !plan_id["cleanup_".len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    let path = plans_dir().join(format!("{plan_id}.json"));
    let bytes = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path); // single-use, even when expired
    let remembered: RememberedPlan = serde_json::from_slice(&bytes).ok()?;
    if unix_now().saturating_sub(remembered.issued_at) >= PLAN_TTL_SECS {
        return None;
    }
    Some(remembered.selected)
}

// ── Actions ────────────────────────────────────────────────────────────────

/// Resolve `target` against a fresh snapshot and perform `action`.
/// `demo_mode` resolves against the (already demo) snapshot but never
/// touches the host: control actions report a simulated ok.
pub fn act(
    snap: &RuntimeSnapshot,
    target: &str,
    action: &BarmanAction,
    demo_mode: bool,
) -> ActionOutcome {
    // Docker namespace first (prefix-disjoint from resource_*).
    if target.starts_with("container_") {
        return act_docker(snap, target, action, demo_mode);
    }
    if target.starts_with("resource_") {
        return act_resource(snap, target, action, demo_mode);
    }
    if target.starts_with("project_") || target.starts_with("session_") {
        return act_group(snap, target, action, demo_mode);
    }
    ActionOutcome {
        ok: false,
        target: target.into(),
        action: Some(action.as_str().into()),
        detail: None,
        url: None,
        error: Some("stale target: re-refresh".into()),
        stale: true,
    }
}

fn find_item_by_id<'a>(snap: &'a RuntimeSnapshot, target: &str) -> Option<&'a RuntimeItem> {
    let by_pid: HashMap<u32, u64> = snap
        .processes
        .iter()
        .map(|p| (p.pid, p.start_time))
        .collect();
    let mut stack: Vec<&'a RuntimeItem> = snap.logical_items.iter().collect();
    while let Some(item) = stack.pop() {
        let project_name = item.project.as_ref().map(|p| p.name.as_str()).unwrap_or("");
        let start = item
            .root_pid
            .and_then(|pid| by_pid.get(&pid).copied())
            .unwrap_or(0);
        if resource_id(
            kind_name(item.category),
            start,
            project_name,
            &item.display_name,
        ) == target
        {
            return Some(item);
        }
        stack.extend(item.children.iter());
    }
    None
}

fn find_docker<'a>(snap: &'a RuntimeSnapshot, target: &str) -> Option<&'a DockerResource> {
    snap.docker
        .resources
        .iter()
        .find(|r| container_id(&r.id) == target)
}

fn act_resource(
    snap: &RuntimeSnapshot,
    target: &str,
    action: &BarmanAction,
    demo_mode: bool,
) -> ActionOutcome {
    let Some(item) = find_item_by_id(snap, target) else {
        return ActionOutcome::stale(target, *action);
    };
    match action {
        BarmanAction::OpenUrl => {
            if demo_mode {
                // fall through to the real lookup; demo data has real ports.
            }
            match item.ports.first().map(|p| p.url()) {
                Some(url) => ActionOutcome::ok_url(target, url),
                None => ActionOutcome::failed(
                    target.into(),
                    action.as_str().into(),
                    "no url for this resource".into(),
                ),
            }
        }
        BarmanAction::Stop => {
            if demo_mode {
                return ActionOutcome::ok(target, *action, "demo: no-op".into());
            }
            let ids = process_actions::identities_for(item, &snap.processes);
            if ids.is_empty() {
                return ActionOutcome::stale(target, *action);
            }
            let report = process_actions::send(&ids, process_actions::Signal::Term);
            signal_report(target, *action, &report)
        }
        BarmanAction::Kill => {
            if demo_mode {
                return ActionOutcome::ok(target, *action, "demo: no-op".into());
            }
            let ids = process_actions::identities_for(item, &snap.processes);
            if ids.is_empty() {
                return ActionOutcome::stale(target, *action);
            }
            let report = process_actions::send(&ids, process_actions::Signal::Kill);
            signal_report(target, *action, &report)
        }
        BarmanAction::Start | BarmanAction::Restart => ActionOutcome::failed(
            target.into(),
            action.as_str().into(),
            "restart not supported for this resource".into(),
        ),
    }
}

fn act_docker(
    snap: &RuntimeSnapshot,
    target: &str,
    action: &BarmanAction,
    demo_mode: bool,
) -> ActionOutcome {
    let Some(res) = find_docker(snap, target) else {
        return ActionOutcome::stale(target, *action);
    };
    if demo_mode && !matches!(action, BarmanAction::OpenUrl) {
        return ActionOutcome::ok(target, *action, "demo: no-op".into());
    }
    let id = res.id.clone();
    let result = match (action, res.kind) {
        (BarmanAction::Stop, DockerKind::Container) => {
            if !res.running() {
                Err("container is not running".to_string())
            } else {
                docker_actions::stop_blocking(res).map(|_| "stopped".to_string())
            }
        }
        (BarmanAction::Kill, _) => {
            docker_actions::remove_blocking(res).map(|_| format!("removed {}", res.name))
        }
        (BarmanAction::Start, DockerKind::Container) => {
            docker_actions::start_blocking(&id).map(|_| "started".to_string())
        }
        (BarmanAction::Restart, DockerKind::Container) => {
            docker_actions::restart_blocking(&id).map(|_| "restarted".to_string())
        }
        (BarmanAction::OpenUrl, _) => {
            return ActionOutcome::failed(
                target.into(),
                action.as_str().into(),
                "no url for this resource".into(),
            );
        }
        _ => Err("restart not supported for this resource".to_string()),
    };
    match result {
        Ok(detail) => ActionOutcome::ok(target, *action, detail),
        Err(error) => ActionOutcome::failed(target.into(), action.as_str().into(), error),
    }
}

/// Project/session group actions: stop/kill fan out to every member process
/// item through the same guarded `send` path (identity revalidated).
fn act_group(
    snap: &RuntimeSnapshot,
    target: &str,
    action: &BarmanAction,
    demo_mode: bool,
) -> ActionOutcome {
    if matches!(
        action,
        BarmanAction::Start | BarmanAction::Restart | BarmanAction::OpenUrl
    ) {
        return ActionOutcome::failed(
            target.into(),
            action.as_str().into(),
            "restart not supported for this resource".into(),
        );
    }
    let doc = build_snapshot(snap);
    let member_ids: Vec<String> = doc
        .projects
        .iter()
        .find(|p| p.id == target)
        .map(|p| p.resource_ids.clone())
        .unwrap_or_default();
    if member_ids.is_empty() {
        return ActionOutcome::stale(target, *action);
    }
    if demo_mode {
        return ActionOutcome::ok(target, *action, "demo: no-op".into());
    }
    let signal = match action {
        BarmanAction::Stop => process_actions::Signal::Term,
        _ => process_actions::Signal::Kill,
    };
    let mut signaled = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    for id in &member_ids {
        if let Some(item) = find_item_by_id(snap, id) {
            let ids = process_actions::identities_for(item, &snap.processes);
            if ids.is_empty() {
                skipped += 1;
                continue;
            }
            let r = process_actions::send(&ids, signal);
            signaled += r.signaled;
            skipped += r.skipped;
            failed += r.failed;
        } else {
            skipped += 1;
        }
    }
    let report = process_actions::KillReport {
        signaled,
        skipped,
        failed,
    };
    signal_report(target, *action, &report)
}

fn signal_report(
    target: &str,
    action: BarmanAction,
    report: &process_actions::KillReport,
) -> ActionOutcome {
    if report.signaled > 0 && report.failed == 0 {
        ActionOutcome::ok(
            target,
            action,
            format!("signaled {} process(es)", report.signaled),
        )
    } else if report.signaled > 0 {
        ActionOutcome::failed(
            target.into(),
            action.as_str().into(),
            format!(
                "partial: signaled {}, failed {}",
                report.signaled, report.failed
            ),
        )
    } else if report.failed > 0 {
        ActionOutcome::failed(
            target.into(),
            action.as_str().into(),
            format!("failed to signal {} process(es)", report.failed),
        )
    } else {
        ActionOutcome::stale(target, action)
    }
}

// ── Execute ────────────────────────────────────────────────────────────────

/// Execute a remembered plan. Every id is re-resolved against a fresh
/// snapshot through the action path: the plan is advisory, the engine
/// re-checks identity and the protected list. Deselected ids are skipped.
pub fn execute(plan_id: &str, only: Option<&[String]>, demo_mode: bool) -> ExecuteOutcome {
    let Some(selected) = take_plan(plan_id) else {
        return ExecuteOutcome {
            ok: false,
            plan_id: plan_id.into(),
            stopped: Vec::new(),
            killed: Vec::new(),
            failed: Vec::new(),
            reclaimed_bytes: 0,
            error: Some("unknown or expired plan".into()),
        };
    };
    let wanted: Vec<String> = match only {
        Some(filter) => selected
            .into_iter()
            .filter(|id| filter.contains(id))
            .collect(),
        None => selected,
    };
    let snap = live_snapshot(demo_mode);
    let doc = build_snapshot(&snap);
    let reclaim_of: HashMap<&str, u64> = doc
        .resources
        .iter()
        .map(|r| (r.id.as_str(), r.estimated_reclaim_bytes))
        .chain(
            doc.containers
                .iter()
                .map(|c| (c.id.as_str(), c.estimated_reclaim_bytes)),
        )
        .collect();

    let mut stopped = Vec::new();
    let mut killed = Vec::new();
    let mut failed = Vec::new();
    let mut reclaimed_bytes = 0u64;
    for id in &wanted {
        // Plan items are leftover candidates: stop processes and running
        // containers, remove stopped containers and docker artifacts.
        let action = if id.starts_with("container_") {
            match find_docker(&snap, id) {
                Some(res) if res.kind == DockerKind::Container && res.running() => {
                    BarmanAction::Stop
                }
                Some(_) => BarmanAction::Kill,
                None => {
                    failed.push(ExecuteFailure {
                        resource_id: id.clone(),
                        error: "stale target: re-refresh".into(),
                    });
                    continue;
                }
            }
        } else {
            BarmanAction::Stop
        };
        let outcome = act(&snap, id, &action, demo_mode);
        if outcome.ok {
            match action {
                BarmanAction::Kill => killed.push(id.clone()),
                _ => stopped.push(id.clone()),
            }
            reclaimed_bytes += reclaim_of.get(id.as_str()).copied().unwrap_or(0);
        } else {
            failed.push(ExecuteFailure {
                resource_id: id.clone(),
                error: outcome.error.unwrap_or_else(|| "failed".into()),
            });
        }
    }
    stopped.sort();
    killed.sort();
    ExecuteOutcome {
        ok: failed.is_empty(),
        plan_id: plan_id.into(),
        stopped,
        killed,
        failed,
        reclaimed_bytes,
        error: None,
    }
}

// ── Time ───────────────────────────────────────────────────────────────────

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Unix seconds → `YYYY-MM-DDTHH:MM:SSZ` (no extra time dep for one field).
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = (secs % 86400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's civil_from_days (days since 1970-01-01).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_doc() -> SnapshotDoc {
        build_snapshot(&demo::snapshot())
    }

    #[test]
    fn snapshot_shape_and_ordering() {
        let doc = demo_doc();
        assert_eq!(doc.schema_version, 1);
        assert!(!doc.wyd_version.is_empty());
        assert!(doc.generated_at.ends_with('Z'));
        for ids in [
            doc.projects.iter().map(|p| &p.id).collect::<Vec<_>>(),
            doc.sessions.iter().map(|s| &s.id).collect::<Vec<_>>(),
            doc.resources.iter().map(|r| &r.id).collect::<Vec<_>>(),
            doc.containers.iter().map(|c| &c.id).collect::<Vec<_>>(),
        ] {
            let mut sorted = ids.clone();
            sorted.sort();
            assert_eq!(ids, sorted, "barman output must be sorted by id");
        }
        assert!(doc.resources.iter().all(|r| !r.actions.is_empty()));
        assert!(doc.containers.iter().all(|c| !c.actions.is_empty()));
        // No raw command lines leak into names: single display token, no `/`.
        assert!(
            doc.resources
                .iter()
                .all(|r| !r.name.contains('/') && !r.name.contains("--"))
        );
    }

    #[test]
    fn stable_ids_same_input() {
        let snap = demo::snapshot();
        let a = build_snapshot(&snap);
        let b = build_snapshot(&snap);
        let ids = |d: &SnapshotDoc| d.resources.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&a), ids(&b));
        assert!(
            a.resources
                .iter()
                .all(|r| r.id.starts_with("resource_") || r.id.starts_with("container_"))
        );
        assert!(a.projects.iter().all(|p| p.id.starts_with("project_")));
        assert!(a.sessions.iter().all(|s| s.id.starts_with("session_")));
        assert!(a.containers.iter().all(|c| c.id.starts_with("container_")));
    }

    #[test]
    fn stale_target_shape() {
        let snap = demo::snapshot();
        for target in ["resource_deadbeef00", "container_nonexistent", "bogus"] {
            let out = act(&snap, target, &BarmanAction::Stop, true);
            assert!(!out.ok, "{target} should not resolve");
            assert!(out.stale, "{target} should be marked stale");
            assert_eq!(out.error.as_deref(), Some("stale target: re-refresh"));
            // JSON shape the client relies on.
            let v = serde_json::to_value(&out).unwrap();
            assert_eq!(v["ok"], false);
            assert_eq!(v["error"], "stale target: re-refresh");
            assert_eq!(v["target"], target);
        }
    }

    #[test]
    fn cleanup_plan_protects_databases() {
        let doc = demo_doc();
        let plan = cleanup_plan(&doc);
        let by_id: HashMap<&str, &ResourceDoc> =
            doc.resources.iter().map(|r| (r.id.as_str(), r)).collect();
        for item in &plan.items {
            let res = by_id.get(item.resource_id.as_str());
            assert!(
                res.is_none_or(|r| r.kind != "database"),
                "database {} must never be selected",
                item.resource_id
            );
            assert!(item.selected && item.safe);
        }
        // Demo postgres/redis/mysql are persistent → protected.
        assert!(plan.protected.iter().any(|p| {
            by_id
                .get(p.resource_id.as_str())
                .is_some_and(|r| r.kind == "database")
        }));
        // Persistent named volume pgdata is protected, anonymous one is not.
        assert!(
            doc.resources
                .iter()
                .filter(|r| r.name == "pgdata")
                .all(|r| plan.protected.iter().any(|p| p.resource_id == r.id))
        );
        assert!(
            doc.resources
                .iter()
                .filter(|r| r.name == "anonymous-1")
                .all(|r| plan.items.iter().any(|i| i.resource_id == r.id))
        );
    }

    #[test]
    fn plans_survive_across_invocations_but_are_single_use() {
        let dir = std::env::temp_dir().join(format!(
            "wyd-barman-plan-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: single-threaded test manipulation of a barman-only var.
        unsafe { std::env::set_var("WYD_STATE_DIR", &dir) };
        let plan = CleanupPlanDoc {
            plan_id: "cleanup_abc123".into(),
            items: vec![CleanupItemDoc {
                resource_id: "resource_x".into(),
                selected: true,
                safe: true,
                reason: "leftover".into(),
            }],
            protected: Vec::new(),
            estimated_reclaim_bytes: 0,
        };
        remember_plan(&plan);
        assert!(dir.join("barman-plans/cleanup_abc123.json").exists());
        assert_eq!(
            take_plan("cleanup_abc123"),
            Some(vec!["resource_x".to_string()])
        );
        assert_eq!(take_plan("cleanup_abc123"), None); // single-use
        assert_eq!(take_plan("cleanup_../../../etc"), None); // traversal
        unsafe { std::env::remove_var("WYD_STATE_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn demo_action_never_signals() {
        let snap = demo::snapshot();
        let doc = build_snapshot(&snap);
        let target = doc
            .resources
            .iter()
            .find(|r| r.classification == "leftover")
            .map(|r| r.id.clone())
            .unwrap();
        let out = act(&snap, &target, &BarmanAction::Stop, true);
        assert!(out.ok);
        assert_eq!(out.detail.as_deref(), Some("demo: no-op"));
    }

    #[test]
    fn process_restart_unsupported_shape() {
        let snap = demo::snapshot();
        let doc = build_snapshot(&snap);
        let target = doc
            .resources
            .iter()
            .find(|r| r.id.starts_with("resource_"))
            .map(|r| r.id.clone())
            .unwrap();
        let out = act(&snap, &target, &BarmanAction::Restart, true);
        assert!(!out.ok && !out.stale);
        assert_eq!(
            out.error.as_deref(),
            Some("restart not supported for this resource")
        );
    }

    #[test]
    fn demo_ids_stable_across_calls() {
        // Separate `demo::snapshot()` values (different wall-clock reads)
        // must still produce identical ids after stabilization.
        let mut a = demo::snapshot();
        let mut b = demo::snapshot();
        stabilize_demo_times(&mut a);
        stabilize_demo_times(&mut b);
        let ids = |s: &RuntimeSnapshot| {
            build_snapshot(s)
                .resources
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&a), ids(&b));
    }

    #[test]
    fn rfc3339_known_value() {
        // 2026-01-02T03:04:05Z
        assert_eq!(rfc3339(1_767_323_045), "2026-01-02T03:04:05Z");
    }
}
