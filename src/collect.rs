use crate::classify::ownership::resolver::{AnchorKind, CandidateInput, ResolverRules, resolve};
use crate::classify::ownership::{ResourceId, derive_ownership};
use crate::classify::{ProjectCache, attach, group, mark};
use crate::config::Config;
use crate::model::Category;
use crate::model::ProcessInfo;
use crate::model::RuntimeSnapshot;
use crate::model::boot::BootId;
use crate::model::process::ProcessIdentity;
use crate::model::runtime::RuntimeItem;
use crate::model::session::{RuntimeSessionId, SessionInfo};
use crate::platform::{BootIdentityProvider, SystemBoot};
use crate::scanner::{ProcessScanner, ports, processes::SysinfoProcessScanner};
use crate::store::RuntimeStore;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// One scan of processes, ports, Docker, and leftover scores.
/// Shared by the TUI loop and `--json` / `--plain`.
pub fn snapshot() -> RuntimeSnapshot {
    let cfg = Config::global();
    let mut scanner = SysinfoProcessScanner::new();
    let mut projects = ProjectCache::with_roots(cfg.project_roots());
    let processes = scanner.scan().unwrap_or_default();
    let ports = ports::scan().unwrap_or_default();
    let mut logical_items = group(&processes);
    attach(&mut logical_items, &processes, &ports, &mut projects);
    mark(&mut logical_items, &processes, cfg);
    let docker = Arc::new(crate::scanner::docker::scan_blocking());
    let (used, total) = scanner.memory();
    RuntimeSnapshot {
        logical_items,
        processes,
        docker,
        total_memory_bytes: total,
        used_memory_bytes: used,
        cpu_percent: scanner.cpu_percent(),
        // The one-shot CLI path has no store-backed sessions.
        sessions: Vec::new(),
        version: 1,
    }
}

/// Persists exact runtime ownership for live runs. Degrades to a no-op when
/// the store or boot identity is unavailable — persistence never breaks the
/// TUI, matching the "scanner failures degrade, never crash" convention.
pub struct OwnershipTracker {
    store: Option<RuntimeStore>,
    boot: Option<BootId>,
    gc_pending: bool,
}

impl OwnershipTracker {
    pub fn new() -> Self {
        let mut store = RuntimeStore::open(&RuntimeStore::default_path()).ok();
        let now = now();
        let boot = store.as_mut().and_then(|s| {
            SystemBoot
                .current_boot_epoch()
                .ok()
                .and_then(|e| s.boot_id_for_epoch(e, now).ok())
        });
        Self {
            store,
            boot,
            gc_pending: true,
        }
    }

    /// Record the exact-observed ownership of one scan, and end any session
    /// whose root process is no longer live. Best-effort: any failure is
    /// swallowed so the live view is never affected.
    pub fn record(&mut self, processes: &[ProcessInfo], items: &[RuntimeItem]) {
        let (Some(store), Some(boot)) = (self.store.as_mut(), self.boot) else {
            return;
        };
        let identities = identities(processes, &boot);
        let now = now();
        let result = derive_ownership(items, &identities, now);
        let _ = store.apply_ownership(&result, now);
        let live_roots: HashSet<ProcessIdentity> = result.sessions.iter().map(|s| s.root).collect();
        let _ = store.end_absent_sessions(&live_roots, now);
        // Resolve + persist a decision for resources with recorded provenance
        // (exact ancestry may already be gone). Heuristic only for gaps.
        for item in items {
            attribution_for(item, &identities, store, &boot, now);
        }
        // GC once per run, using the first scan's real live-process set.
        if self.gc_pending {
            let live: HashSet<ProcessIdentity> = identities.values().copied().collect();
            let _ = store.gc(RETENTION_SECS, now, &live);
            self.gc_pending = false;
        }
    }

    /// Snapshot-friendly view of known sessions, for the TUI.
    pub fn sessions(&self) -> Vec<SessionInfo> {
        let Some(store) = self.store.as_ref() else {
            return Vec::new();
        };
        match store.sessions() {
            Ok(rows) => rows
                .into_iter()
                .map(|s| SessionInfo {
                    id: s.id,
                    agent: s.agent,
                    project: s.project,
                    active: s.ended_at.is_none(),
                    started_at: s.started_at,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Layer session-ended leftover marks into the live items (contract §17):
    /// any non-persistent resource whose origin session ended is a strong
    /// leftover candidate. No-op when the store is unavailable.
    pub fn layer_session_leftovers(&self, items: &mut [RuntimeItem], processes: &[ProcessInfo]) {
        let (Some(store), Some(boot)) = (self.store.as_ref(), self.boot) else {
            return;
        };
        let identities = identities(processes, &boot);
        for item in items {
            session_leftover_one(item, &identities, store, &boot);
        }
    }
}

/// Resolve and persist an attribution decision for a resource with recorded
/// provenance (contract §7–8, §16). Persists only the first decision per
/// resource — decisions are historical (§14), so re-running every scan would
/// grow the table needlessly.
fn attribution_for(
    item: &RuntimeItem,
    identities: &HashMap<u32, ProcessIdentity>,
    store: &mut RuntimeStore,
    boot: &BootId,
    now: u64,
) {
    for child in &item.children {
        attribution_for(child, identities, store, boot, now);
    }
    if item.category == Category::Agent {
        return;
    }
    let Some(root) = item.root_pid.and_then(|p| identities.get(&p).copied()) else {
        return; // no stable identity → live-only
    };
    let resource_id = ResourceId::of_root(&root).as_u64();
    if store.latest_decision(resource_id).ok().flatten().is_some() {
        return; // already decided
    }

    // Anchor (§7): recorded provenance for any process of this resource.
    let mut pids: Vec<u32> = item.process_ids.clone();
    if let Some(rp) = item.root_pid {
        pids.push(rp);
    }
    // (session, started_at, session project root)
    let mut owner: Option<(RuntimeSessionId, u64, Option<String>)> = None;
    for pid in pids {
        if let Some(id) = identities.get(&pid)
            && let Ok(Some(exp)) = store.explain_process(boot, id.pid, id.start_time)
        {
            owner = Some((exp.owner, exp.session.started_at, exp.session.project));
            break;
        }
    }
    let Some((owner, session_start, session_project)) = owner else {
        return; // no anchor → not a candidate (§7)
    };

    // Project family (§8): a cwd match with the owning session's project
    // corroborates attribution.
    let resource_project = item
        .project
        .as_ref()
        .map(|p| p.root.to_string_lossy().to_string());
    let exact_cwd = resource_project.is_some() && resource_project == session_project;

    let predates = root.start_time < session_start;
    let cand = CandidateInput {
        session: owner,
        anchor: Some(AnchorKind::PersistedPrevious),
        anchor_score: Some(75),
        propagate_cap: None,
        exact_cwd,
        same_git_root: false,
        start_delta_secs: (!predates).then_some(root.start_time.saturating_sub(session_start)),
        tool_relationship: matches!(
            item.category,
            Category::Mcp | Category::Browser | Category::LanguageServer
        ),
        predates_session: predates,
        reaches_other_agent: false,
    };
    let decision = resolve(&ResolverRules::v1(), vec![cand]);
    let _ = store.persist_decision(resource_id, &decision, now);
}

fn session_leftover_one(
    item: &mut RuntimeItem,
    identities: &HashMap<u32, ProcessIdentity>,
    store: &RuntimeStore,
    boot: &BootId,
) {
    for child in &mut item.children {
        session_leftover_one(child, identities, store, boot);
    }
    if item.category == crate::model::Category::Agent
        || item.state == crate::model::RuntimeState::Persistent
    {
        return;
    }

    // A process of this resource has a recorded origin session that ended.
    let mut origin_ended = false;
    let mut pids: Vec<u32> = item.process_ids.clone();
    if let Some(rp) = item.root_pid {
        pids.push(rp);
    }
    for pid in pids {
        let Some(id) = identities.get(&pid) else {
            continue;
        };
        let Ok(Some(exp)) = store.explain_process(boot, id.pid, id.start_time) else {
            continue;
        };
        if exp.session.ended_at.is_some() {
            origin_ended = true;
            break;
        }
    }
    if !origin_ended {
        return;
    }

    item.state = crate::model::RuntimeState::Suspicious;
    let reason = crate::model::SuspicionReason::SessionOwnerEnded;
    match &mut item.suspicion {
        Some(s) => {
            if !s.reasons.contains(&reason) {
                s.reasons.push(reason);
                s.score = s.score.saturating_add(30).min(100);
            }
        }
        None => {
            item.suspicion = Some(crate::model::Suspicion {
                score: 30,
                reasons: vec![reason],
            });
        }
    }
}

/// Apply session-ended leftover marks to live items (shared by the TUI
/// tracker layer and the CLI one-shot path).
pub fn apply_session_leftovers(
    items: &mut [RuntimeItem],
    processes: &[ProcessInfo],
    store: &RuntimeStore,
    boot: &BootId,
) {
    let identities = identities(processes, boot);
    for item in items {
        session_leftover_one(item, &identities, store, boot);
    }
}

/// Provenance retention before GC (contract §13 `[history] retention_days`).
const RETENTION_SECS: u64 = 30 * 24 * 3600;

fn identities(processes: &[ProcessInfo], boot: &BootId) -> HashMap<u32, ProcessIdentity> {
    processes
        .iter()
        .filter_map(|p| ProcessIdentity::from_process(boot, p).map(|id| (p.pid, id)))
        .collect()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{group, ownership::derive_ownership};
    use crate::model::{Category, RuntimeState, SuspicionReason};

    fn proc(pid: u32, ppid: Option<u32>, name: &str, cmd: &[&str], start: u64) -> ProcessInfo {
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
            tty: None,
        }
    }

    fn mcp_item() -> RuntimeItem {
        RuntimeItem {
            category: Category::Mcp,
            display_name: "chrome-devtools-mcp".into(),
            root_pid: Some(110),
            process_ids: vec![110],
            memory_bytes: 0,
            cpu_percent: 0.0,
            state: RuntimeState::Active,
            suspicion: None,
            ports: vec![],
            project: None,
            children: vec![],
        }
    }

    #[test]
    fn ended_origin_session_marks_the_item_a_leftover() {
        let boot = BootId::from_u128(7);
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(100, Some(1), "omp", &["omp"], 1000),
            proc(
                110,
                Some(100),
                "node",
                &["node", "chrome-devtools-mcp"],
                1004,
            ),
        ];
        let mut store = RuntimeStore::open_in_memory().unwrap();
        let out = derive_ownership(&group(&procs), &identities(&procs, &boot), 1000);
        store.apply_ownership(&out, 1000).unwrap();
        store.end_absent_sessions(&HashSet::new(), 2000).unwrap(); // agent gone

        // The MCP reappears detached (agent parent gone).
        let mut mcp = mcp_item();
        session_leftover_one(&mut mcp, &identities(&procs, &boot), &store, &boot);

        assert_eq!(mcp.state, RuntimeState::Suspicious);
        let s = mcp.suspicion.expect("session-ended mark");
        assert!(s.reasons.contains(&SuspicionReason::SessionOwnerEnded));
        assert_eq!(s.score, 30);
    }

    #[test]
    fn active_origin_session_does_not_mark_leftover() {
        let boot = BootId::from_u128(7);
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(100, Some(1), "omp", &["omp"], 1000),
            proc(
                110,
                Some(100),
                "node",
                &["node", "chrome-devtools-mcp"],
                1004,
            ),
        ];
        let mut store = RuntimeStore::open_in_memory().unwrap();
        let out = derive_ownership(&group(&procs), &identities(&procs, &boot), 1000);
        store.apply_ownership(&out, 1000).unwrap();
        // Session stays active: no end_absent_sessions call.

        let mut mcp = mcp_item();
        session_leftover_one(&mut mcp, &identities(&procs, &boot), &store, &boot);
        assert_eq!(mcp.state, RuntimeState::Active);
        assert!(mcp.suspicion.is_none());
    }

    #[test]
    fn persistent_items_are_never_marked() {
        let boot = BootId::from_u128(7);
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(100, Some(1), "omp", &["omp"], 1000),
            proc(
                110,
                Some(100),
                "node",
                &["node", "chrome-devtools-mcp"],
                1004,
            ),
        ];
        let mut store = RuntimeStore::open_in_memory().unwrap();
        let out = derive_ownership(&group(&procs), &identities(&procs, &boot), 1000);
        store.apply_ownership(&out, 1000).unwrap();
        store.end_absent_sessions(&HashSet::new(), 2000).unwrap();

        let mut mcp = mcp_item();
        mcp.state = RuntimeState::Persistent; // e.g. a service
        session_leftover_one(&mut mcp, &identities(&procs, &boot), &store, &boot);
        assert_eq!(mcp.state, RuntimeState::Persistent);
        assert!(mcp.suspicion.is_none());
    }

    #[test]
    fn attribution_persists_a_decision_for_a_detached_resource() {
        let boot = BootId::from_u128(7);
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(100, Some(1), "omp", &["omp"], 1000),
            proc(
                110,
                Some(100),
                "node",
                &["node", "chrome-devtools-mcp"],
                1004,
            ),
        ];
        let mut store = RuntimeStore::open_in_memory().unwrap();
        let out = derive_ownership(&group(&procs), &identities(&procs, &boot), 1000);
        store.apply_ownership(&out, 1000).unwrap();

        // The MCP now appears detached (agent parent gone) but its provenance
        // is recorded.
        let item = mcp_item();
        attribution_for(&item, &identities(&procs, &boot), &mut store, &boot, 2000);

        let root = ProcessIdentity {
            boot_id: boot,
            pid: 110,
            start_time: 1004,
        };
        let decision = store
            .latest_decision(ResourceId::of_root(&root).as_u64())
            .unwrap()
            .expect("a decision must be persisted");
        assert_eq!(decision.verdict, "Owned");
        let owner = store.owning_session_for_process(&boot, 110, 1004).unwrap();
        assert_eq!(
            decision.winner_session, owner,
            "resolver re-affirms the recorded owner"
        );
    }

    #[test]
    fn attribution_is_idempotent_per_resource() {
        let boot = BootId::from_u128(7);
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(100, Some(1), "omp", &["omp"], 1000),
            proc(
                110,
                Some(100),
                "node",
                &["node", "chrome-devtools-mcp"],
                1004,
            ),
        ];
        let mut store = RuntimeStore::open_in_memory().unwrap();
        let out = derive_ownership(&group(&procs), &identities(&procs, &boot), 1000);
        store.apply_ownership(&out, 1000).unwrap();

        let item = mcp_item();
        let ids = identities(&procs, &boot);
        attribution_for(&item, &ids, &mut store, &boot, 2000);
        attribution_for(&item, &ids, &mut store, &boot, 3000); // second scan

        let root = ProcessIdentity {
            boot_id: boot,
            pid: 110,
            start_time: 1004,
        };
        let count = store
            .decision_count(ResourceId::of_root(&root).as_u64())
            .unwrap();
        assert_eq!(
            count, 1,
            "a second identical scan must not add another decision"
        );
    }
}
