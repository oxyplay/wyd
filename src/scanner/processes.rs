use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::model::ProcessInfo;
use crate::platform::tty_of;
use crate::scanner::{ProcessScanner, Result};

/// sysinfo-backed scanner. Keeps the `System` instance alive between scans so
/// CPU usage is a real delta — no subprocesses.
pub struct SysinfoProcessScanner {
    sys: System,
}

impl SysinfoProcessScanner {
    pub fn new() -> Self {
        Self { sys: System::new() }
    }

    /// Aggregate CPU across cores; only meaningful after a second refresh.
    pub fn cpu_percent(&self) -> f32 {
        self.sys.global_cpu_usage()
    }

    pub fn memory(&self) -> (u64, u64) {
        (self.sys.used_memory(), self.sys.total_memory())
    }
}

impl ProcessScanner for SysinfoProcessScanner {
    fn scan(&mut self) -> Result<Vec<ProcessInfo>> {
        self.sys.refresh_memory();
        self.sys.refresh_cpu_all();
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_memory()
                .with_cpu()
                .with_exe(UpdateKind::OnlyIfNotSet)
                .with_cmd(UpdateKind::OnlyIfNotSet)
                .with_cwd(UpdateKind::OnlyIfNotSet),
        );

        let processes = self
            .sys
            .processes()
            .values()
            .map(|p| ProcessInfo {
                pid: p.pid().as_u32(),
                parent_pid: p.parent().map(|ppid| ppid.as_u32()),
                name: p.name().to_string_lossy().into_owned(),
                command: p
                    .cmd()
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect(),
                executable: p.exe().map(|e| e.to_path_buf()),
                cwd: p.cwd().map(|c| c.to_path_buf()),
                cpu_percent: p.cpu_usage(),
                memory_bytes: p.memory(),
                start_time: p.start_time(),
                tty: tty_of(p.pid().as_u32()),
            })
            .collect();
        Ok(processes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_real_processes_without_subprocesses() {
        let mut scanner = SysinfoProcessScanner::new();
        let procs = scanner.scan().unwrap();
        assert!(!procs.is_empty());
        // PID 0/1 exist on any Unix; every row must have a name.
        assert!(procs.iter().all(|p| !p.name.is_empty()));
        // This very test process must be visible with its real parent.
        let me = procs
            .iter()
            .find(|p| p.pid == std::process::id())
            .expect("own process in snapshot");
        assert_eq!(me.parent_pid, Some(std::os::unix::process::parent_id()));
        assert!(me.cwd.is_some(), "cwd missing for live process");
    }
}
