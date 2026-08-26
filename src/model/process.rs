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

#[cfg(test)]
mod tests {
    use super::*;

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
}
