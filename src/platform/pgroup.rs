//! Process-group primitives for managed runs.
//!
//! A managed run executes in a process group created before the user command
//! starts, so stopping the run can signal the whole group without the
//! heuristics the generic kill path deliberately avoids. Group membership is
//! the only thing we own; a command that calls `setsid`, double-forks or
//! hands work to an external daemon leaves the group, and that escape is
//! reported honestly rather than papered over.
//!
//! No subprocesses: `/proc` on Linux, `proc_listpids`/`proc_pidinfo` on macOS.

use crate::model::boot::BootId;
use crate::model::process::ProcessIdentity;
use std::collections::HashSet;
use std::io;
use sysinfo::{Pid, ProcessesToUpdate, System};

/// Put `cmd` in a fresh process group (`pgid == child pid`) before exec.
pub fn isolate(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

/// Detach `cmd` from our session and process group, so a background
/// supervisor survives its launcher (and a terminal Ctrl-C).
pub fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Process group id of `pid`.
pub fn pgid_of(pid: u32) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Fields after the parenthesised comm: state, ppid, pgrp.
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(2)?
            .parse()
            .ok()
    }
    #[cfg(target_os = "macos")]
    {
        use std::mem::{MaybeUninit, size_of};
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let wanted = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: exact-size out-buffer, flavor `PROC_PIDTBSDINFO`.
        let got = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                wanted,
            )
        };
        if got != wanted {
            return None;
        }
        // SAFETY: kernel wrote the full struct.
        let pgid = unsafe { info.assume_init() }.pbi_pgid;
        (pgid > 0).then_some(pgid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Every live pid that belongs to process group `pgid`.
pub fn members_of_group(pgid: u32) -> Vec<u32> {
    all_pids()
        .into_iter()
        .filter(|&pid| pgid_of(pid) == Some(pgid))
        .collect()
}

/// Identity of every live member of `pgid`, skipping processes whose start
/// time the OS will not tell us (an unidentifiable member is never signaled
/// on its own).
pub fn group_identities(pgid: u32, boot_id: &BootId) -> Vec<ProcessIdentity> {
    let pids: Vec<Pid> = members_of_group(pgid)
        .into_iter()
        .map(Pid::from_u32)
        .collect();
    if pids.is_empty() {
        return Vec::new();
    }
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&pids), false);
    pids.iter()
        .filter_map(|p| {
            let start_time = sys.process(*p)?.start_time();
            (start_time != 0).then_some(ProcessIdentity {
                boot_id: *boot_id,
                pid: p.as_u32(),
                start_time,
            })
        })
        .collect()
}

/// Live resource usage of a process group, from the OS process table.
///
/// Memory is the sum of RSS over the group's members: shared pages can be
/// counted more than once, so this is an observation, not an accounting
/// figure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupUsage {
    pub processes: usize,
    pub memory_bytes: u64,
}

pub fn group_usage(pgid: u32) -> GroupUsage {
    let pids: Vec<Pid> = members_of_group(pgid)
        .into_iter()
        .map(Pid::from_u32)
        .collect();
    if pids.is_empty() {
        return GroupUsage::default();
    }
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        false,
        sysinfo::ProcessRefreshKind::nothing().with_memory(),
    );
    let mut usage = GroupUsage::default();
    for pid in pids {
        if let Some(process) = sys.process(pid) {
            usage.processes += 1;
            usage.memory_bytes += process.memory();
        }
    }
    usage
}

/// Identity of one specific `pid`, independent of its process group.
pub fn identity_of_pid(pid: u32, boot_id: &BootId) -> Option<ProcessIdentity> {
    let pids = [Pid::from_u32(pid)];
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&pids), false);
    let start_time = sys.process(pids[0])?.start_time();
    (start_time != 0).then_some(ProcessIdentity {
        boot_id: *boot_id,
        pid,
        start_time,
    })
}

/// Does this identity still name the same live process?
pub fn identity_holds(identity: &ProcessIdentity) -> bool {
    identity_of_pid(identity.pid, &identity.boot_id)
        .map(|live| live == *identity)
        .unwrap_or(false)
}

/// Is at least one live member of `pgid` a process from `known`?
pub fn group_has_known_member(
    pgid: u32,
    boot_id: &BootId,
    known: &HashSet<ProcessIdentity>,
) -> bool {
    group_identities(pgid, boot_id)
        .iter()
        .any(|live| known.contains(live))
}

/// Signal every process in group `pgid`. A vanished group (`ESRCH`) is not an
/// error: the goal is reached.
pub fn signal_group(pgid: u32, sig: i32) -> io::Result<()> {
    if pgid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to signal process group 0",
        ));
    }
    // SAFETY: `kill` with a negative pid signals the process group.
    let rc = unsafe { libc::kill(-(pgid as libc::pid_t), sig) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err)
}

/// Does any process still belong to `pgid`?
pub fn group_alive(pgid: u32) -> bool {
    // SAFETY: signal 0 only performs the permission/existence check.
    unsafe { libc::kill(-(pgid as libc::pid_t), 0) == 0 }
}

#[cfg(target_os = "linux")]
fn all_pids() -> Vec<u32> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok()))
        .collect()
}

#[cfg(target_os = "macos")]
fn all_pids() -> Vec<u32> {
    /// `PROC_ALL_PIDS` from `<libproc.h>`; the libc crate does not expose it.
    const PROC_ALL_PIDS: u32 = 1;
    // Two passes: size the buffer, then fill it. `proc_listpids` returns the
    // bytes used, and the pid list may grow between calls — the extra bytes
    // are simply unused.
    let mut buf = vec![0i32; 4096];
    // SAFETY: `buf` is a valid writable buffer of `buf.len() * 4` bytes.
    let used = unsafe {
        libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            buf.as_mut_ptr().cast(),
            (buf.len() * std::mem::size_of::<i32>()) as libc::c_int,
        )
    };
    if used <= 0 {
        return Vec::new();
    }
    let count = used as usize / std::mem::size_of::<i32>();
    buf.truncate(count);
    buf.into_iter()
        .filter(|&p| p > 0)
        .map(|p| p as u32)
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn all_pids() -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A child in its own group reports that group as its pgid, and the group
    /// disappears once the child is gone.
    #[test]
    fn isolate_creates_own_group() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 30");
        isolate(&mut cmd);
        let child = cmd.spawn().unwrap();
        let pgid = pgid_of(child.id()).expect("pgid of live child");
        assert_eq!(pgid, child.id(), "leader's pgid is its own pid");
        assert!(members_of_group(pgid).contains(&child.id()));
        assert!(group_alive(pgid));

        signal_group(pgid, libc::SIGKILL).unwrap();
        let mut child = child;
        child.wait().unwrap();
        assert!(!group_alive(pgid), "group empty after group kill");
    }

    /// A worker that outlives its leader stays in the group, so a group kill
    /// still reaches it — this is the orphan case stage 1 must cover.
    #[test]
    fn group_kill_reaches_orphaned_worker() {
        let mut cmd = Command::new("/bin/sh");
        // The shell exits immediately; the background sleep keeps the group.
        // Its own stdio is detached so reading the leader's stdout does not
        // wait for the worker.
        cmd.arg("-c").arg("sleep 30 >/dev/null 2>&1 & echo $!");
        isolate(&mut cmd);
        let mut child = cmd.stdout(std::process::Stdio::piped()).spawn().unwrap();
        let group = child.id();
        let mut out = String::new();
        {
            use std::io::Read;
            child
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut out)
                .unwrap();
        }
        let worker: u32 = out.trim().parse().unwrap();
        child.wait().unwrap();

        // The leader is reaped; the worker remains in the same group.
        assert_eq!(pgid_of(worker), Some(group), "worker stays in run's group");
        assert!(group_alive(group));

        signal_group(group, libc::SIGKILL).unwrap();
        for _ in 0..100 {
            if !group_alive(group) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("orphaned worker survived a group kill");
    }

    /// A process that leaves the managed group (`setsid`, double fork, a
    /// daemon) is not reachable by a group kill. There is no portable
    /// `setsid(1)` on macOS, so the boundary is proven the other way round:
    /// a run in a different group is never a member of this group.
    #[test]
    fn group_kill_never_reaches_another_group() {
        let mut mine = Command::new("/bin/sh");
        mine.arg("-c").arg("sleep 30");
        isolate(&mut mine);
        let mine = mine.spawn().unwrap();

        let mut other = Command::new("/bin/sh");
        other.arg("-c").arg("sleep 30");
        isolate(&mut other);
        let other = other.spawn().unwrap();

        assert_eq!(pgid_of(other.id()), Some(other.id()));
        assert!(
            !members_of_group(mine.id()).contains(&other.id()),
            "an escaped/foreign process must not count as a group member"
        );
        assert!(
            !group_identities(mine.id(), &BootId::from_le_bytes([1; 16]))
                .iter()
                .any(|id| id.pid == other.id()),
            "escaped process must never be attributed to the run"
        );

        signal_group(mine.id(), libc::SIGKILL).unwrap();
        signal_group(other.id(), libc::SIGKILL).unwrap();
        let (mut mine, mut other) = (mine, other);
        mine.wait().unwrap();
        other.wait().unwrap();
    }
}
