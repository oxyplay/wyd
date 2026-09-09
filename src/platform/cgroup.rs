//! Linux cgroup v2 backend for managed runs.
//!
//! Runs live in their own cgroup under a **delegated** subtree. Delegation is
//! the whole point: wyd never asks for root and never rewrites systemd units
//! or global OS configuration. If no delegated subtree is available the
//! capability is reported unavailable and a hard request is refused before
//! spawn — a limit that does not exist is never promised.
//!
//! Why cgroups on top of process groups: cgroup membership is inherited by
//! every descendant, so a child that calls `setsid` or double-forks still
//! cannot leave the run's cgroup. `cgroup.kill` then reaches it. That closes
//! the escape process groups cannot cover.
//!
//! The supervisor itself is never placed in a run cgroup: it must keep
//! resources to reap and clean up after an OOM.

use std::io;
use std::path::{Path, PathBuf};

/// Mount point of the unified hierarchy.
const CGROUP2_MOUNT: &str = "/sys/fs/cgroup";

/// Controllers a run wants, in the order they are enabled.
const WANTED: [&str; 3] = ["memory", "pids", "cpu"];

/// `cpu.max` period. One core is one full period of quota.
pub const CPU_PERIOD_US: u64 = 100_000;

/// `cpu.max` quota for a request in millicores: 1000 millicores is one core,
/// i.e. one full period.
pub fn quota_for_millicores(millicores: u32) -> u64 {
    u64::from(millicores) * (CPU_PERIOD_US / 1000)
}

/// A writable delegated subtree whose children may carry limits.
#[derive(Debug, Clone)]
pub struct CgroupRoot {
    path: PathBuf,
    controllers: Vec<String>,
}

/// Limits applied to one run's cgroup. `None` leaves the controller's own
/// default (i.e. unlimited) in place.
#[derive(Debug, Clone, Default)]
pub struct CgroupLimits {
    pub memory_max_bytes: Option<u64>,
    pub memory_swap_max_bytes: Option<u64>,
    /// `cpu.max` in `$QUOTA $PERIOD` form, e.g. `200000 100000` for 2 CPUs.
    pub cpu_max: Option<(u64, u64)>,
    pub pids_max: Option<u64>,
}

/// What was actually written, so `get_run` can report effective values rather
/// than the request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppliedLimits {
    pub memory_max_bytes: Option<u64>,
    pub memory_swap_max_bytes: Option<u64>,
    pub cpu_max: Option<(u64, u64)>,
    pub pids_max: Option<u64>,
}

/// Counters from `memory.events` / `cgroup.events`. An OOM claim is only made
/// when the kernel says so — never inferred from a SIGKILL.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CgroupEvents {
    pub memory_max_hits: u64,
    pub oom: u64,
    pub oom_kill: u64,
    pub populated: bool,
}

/// One run's cgroup.
#[derive(Debug, Clone)]
pub struct Cgroup {
    path: PathBuf,
}

impl CgroupRoot {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn controllers(&self) -> &[String] {
        &self.controllers
    }

    /// Create a leaf cgroup directly under `dir` (one per run).
    pub fn create_leaf(dir: &Path, name: &str) -> io::Result<Cgroup> {
        let path = dir.join(name);
        std::fs::create_dir(&path)?;
        Ok(Cgroup { path })
    }

    /// Create a cgroup that will hold other cgroups (the aggregate one) and
    /// enable the controllers for its children.
    pub fn prepare_parent(&self, name: &str) -> io::Result<Cgroup> {
        let path = self.path.join(name);
        std::fs::create_dir(&path)?;
        let cgroup = Cgroup { path };
        cgroup.enable_controllers()?;
        Ok(cgroup)
    }
}

impl Cgroup {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create a leaf cgroup below this one (one per run).
    pub fn create_child(&self, name: &str) -> io::Result<Cgroup> {
        let path = self.path.join(name);
        std::fs::create_dir(&path)?;
        Ok(Cgroup { path })
    }

    /// Enable the controllers our runs need for this cgroup's children. Only
    /// valid while this cgroup has no processes of its own.
    pub fn enable_controllers(&self) -> io::Result<Vec<String>> {
        let available = read_words(&self.path.join("cgroup.controllers"))?;
        let missing: Vec<&str> = WANTED
            .iter()
            .copied()
            .filter(|c| available.iter().any(|a| a == c))
            .collect();
        if missing.is_empty() {
            return Ok(Vec::new());
        }
        let enable: String = missing.iter().map(|c| format!("+{c} ")).collect();
        write_str(&self.path, "cgroup.subtree_control", enable.trim())?;
        read_words(&self.path.join("cgroup.subtree_control"))
    }

    /// Current `cpu.max` quota in millicores, `None` when unlimited.
    pub fn cpu_max_millicores(&self) -> Option<u32> {
        let raw = std::fs::read_to_string(self.path.join("cpu.max")).ok()?;
        let quota: u64 = raw.split_whitespace().next()?.parse().ok()?;
        let period: u64 = raw.split_whitespace().nth(1)?.parse().ok()?;
        Some((quota.saturating_mul(1000) / period.max(1)) as u32)
    }

    /// Current `pids.max`, `None` when unlimited.
    pub fn pids_max(&self) -> Option<u32> {
        std::fs::read_to_string(self.path.join("pids.max"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Current `memory.max`, `None` when the controller is not present or the
    /// value is `max` (unlimited).
    pub fn memory_max(&self) -> Option<u64> {
        std::fs::read_to_string(self.path.join("memory.max"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Times the pids controller refused a fork in this cgroup.
    #[cfg(test)]
    pub fn pids_max_hits(&self) -> u64 {
        read_kv(&self.path.join("pids.events"))
            .ok()
            .and_then(|kv| kv.get("max").copied())
            .unwrap_or(0)
    }

    /// CPU time charged to this cgroup and its descendants, in microseconds.
    /// Only the cpu-quota test needs it so far.
    #[cfg(test)]
    pub fn cpu_usage_usec(&self) -> u64 {
        read_kv(&self.path.join("cpu.stat"))
            .ok()
            .and_then(|kv| kv.get("usage_usec").copied())
            .unwrap_or(0)
    }

    /// Write every requested limit, returning what the kernel accepted.
    /// A controller that is not enabled for this cgroup is an error, not a
    /// silent skip: the caller asked for a hard limit.
    pub fn apply(&self, limits: &CgroupLimits) -> io::Result<AppliedLimits> {
        let mut applied = AppliedLimits::default();
        if let Some(bytes) = limits.memory_max_bytes {
            write_u64(&self.path, "memory.max", bytes)?;
            applied.memory_max_bytes = Some(bytes);
        }
        if let Some(bytes) = limits.memory_swap_max_bytes {
            write_u64(&self.path, "memory.swap.max", bytes)?;
            applied.memory_swap_max_bytes = Some(bytes);
        }
        if let Some((quota, period)) = limits.cpu_max {
            write_str(&self.path, "cpu.max", &format!("{quota} {period}"))?;
            applied.cpu_max = Some((quota, period));
        }
        if let Some(max) = limits.pids_max {
            write_u64(&self.path, "pids.max", max)?;
            applied.pids_max = Some(max);
        }
        Ok(applied)
    }

    /// Path of `cgroup.procs`, for the async-signal-safe pre-exec write.
    pub fn procs_path(&self) -> PathBuf {
        self.path.join("cgroup.procs")
    }

    /// The same path as a `CStr`, ready for [`attach_self`].
    pub fn procs_cstr(&self) -> io::Result<std::ffi::CString> {
        std::ffi::CString::new(self.procs_path().into_os_string().as_encoded_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "cgroup path has a NUL"))
    }

    /// Kernel counters for this cgroup.
    pub fn events(&self) -> io::Result<CgroupEvents> {
        let memory = read_kv(&self.path.join("memory.events"))?;
        let mut events = CgroupEvents {
            memory_max_hits: memory.get("max").copied().unwrap_or(0),
            oom: memory.get("oom").copied().unwrap_or(0),
            oom_kill: memory.get("oom_kill").copied().unwrap_or(0),
            populated: true,
        };
        if let Ok(cgroup) = read_kv(&self.path.join("cgroup.events")) {
            events.populated = cgroup.get("populated").copied().unwrap_or(1) != 0;
        }
        Ok(events)
    }

    /// `cgroup.kill` (kernel 5.14+): kill every process in the subtree,
    /// including ones that left the process group.
    pub fn kill(&self) -> io::Result<()> {
        write_str(&self.path, "cgroup.kill", "1")
    }

    /// Remove the cgroup. Fails while it still has processes, which is the
    /// honest signal that cleanup is incomplete.
    pub fn remove(&self) -> io::Result<()> {
        std::fs::remove_dir(&self.path)
    }

    pub fn populated(&self) -> io::Result<bool> {
        Ok(self.events()?.populated)
    }
}

/// Move the **calling** process into a cgroup. Meant for `pre_exec`, between
/// fork and exec: the user command then starts already inside its limit, and
/// everything it forks inherits the membership — including a `setsid` child,
/// which is exactly what a process group cannot keep.
///
/// Only async-signal-safe calls: no allocation, no locks. Build the `CString`
/// in the parent, before the fork.
pub fn attach_self(procs: &std::ffi::CStr) -> io::Result<()> {
    // SAFETY: getpid/open/write/close are async-signal-safe; the buffer is
    // stack-allocated and the path was validated before the fork.
    unsafe {
        let pid = libc::getpid().max(0) as u32;
        let mut buf = [0u8; 12];
        let digits = itoa(pid, &mut buf);
        let fd = libc::open(procs.as_ptr(), libc::O_WRONLY);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let written = libc::write(fd, digits.as_ptr().cast(), digits.len());
        libc::close(fd);
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Decimal digits of `value` written to the end of `buf`; returns just those
/// digits. No allocation.
fn itoa(mut value: u32, buf: &mut [u8; 12]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    &buf[i..]
}

/// Find a delegated subtree we may create run cgroups in.
///
/// Order: an explicit `WYD_CGROUP_ROOT`, then the writable ancestors of our
/// own cgroup. A candidate qualifies only when the controllers a run needs
/// are either already enabled for its children or can be enabled without
/// moving anyone else's processes.
pub fn discover() -> Result<CgroupRoot, String> {
    if !Path::new(CGROUP2_MOUNT).join("cgroup.controllers").exists() {
        return Err("cgroup v2 is not mounted at /sys/fs/cgroup".into());
    }
    let mut tried: Vec<String> = Vec::new();
    for candidate in candidates() {
        match qualify(&candidate) {
            Ok(controllers) => {
                return Ok(CgroupRoot {
                    path: candidate,
                    controllers,
                });
            }
            Err(why) => tried.push(format!("{}: {why}", candidate.display())),
        }
    }
    Err(format!(
        "no delegated cgroup v2 subtree ({}). Run wyd under a systemd unit with \
         Delegate=yes, or point WYD_CGROUP_ROOT at a directory you own inside a \
         delegated subtree",
        tried.join("; ")
    ))
}

/// Candidate directories, most specific first.
fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(explicit) = std::env::var("WYD_CGROUP_ROOT")
        && !explicit.trim().is_empty()
    {
        out.push(PathBuf::from(explicit));
    }
    // Our own cgroup, then each ancestor up to the mount root.
    if let Some(own) = own_cgroup_path() {
        let mut current = Some(own.as_path());
        while let Some(dir) = current {
            if dir.starts_with(CGROUP2_MOUNT) {
                out.push(dir.to_path_buf());
            }
            current = dir.parent();
        }
    }
    if !out.iter().any(|p| p == Path::new(CGROUP2_MOUNT)) {
        out.push(PathBuf::from(CGROUP2_MOUNT));
    }
    out
}

/// Our own cgroup directory, from `/proc/self/cgroup` (`0::/path`).
fn own_cgroup_path() -> Option<PathBuf> {
    let content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative = content
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim()
        .trim_start_matches('/');
    Some(Path::new(CGROUP2_MOUNT).join(relative))
}

/// Can we use `dir` as the parent of run cgroups?
fn qualify(dir: &Path) -> Result<Vec<String>, String> {
    let controllers_file = dir.join("cgroup.controllers");
    let available = read_words(&controllers_file)
        .map_err(|e| format!("cannot read {}: {e}", controllers_file.display()))?;
    let mut enabled = read_words(&dir.join("cgroup.subtree_control")).unwrap_or_default();

    let missing: Vec<&str> = WANTED
        .iter()
        .copied()
        .filter(|c| !enabled.iter().any(|e| e == c))
        .collect();
    if !missing.is_empty() {
        // Enabling requires this cgroup to have no processes of its own
        // (the "no internal processes" rule). We never move anyone else's
        // processes to satisfy it.
        if !available.iter().any(|c| missing.contains(&c.as_str())) {
            return Err(format!("controllers {:?} are not available here", missing));
        }
        let cgroup = Cgroup {
            path: dir.to_path_buf(),
        };
        cgroup
            .enable_controllers()
            .map_err(|e| format!("cannot enable {:?}: {e}", missing))?;
        enabled = read_words(&dir.join("cgroup.subtree_control")).unwrap_or_default();
        let still: Vec<&str> = WANTED
            .iter()
            .copied()
            .filter(|c| !enabled.iter().any(|e| e == c))
            .collect();
        if !still.is_empty() {
            return Err(format!("controllers {:?} still not enabled", still));
        }
    }

    // A probe child proves we can actually create run cgroups here. The name
    // is unique per call: two concurrent probes would otherwise collide and
    // disqualify a perfectly good root.
    let probe = dir.join(format!(
        "wyd-probe-{}-{}",
        std::process::id(),
        PROBE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&probe).map_err(|e| format!("cannot create a child: {e}"))?;
    let _ = std::fs::remove_dir(&probe);
    Ok(enabled)
}

/// Makes concurrent probes from one process unique.
static PROBE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn read_words(path: &Path) -> io::Result<Vec<String>> {
    Ok(std::fs::read_to_string(path)?
        .split_whitespace()
        .map(str::to_string)
        .collect())
}

fn read_kv(path: &Path) -> io::Result<std::collections::HashMap<String, u64>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .filter_map(|line| {
            let (k, v) = line.split_once(' ')?;
            Some((k.to_string(), v.trim().parse().ok()?))
        })
        .collect())
}

fn write_u64(dir: &Path, file: &str, value: u64) -> io::Result<()> {
    write_str(dir, file, &value.to_string())
}

fn write_str(dir: &Path, file: &str, value: &str) -> io::Result<()> {
    std::fs::write(dir.join(file), value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The container/CI harness prepares a delegated directory and points
    /// `WYD_CGROUP_ROOT` at it; without one, discovery must say so instead of
    /// pretending a limit exists.
    #[test]
    fn discovery_is_honest_about_missing_delegation() {
        match std::env::var("WYD_CGROUP_ROOT") {
            Ok(root) if !root.trim().is_empty() => {
                let found = discover().expect("the prepared root must qualify");
                assert_eq!(found.path(), Path::new(root.trim()));
                for wanted in WANTED {
                    assert!(
                        found.controllers().iter().any(|c| c == wanted),
                        "{wanted} must be enabled for run children"
                    );
                }
            }
            _ => {
                // No prepared root: either the machine delegates (discovery
                // succeeds) or it does not (discovery explains why). It must
                // never panic.
                let _ = discover();
            }
        }
    }

    #[test]
    fn limits_attach_events_and_kill_round_trip() {
        let Ok(root) = discover() else {
            eprintln!("no delegated cgroup root; skipping the lifecycle test");
            return;
        };
        let name = format!("wyd-test-{}", std::process::id());
        let cg = CgroupRoot::create_leaf(root.path(), &name).expect("create run cgroup");
        let applied = cg
            .apply(&CgroupLimits {
                memory_max_bytes: Some(32 * 1024 * 1024),
                pids_max: Some(8),
                ..CgroupLimits::default()
            })
            .expect("apply limits");
        assert_eq!(applied.memory_max_bytes, Some(32 * 1024 * 1024));
        assert_eq!(applied.pids_max, Some(8));

        // The command starts inside the cgroup (like the real runner does, via
        // pre_exec), so a child it detaches with setsid inherits the
        // membership. Attaching after spawn would miss it: cgroup membership
        // is inherited at fork.
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("setsid sleep 30 >/dev/null 2>&1 & echo $!; sleep 30");
        let procs = cg.procs_cstr().unwrap();
        // SAFETY: `attach_self` only calls async-signal-safe functions.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || attach_self(&procs));
        }
        let mut child = cmd.stdout(std::process::Stdio::piped()).spawn().unwrap();
        {
            // One line only: the shell keeps running, so reading to EOF would
            // wait for the whole run.
            use std::io::BufRead;
            let mut out = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut out)
                .unwrap();
            let detached: u32 = out.trim().parse().unwrap();
            // Give the child a moment to fork.
            std::thread::sleep(std::time::Duration::from_millis(200));
            let members = std::fs::read_to_string(cg.procs_path()).unwrap();
            let members: Vec<u32> = members
                .split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect();
            assert!(
                members.contains(&detached),
                "setsid child {detached} must stay in the run cgroup: {members:?}"
            );
        }
        assert!(cg.events().unwrap().populated);
        cg.kill().expect("cgroup.kill");
        child.wait().unwrap();
        for _ in 0..50 {
            if !cg.populated().unwrap_or(true) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!cg.populated().unwrap(), "cgroup.kill must empty the group");
        cg.remove().expect("remove empty cgroup");
    }
    /// Spawn `script` inside `cg` (attached between fork and exec).
    fn spawn_attached(cg: &Cgroup, script: &str) -> std::process::Child {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let procs = cg.procs_cstr().unwrap();
        // SAFETY: attach_self only calls async-signal-safe functions.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || attach_self(&procs));
        }
        cmd.spawn().unwrap()
    }

    /// The aggregate parent cap must hold even when a child has no limit of
    /// its own: the budget is a kernel guarantee, not only an admission rule.
    #[test]
    fn a_parent_limit_caps_a_child_without_its_own() {
        let Ok(root) = discover() else {
            eprintln!("no delegated cgroup root; skipping the parent-limit test");
            return;
        };
        let parent = root
            .prepare_parent(&format!("wyd-parent-{}", std::process::id()))
            .expect("prepare parent");
        parent
            .apply(&CgroupLimits {
                memory_max_bytes: Some(32 * 1024 * 1024),
                memory_swap_max_bytes: Some(0),
                ..CgroupLimits::default()
            })
            .expect("cap the parent");
        assert_eq!(parent.memory_max(), Some(32 * 1024 * 1024));

        let child = parent.create_child("run").expect("child cgroup");
        assert_eq!(
            child.memory_max(),
            None,
            "the child has no limit of its own"
        );
        // tmpfs pages are charged to the cgroup, so this allocation must hit
        // the parent's cap rather than the child's absent one.
        let mut proc = spawn_attached(
            &child,
            "dd if=/dev/zero of=/dev/shm/wyd-parent-test bs=1M count=64 2>/dev/null",
        );
        let status = proc.wait().unwrap();
        assert!(
            !status.success(),
            "the parent cap must stop the child, got {status}"
        );
        assert!(
            parent.events().unwrap().oom_kill > 0,
            "the kernel must report the OOM in the parent cgroup"
        );
        child.remove().ok();
        parent.remove().ok();
    }

    /// A cpu.max quota must actually throttle: a busy loop at 0.2 core for
    /// two seconds cannot consume two seconds of CPU.
    #[test]
    fn a_cpu_quota_throttles_the_cgroup() {
        let Ok(root) = discover() else {
            eprintln!("no delegated cgroup root; skipping the cpu-quota test");
            return;
        };
        let cg = CgroupRoot::create_leaf(root.path(), &format!("wyd-cpu-{}", std::process::id()))
            .expect("create cgroup");
        cg.apply(&CgroupLimits {
            cpu_max: Some((20_000, 100_000)),
            ..CgroupLimits::default()
        })
        .expect("apply cpu.max");

        let mut proc = spawn_attached(&cg, "while :; do :; done");
        std::thread::sleep(std::time::Duration::from_secs(2));
        let used = cg.cpu_usage_usec();
        cg.kill().ok();
        proc.wait().unwrap();
        // 0.2 core for ~2 s is 0.4 CPU-seconds; allow a generous margin for
        // scheduling and for the loop not starting instantly.
        assert!(
            used < 1_200_000,
            "0.2-core quota used {used} us of CPU in ~2 s"
        );
        cg.remove().ok();
    }
    /// The aggregate caps must apply to children that have no limit of their
    /// own: CPU is throttled and forks past the pids cap are refused.
    #[test]
    fn parent_cpu_and_pids_limits_apply_to_children() {
        let Ok(root) = discover() else {
            eprintln!("no delegated cgroup root; skipping the parent cpu/pids test");
            return;
        };
        let parent = root
            .prepare_parent(&format!("wyd-parent-cpupids-{}", std::process::id()))
            .expect("prepare parent");
        parent
            .apply(&CgroupLimits {
                cpu_max: Some((20_000, 100_000)),
                pids_max: Some(8),
                ..CgroupLimits::default()
            })
            .expect("cap the parent");
        assert_eq!(parent.cpu_max_millicores(), Some(200));
        assert_eq!(parent.pids_max(), Some(8));

        let child = parent.create_child("run").expect("child cgroup");

        let mut busy = spawn_attached(&child, "while :; do :; done");
        std::thread::sleep(std::time::Duration::from_secs(2));
        let used = child.cpu_usage_usec();
        parent.kill().ok();
        busy.wait().unwrap();
        assert!(
            used < 1_200_000,
            "a child with no cpu.max of its own used {used} us in ~2 s under a 0.2-core parent"
        );

        let mut forker = spawn_attached(
            &child,
            "i=0; while [ $i -lt 200 ]; do sleep 30 & i=$((i+1)); done; wait",
        );
        std::thread::sleep(std::time::Duration::from_secs(1));
        let hits = parent.pids_max_hits();
        parent.kill().ok();
        forker.wait().unwrap();
        assert!(hits > 0, "the parent pids cap was never reached");
        child.remove().ok();
        parent.remove().ok();
    }

    #[test]
    fn millicores_map_to_a_full_period_per_core() {
        assert_eq!(quota_for_millicores(1000), CPU_PERIOD_US, "one core");
        assert_eq!(quota_for_millicores(1500), 150_000, "1.5 cores");
        assert_eq!(quota_for_millicores(200), 20_000, "0.2 cores");
    }
}
