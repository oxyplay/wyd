use std::collections::{HashMap, HashSet};
use std::io;

use sysinfo::{Pid, ProcessesToUpdate, System};

use crate::model::{ProcessInfo, RuntimeItem};

/// Frozen process identity used to reject PID reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub pid: u32,
    pub start_time: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct KillReport {
    pub signaled: usize,
    pub skipped: usize,
    pub failed: usize,
}

/// PIDs recorded on `item` (and nested items), deepest first.
/// OS children that are not part of the item are left alone — we never killpg.
pub fn identities_for(item: &RuntimeItem, processes: &[ProcessInfo]) -> Vec<Identity> {
    let seeds = item_pids(item);
    let order = postorder(seeds, processes);
    let by_pid: HashMap<u32, &ProcessInfo> = processes.iter().map(|p| (p.pid, p)).collect();
    order
        .into_iter()
        .filter_map(|pid| {
            by_pid.get(&pid).map(|p| Identity {
                pid,
                start_time: p.start_time,
            })
        })
        .collect()
}

fn item_pids(item: &RuntimeItem) -> Vec<u32> {
    let mut pids = item.process_ids.clone();
    for child in &item.children {
        pids.extend(item_pids(child));
    }
    pids
}

fn postorder(seeds: Vec<u32>, processes: &[ProcessInfo]) -> Vec<u32> {
    let allowed: HashSet<u32> = seeds.iter().copied().collect();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in processes {
        if let Some(ppid) = p.parent_pid
            && allowed.contains(&p.pid)
            && allowed.contains(&ppid)
        {
            children.entry(ppid).or_default().push(p.pid);
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for seed in seeds {
        walk(seed, &children, &mut seen, &mut out);
    }
    out
}

fn walk(pid: u32, children: &HashMap<u32, Vec<u32>>, seen: &mut HashSet<u32>, out: &mut Vec<u32>) {
    if !seen.insert(pid) {
        return;
    }
    if let Some(kids) = children.get(&pid) {
        for &kid in kids {
            walk(kid, children, seen, out);
        }
    }
    out.push(pid);
}

/// Re-check PID + start_time, then signal deepest-first. Never uses killpg.
pub fn send(ids: &[Identity], signal: Signal) -> KillReport {
    if ids.is_empty() {
        return KillReport::default();
    }

    let mut sys = System::new();
    let pids: Vec<Pid> = ids.iter().map(|id| Pid::from_u32(id.pid)).collect();
    sys.refresh_processes(ProcessesToUpdate::Some(&pids), false);

    let mut report = KillReport::default();
    for id in ids {
        if !identity_holds(&sys, id) {
            report.skipped += 1;
            continue;
        }
        match signal_one(id.pid, signal) {
            Ok(()) => report.signaled += 1,
            Err(err) if gone(&err) => report.skipped += 1,
            Err(_) => report.failed += 1,
        }
    }
    report
}

fn identity_holds(sys: &System, id: &Identity) -> bool {
    let Some(live) = sys.process(Pid::from_u32(id.pid)) else {
        return false;
    };
    if id.start_time != 0 && live.start_time() != 0 {
        return id.start_time == live.start_time();
    }
    true
}

fn gone(err: &io::Error) -> bool {
    err.raw_os_error() == Some(libc::ESRCH)
}

fn signal_one(pid: u32, signal: Signal) -> io::Result<()> {
    #[cfg(unix)]
    {
        let sig = match signal {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        let rc = unsafe { libc::kill(pid as libc::pid_t, sig) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kill is only implemented on Unix",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Category, RuntimeItem, RuntimeState};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    fn item(pids: &[u32], children: Vec<RuntimeItem>) -> RuntimeItem {
        RuntimeItem {
            category: Category::UnknownDev,
            display_name: "sleep".into(),
            root_pid: pids.first().copied(),
            process_ids: pids.to_vec(),
            memory_bytes: 0,
            cpu_percent: 0.0,
            state: RuntimeState::Active,
            suspicion: None,
            ports: Vec::new(),
            project: None,
            children,
        }
    }

    fn proc(pid: u32, ppid: Option<u32>, start: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: ppid,
            name: "p".into(),
            command: vec!["p".into()],
            executable: None,
            cwd: None,
            cpu_percent: 0.0,
            memory_bytes: 0,
            start_time: start,
            tty: None,
        }
    }

    #[test]
    fn postorder_is_deepest_first() {
        let tree = item(&[1], vec![item(&[2], vec![item(&[3, 4], vec![])])]);
        let snap = vec![
            proc(1, None, 10),
            proc(2, Some(1), 11),
            proc(3, Some(2), 12),
            proc(4, Some(2), 13),
            proc(99, Some(1), 14), // OS child, not in the item — must not be killed
        ];
        let ids = identities_for(&tree, &snap);
        let pids: Vec<u32> = ids.iter().map(|i| i.pid).collect();
        assert_eq!(pids.last().copied(), Some(1), "root last");
        let pos = |pid| pids.iter().position(|&p| p == pid).unwrap();
        assert!(pos(3) < pos(2));
        assert!(pos(4) < pos(2));
        assert!(pos(2) < pos(1));
        assert!(!pids.contains(&99), "do not killpg the whole OS tree");
    }

    #[test]
    fn reused_pid_is_not_signaled() {
        let report = send(
            &[Identity {
                pid: std::process::id(),
                start_time: 1, // this process's real start_time is not 1
            }],
            Signal::Term,
        );
        assert_eq!(
            report,
            KillReport {
                signaled: 0,
                skipped: 1,
                failed: 0,
            }
        );
    }

    #[test]
    fn term_stops_spawned_process() {
        let child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut sys = System::new();
        let handle = Pid::from_u32(pid);
        sys.refresh_processes(ProcessesToUpdate::Some(&[handle]), false);
        let start = sys.process(handle).map(|p| p.start_time()).unwrap_or(0);

        let report = send(
            &[Identity {
                pid,
                start_time: start,
            }],
            Signal::Term,
        );
        assert_eq!(report.signaled, 1, "{report:?}");

        let mut child = child;
        for _ in 0..50 {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        panic!("sleep {pid} still alive after SIGTERM");
    }
}
