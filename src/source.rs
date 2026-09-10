//! System-source detection for processes with no recorded agent ownership.
//!
//! `wyd why` answers "which agent session owns this process" from durable
//! provenance. When no session is recorded, the agent that started it is
//! gone — but the process still has a *system* origin. This module walks the
//! live ancestry and names it: systemd, launchd, cron, tmux, screen, ssh,
//! snap, flatpak, or an interactive shell.
//!
//! Best effort with explicit uncertainty (mirrors the platform split of
//! `platform::tty_of`): no subprocess spawning, only the process snapshot and
//! a `/proc` read for the systemd unit name on Linux.

use std::collections::{HashMap, HashSet};

use crate::model::ProcessInfo;

/// The system-level source responsible for a process existing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// Linux systemd (pid 1). `unit` is the innermost unit from cgroup, when
    /// readable.
    Systemd { unit: Option<String> },
    /// macOS launchd (pid 1).
    Launchd,
    /// cron / crond / anacron ancestor.
    Cron,
    /// tmux server ancestor.
    Tmux,
    /// GNU screen ancestor.
    Screen,
    /// sshd ancestor (remote login).
    Ssh,
    /// Snap package (executable under `/snap/`).
    Snap,
    /// Flatpak sandbox (executable under `/app/`).
    Flatpak,
    /// Interactive shell with a controlling terminal.
    InteractiveShell,
    /// No recognized source. `pid1` names pid 1 when it was reachable.
    Unknown { pid1: Option<String> },
}

impl SourceKind {
    pub fn label(&self) -> String {
        match self {
            SourceKind::Systemd { unit } => unit
                .as_deref()
                .map(|u| format!("systemd ({u})"))
                .unwrap_or_else(|| "systemd".into()),
            SourceKind::Launchd => "launchd".into(),
            SourceKind::Cron => "cron".into(),
            SourceKind::Tmux => "tmux".into(),
            SourceKind::Screen => "screen".into(),
            SourceKind::Ssh => "ssh".into(),
            SourceKind::Snap => "snap".into(),
            SourceKind::Flatpak => "flatpak".into(),
            SourceKind::InteractiveShell => "interactive shell".into(),
            SourceKind::Unknown { pid1 } => pid1
                .as_deref()
                .map(|s| format!("unknown ({s})"))
                .unwrap_or_else(|| "unknown".into()),
        }
    }
}

/// Ancestry chain plus the detected source for one target process.
///
/// `chain` is root-first: pid 1 (or the highest visible ancestor) at index 0,
/// the target's direct parent last. Empty when the target's parent is absent
/// from the snapshot (re-parented, or the parent already exited).
#[derive(Debug)]
pub struct SourceReport {
    pub chain: Vec<ProcessInfo>,
    pub source: SourceKind,
}

/// Detect the system source for `target_pid` by walking parent links.
///
/// A recognized supervisor (ssh, tmux, screen, cron, snap, flatpak) wins over
/// an interactive shell, because a shell is the weakest signal: it is usually
/// a human's terminal, not the thing keeping the process alive. pid 1
/// (systemd / launchd) is the final fallback.
pub fn detect(target_pid: u32, processes: &[ProcessInfo]) -> SourceReport {
    let by_pid: HashMap<u32, &ProcessInfo> = processes.iter().map(|p| (p.pid, p)).collect();
    let mut chain = chain_up(target_pid, &by_pid);

    let source = by_pid
        .get(&target_pid)
        .and_then(|t| sandbox_marker(t))
        .or_else(|| chain.iter().find_map(supervisor_marker))
        .or_else(|| chain.iter().find_map(shell_marker))
        .or_else(|| chain.iter().find(|p| p.pid == 1).map(pid1_of))
        .unwrap_or(SourceKind::Unknown { pid1: None });

    // Enrich systemd with the unit name (Linux only, best effort).
    let source = match source {
        SourceKind::Systemd { .. } => {
            let unit = systemd_unit_for(target_pid)
                .or_else(|| chain.iter().rev().find_map(|p| systemd_unit_for(p.pid)));
            SourceKind::Systemd { unit }
        }
        other => other,
    };

    chain.reverse();
    SourceReport { chain, source }
}

/// Parent chain from the target's parent up to (and including) pid 1, nearest
/// first. Cycle-safe against malformed snapshots; stops where the parent is no
/// longer visible.
fn chain_up(target_pid: u32, by_pid: &HashMap<u32, &ProcessInfo>) -> Vec<ProcessInfo> {
    let mut chain: Vec<ProcessInfo> = Vec::new();
    let mut seen = HashSet::new();
    let mut cur = by_pid.get(&target_pid).and_then(|p| p.parent_pid);
    while let Some(pid) = cur {
        if !seen.insert(pid) {
            break; // malformed snapshot: cycle
        }
        let Some(p) = by_pid.get(&pid).copied() else {
            break;
        };
        chain.push(p.clone());
        if pid == 1 {
            break;
        }
        cur = p.parent_pid;
    }
    chain
}

/// Render the ancestry path from pid 1 (or the highest visible ancestor) down
/// to `target_pid`, plus the target's children, as a box-drawing tree with the
/// target marked `◀`. Children are capped to keep the tree readable.
pub fn render_tree(target_pid: u32, processes: &[ProcessInfo]) -> String {
    const CHILD_CAP: usize = 10;

    let by_pid: HashMap<u32, &ProcessInfo> = processes.iter().map(|p| (p.pid, p)).collect();
    let mut chain = chain_up(target_pid, &by_pid);
    chain.reverse(); // root-first

    let target = by_pid.get(&target_pid).copied();
    let mut children: Vec<&ProcessInfo> = processes
        .iter()
        .filter(|p| p.parent_pid == Some(target_pid))
        .collect();
    children.sort_by_key(|p| p.pid);

    let label = |p: &ProcessInfo| format!("{} ({})", p.name, p.pid);
    let mut out = String::new();

    // Linear ancestry, target excluded (rendered below with its children).
    for (i, p) in chain.iter().enumerate() {
        let prefix = if i == 0 {
            String::new()
        } else {
            format!("{}└─ ", "   ".repeat(i - 1))
        };
        out.push_str(&format!("{prefix}{}\n", label(p)));
    }

    let depth = chain.len();
    let target_prefix = if depth == 0 {
        String::new()
    } else {
        format!("{}└─ ", "   ".repeat(depth - 1))
    };
    let target_label = target
        .map(label)
        .unwrap_or_else(|| format!("pid {target_pid}"));
    out.push_str(&format!("{target_prefix}{target_label} ◀\n"));

    let child_indent = "   ".repeat(depth);
    let total = children.len();
    for (j, c) in children.iter().take(CHILD_CAP).enumerate() {
        // `└─` only on the genuinely last rendered child; when truncated, the
        // `… (+N more)` line is last, so every shown child stays `├─`.
        let branch = if total <= CHILD_CAP && j + 1 == total {
            "└─ "
        } else {
            "├─ "
        };
        out.push_str(&format!("{child_indent}{branch}{}\n", label(c)));
    }
    if children.len() > CHILD_CAP {
        out.push_str(&format!(
            "{child_indent}└─ … (+{} more)\n",
            children.len() - CHILD_CAP
        ));
    }

    out
}

/// A sandbox the process itself runs in, from its executable path. Snap and
/// flatpak are properties of the process, not of what started it, so they are
/// checked on the target before the ancestor walk.
fn sandbox_marker(p: &ProcessInfo) -> Option<SourceKind> {
    let exe = p
        .executable
        .as_ref()?
        .to_string_lossy()
        .to_ascii_lowercase();
    if exe.starts_with("/snap/") {
        return Some(SourceKind::Snap);
    }
    if exe.starts_with("/app/") {
        return Some(SourceKind::Flatpak);
    }
    None
}

/// A recognized supervisor in the ancestry. Nearest (from the target) wins.
fn supervisor_marker(p: &ProcessInfo) -> Option<SourceKind> {
    sandbox_marker(p).or_else(|| {
        let name = p.name.to_ascii_lowercase();
        if name == "sshd" {
            Some(SourceKind::Ssh)
        } else if name == "tmux" || name.starts_with("tmux:") {
            Some(SourceKind::Tmux)
        } else if name == "screen" || name.starts_with("screen-") {
            Some(SourceKind::Screen)
        } else if name == "cron" || name == "crond" || name == "anacron" {
            Some(SourceKind::Cron)
        } else {
            None
        }
    })
}

/// An interactive shell: only a shell *with* a controlling terminal counts.
fn shell_marker(p: &ProcessInfo) -> Option<SourceKind> {
    if is_shell(&p.name.to_ascii_lowercase()) && p.tty.is_some() {
        Some(SourceKind::InteractiveShell)
    } else {
        None
    }
}

fn is_shell(name: &str) -> bool {
    matches!(
        name,
        "bash"
            | "zsh"
            | "sh"
            | "dash"
            | "fish"
            | "ksh"
            | "csh"
            | "tcsh"
            | "nu"
            | "pwsh"
            | "powershell"
    )
}

fn pid1_of(p: &ProcessInfo) -> SourceKind {
    match p.name.to_ascii_lowercase().as_str() {
        "systemd" => SourceKind::Systemd { unit: None },
        "launchd" => SourceKind::Launchd,
        other => SourceKind::Unknown {
            pid1: Some(other.to_string()),
        },
    }
}

/// Innermost systemd unit name from `/proc/<pid>/cgroup`.
#[cfg(target_os = "linux")]
fn systemd_unit_for(pid: u32) -> Option<String> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    unit_from_cgroup(&cgroup)
}

#[cfg(not(target_os = "linux"))]
fn systemd_unit_for(_pid: u32) -> Option<String> {
    None
}

/// Extract the innermost unit from a cgroup path line (`0::/system.slice/foo.service`).
/// Prefers a leaf unit (`.service`/`.scope`/`.socket`/`.timer`) over its parent
/// slice. Tested cross-platform; only *called* on Linux.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unit_from_cgroup(cgroup: &str) -> Option<String> {
    let segs: Vec<&str> = cgroup
        .lines()
        .flat_map(|l| l.split(':').next_back())
        .flat_map(|p| p.split('/'))
        .filter(|s| !s.is_empty())
        .collect();
    segs.iter()
        .rev()
        .find(|s| {
            s.ends_with(".service")
                || s.ends_with(".scope")
                || s.ends_with(".socket")
                || s.ends_with(".timer")
        })
        .or_else(|| segs.iter().rev().find(|s| s.ends_with(".slice")))
        .map(|s| (*s).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn proc(
        pid: u32,
        ppid: Option<u32>,
        name: &str,
        exe: Option<&str>,
        tty: Option<&str>,
    ) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: ppid,
            name: name.into(),
            command: vec![name.into()],
            executable: exe.map(PathBuf::from),
            cwd: None,
            cpu_percent: 0.0,
            memory_bytes: 0,
            start_time: 1,
            tty: tty.map(str::to_string),
        }
    }

    #[test]
    fn systemd_service_is_systemd() {
        let ps = vec![
            proc(1, None, "systemd", None, None),
            proc(1234, Some(1), "postgres", None, None),
        ];
        let r = detect(1234, &ps);
        assert!(matches!(r.source, SourceKind::Systemd { .. }));
        assert_eq!(r.chain.len(), 1);
        assert_eq!(r.chain[0].pid, 1);
    }

    #[test]
    fn launchd_orphan_is_launchd() {
        let ps = vec![
            proc(1, None, "launchd", None, None),
            proc(99, Some(1), "node", None, None),
        ];
        let r = detect(99, &ps);
        assert_eq!(r.source, SourceKind::Launchd);
    }

    #[test]
    fn cron_wins_over_shell() {
        let ps = vec![
            proc(1, None, "systemd", None, None),
            proc(10, Some(1), "cron", None, None),
            proc(11, Some(10), "sh", None, None),
            proc(12, Some(11), "job", None, None),
        ];
        let r = detect(12, &ps);
        assert_eq!(r.source, SourceKind::Cron);
    }

    #[test]
    fn ssh_wins_over_interactive_shell() {
        let ps = vec![
            proc(1, None, "systemd", None, None),
            proc(20, Some(1), "sshd", None, None),
            proc(21, Some(20), "bash", None, Some("pts/0")),
            proc(22, Some(21), "node", None, None),
        ];
        let r = detect(22, &ps);
        assert_eq!(r.source, SourceKind::Ssh);
    }

    #[test]
    fn interactive_shell_local() {
        let ps = vec![
            proc(1, None, "launchd", None, None),
            proc(30, Some(1), "zsh", None, Some("ttys001")),
            proc(31, Some(30), "node", None, None),
        ];
        let r = detect(31, &ps);
        assert_eq!(r.source, SourceKind::InteractiveShell);
    }

    #[test]
    fn tmux_server_is_tmux() {
        let ps = vec![
            proc(1, None, "systemd", None, None),
            proc(40, Some(1), "tmux: server", None, None),
            proc(41, Some(40), "vim", None, None),
        ];
        let r = detect(41, &ps);
        assert_eq!(r.source, SourceKind::Tmux);
    }

    #[test]
    fn snap_path_is_snap() {
        let ps = vec![
            proc(1, None, "systemd", None, None),
            proc(
                50,
                Some(1),
                "mysnap",
                Some("/snap/mysnap/current/bin/mysnap"),
                None,
            ),
        ];
        let r = detect(50, &ps);
        assert_eq!(r.source, SourceKind::Snap);
    }

    #[test]
    fn flatpak_path_is_flatpak() {
        let ps = vec![
            proc(1, None, "systemd", None, None),
            proc(51, Some(1), "app", Some("/app/bin/app"), None),
        ];
        let r = detect(51, &ps);
        assert_eq!(r.source, SourceKind::Flatpak);
    }

    #[test]
    fn absent_parent_is_unknown() {
        let ps = vec![proc(60, None, "orphan", None, None)];
        let r = detect(60, &ps);
        assert!(matches!(r.source, SourceKind::Unknown { .. }));
        assert!(r.chain.is_empty());
    }

    #[test]
    fn chain_is_root_first() {
        let ps = vec![
            proc(1, None, "systemd", None, None),
            proc(70, Some(1), "cron", None, None),
            proc(71, Some(70), "sh", None, None),
            proc(72, Some(71), "job", None, None),
        ];
        let r = detect(72, &ps);
        assert_eq!(r.chain.len(), 3);
        assert_eq!(r.chain[0].pid, 1);
        assert_eq!(r.chain[2].pid, 71);
    }

    #[test]
    fn tree_marks_target_and_lists_children() {
        let ps = vec![
            proc(1, None, "launchd", None, None),
            proc(100, Some(1), "omp", None, None),
            proc(110, Some(100), "node", None, None),
            proc(120, Some(110), "node", None, None),
            proc(121, Some(110), "node", None, None),
        ];
        let tree = render_tree(110, &ps);
        assert_eq!(
            tree,
            "launchd (1)\n└─ omp (100)\n   └─ node (110) ◀\n      ├─ node (120)\n      └─ node (121)\n"
        );
    }

    #[test]
    fn tree_target_is_root() {
        let ps = vec![
            proc(1, None, "launchd", None, None),
            proc(2, Some(1), "x", None, None),
        ];
        assert_eq!(render_tree(1, &ps), "launchd (1) ◀\n└─ x (2)\n");
    }

    #[test]
    fn tree_truncates_children_and_keeps_branches_open() {
        let mut ps = vec![proc(1, None, "launchd", None, None)];
        for i in 0..12 {
            ps.push(proc(100 + i, Some(1), "c", None, None));
        }
        let tree = render_tree(1, &ps);
        // Ten children shown, all `├─` (not last); truncation line is `└─`.
        assert_eq!(tree.matches("├─ c (").count(), 10);
        assert!(tree.contains("└─ … (+2 more)"));
        assert!(!tree.contains("└─ c ("));
    }

    #[test]
    fn unit_from_cgroup_extracts_service() {
        assert_eq!(
            unit_from_cgroup("0::/system.slice/postgresql.service"),
            Some("postgresql.service".into())
        );
        assert_eq!(
            unit_from_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/foo.service"
            ),
            Some("foo.service".into())
        );
        assert_eq!(unit_from_cgroup("0::/"), None);
        assert_eq!(
            unit_from_cgroup("0::/init.scope"),
            Some("init.scope".into())
        );
        assert_eq!(
            unit_from_cgroup("0::/user.slice/user-1000.slice/session-3.scope"),
            Some("session-3.scope".into())
        );
    }
}
