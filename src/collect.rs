use crate::classify::{ProjectCache, attach, group, mark};
use crate::config::Config;
use crate::model::RuntimeSnapshot;
use crate::scanner::{ProcessScanner, ports, processes::SysinfoProcessScanner};

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
    let docker = crate::scanner::docker::scan_blocking();
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
