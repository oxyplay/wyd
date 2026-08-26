use crate::model::boot::BootId;
use std::path::PathBuf;

/// One observed process.
///
/// Platform backends expose different optional fields; missing values are
/// `None` and must be tolerated by all consumers.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent_pid: Option<u32>,

    pub name: String,
    pub command: Vec<String>,
    pub executable: Option<PathBuf>,
    pub cwd: Option<PathBuf>,

    pub cpu_percent: f32,
    pub memory_bytes: u64,

    /// Seconds since Unix epoch. `0` means unknown.
    pub start_time: u64,
    pub tty: Option<String>,
}

impl ProcessInfo {
    /// Best short name: `name`, else first argv, else executable basename.
    pub fn label(&self) -> &str {
        if !self.name.is_empty() {
            return &self.name;
        }
        if let Some(cmd) = self.command.first().filter(|s| !s.is_empty()) {
            return cmd.as_str();
        }
        self.executable
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("?")
    }

    /// Lowercased `name + cmd + exe` for substring matching.
    pub fn hay(&self) -> String {
        format!(
            "{} {} {}",
            self.name.to_ascii_lowercase(),
            self.command.join(" ").to_ascii_lowercase(),
            self.executable
                .as_ref()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default()
        )
    }
}

// ponytail: consumed by steps 3–4 (sessions/exact ownership); dead until wired.
#[allow(dead_code)]
/// Stable identity of one observed process, valid only when `start_time != 0`.
///
/// Never identity a process by `pid` alone: PIDs are reused. `boot_id` +
/// `pid` + `start_time` uniquely identify one process invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    pub boot_id: BootId,
    pub pid: u32,
    pub start_time: u64,
}

#[allow(dead_code)]
impl ProcessIdentity {
    /// Construct from a live process. Returns `None` when `start_time == 0`
    /// (identity unavailable → live-only attribution, never persisted).
    pub fn from_process(boot_id: &BootId, process: &ProcessInfo) -> Option<Self> {
        (process.start_time != 0).then_some(Self {
            boot_id: *boot_id,
            pid: process.pid,
            start_time: process.start_time,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::boot::BootId;

    fn blank() -> ProcessInfo {
        ProcessInfo {
            pid: 1,
            parent_pid: None,
            name: String::new(),
            command: vec![],
            executable: None,
            cwd: None,
            cpu_percent: 0.0,
            memory_bytes: 0,
            start_time: 0,
            tty: None,
        }
    }

    #[test]
    fn label_prefers_name_then_argv_then_exe() {
        let mut p = blank();
        p.name = "node".into();
        assert_eq!(p.label(), "node");
        p.name.clear();
        p.command = vec!["/usr/bin/node".into()];
        assert_eq!(p.label(), "/usr/bin/node");
        p.command.clear();
        p.executable = Some(PathBuf::from("/usr/bin/python3"));
        assert_eq!(p.label(), "python3");
        p.executable = None;
        assert_eq!(p.label(), "?");
    }

    fn proc_with(pid: u32, start_time: u64) -> ProcessInfo {
        let mut p = blank();
        p.pid = pid;
        p.start_time = start_time;
        p
    }

    #[test]
    fn valid_start_time_yields_identity() {
        let boot = BootId::from_u128(7);
        let id = ProcessIdentity::from_process(&boot, &proc_with(100, 1000))
            .expect("nonzero start_time");
        assert_eq!(
            id,
            ProcessIdentity {
                boot_id: boot,
                pid: 100,
                start_time: 1000
            }
        );
    }

    #[test]
    fn zero_start_time_yields_none() {
        let boot = BootId::from_u128(7);
        assert!(ProcessIdentity::from_process(&boot, &proc_with(100, 0)).is_none());
    }

    #[test]
    fn same_pid_same_start_is_same_identity() {
        let boot = BootId::from_u128(7);
        let a = ProcessIdentity::from_process(&boot, &proc_with(100, 1000)).unwrap();
        let b = ProcessIdentity::from_process(&boot, &proc_with(100, 1000)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn same_pid_different_start_is_different_identity() {
        let boot = BootId::from_u128(7);
        let a = ProcessIdentity::from_process(&boot, &proc_with(100, 1000)).unwrap();
        let b = ProcessIdentity::from_process(&boot, &proc_with(100, 2000)).unwrap();
        assert_ne!(a, b, "PID reuse must yield a different identity");
    }

    #[test]
    fn different_boot_is_different_identity() {
        let a =
            ProcessIdentity::from_process(&BootId::from_u128(1), &proc_with(100, 1000)).unwrap();
        let b =
            ProcessIdentity::from_process(&BootId::from_u128(2), &proc_with(100, 1000)).unwrap();
        assert_ne!(a, b, "identical pid/start across boots must differ");
    }
}
