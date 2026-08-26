use crate::classify::ownership::derive_ownership;
use crate::classify::{ProjectCache, attach, group, mark};
use crate::config::Config;
use crate::model::ProcessInfo;
use crate::model::RuntimeSnapshot;
use crate::model::boot::BootId;
use crate::model::process::ProcessIdentity;
use crate::model::runtime::RuntimeItem;
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
        // GC once per run, using the first scan's real live-process set.
        if self.gc_pending {
            let live: HashSet<ProcessIdentity> = identities.values().copied().collect();
            let _ = store.gc(RETENTION_SECS, now, &live);
            self.gc_pending = false;
        }
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
