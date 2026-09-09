//! Managed run contract: the durable, backend-agnostic description of a
//! command wyd started, its lifecycle, result and cleanup.
//!
//! A `Run` is not a `RuntimeSession`: a session is an agent invocation, a run
//! is one executed command. The link between them is optional metadata.
//!
//! States: `queued → starting → running → stopping → finished`. Before a
//! spawn the run may also go straight to `finished` (cancelled or spawn
//! failure). Stage 1 has no real queue, so `queued` is transient; it exists
//! so the stage 2 scheduler does not have to redefine the state machine.

use crate::model::process::ProcessIdentity;
use crate::model::session::RuntimeSessionId;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Timeout for one run when the caller does not ask for one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// Grace period between the soft and the forced stop.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(3);
/// Bounded log tail per run (per stream, kept bytes).
pub const DEFAULT_RUN_LOG_LIMIT: u64 = 10 * 1024 * 1024;
/// Bounded total on-disk log bytes across retained runs.
pub const DEFAULT_TOTAL_LOG_LIMIT: u64 = 250 * 1024 * 1024;
/// How long finished runs (rows and log files) are kept.
pub const RUN_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Window in which a repeated `request_id` deduplicates instead of creating
/// a second run. Outside it the key is treated as new and may be reused.
pub const REQUEST_ID_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
/// Largest accepted environment block for a run (not persisted).
pub const MAX_ENV_BYTES: usize = 256 * 1024;
/// Largest accepted request line on the local API.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Stable identifier of one managed run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RunId(pub i64);

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where the run's session link came from. A session id asserted by an agent
/// is provenance metadata, never proof of authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    /// No session link.
    None,
    /// The caller (CLI/MCP) named a session it claims to be part of.
    CallerReported,
}

/// Everything needed to start one run. Contains no secrets from the process
/// environment: env is passed to the supervisor in memory, never stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSpec {
    /// Idempotency key. The same key with an identical spec returns the
    /// existing run; the same key with a different spec is a conflict.
    pub request_id: String,
    /// Executable followed by arguments. Never implicitly run through a shell.
    pub argv: Vec<String>,
    /// Absolute working directory.
    pub cwd: PathBuf,
    /// Project root, when the caller knows it.
    #[serde(default)]
    pub project_root: Option<PathBuf>,
    /// Optional originating agent session.
    #[serde(default, with = "opt_session_hex")]
    pub session_id: Option<RuntimeSessionId>,
    #[serde(default = "SessionOrigin::default_origin")]
    pub session_origin: SessionOrigin,
    #[serde(with = "duration_secs")]
    pub timeout: Duration,
    #[serde(with = "duration_secs")]
    pub grace: Duration,
    /// Per-stream retained log bytes.
    pub log_limit_bytes: u64,
    /// Resource requirements. Empty means "whatever the supervisor allows".
    #[serde(default)]
    pub resources: ResourceRequest,
}

impl SessionOrigin {
    fn default_origin() -> Self {
        SessionOrigin::None
    }
}

impl RunSpec {
    /// A spec with default limits for `argv` in `cwd`.
    pub fn new(request_id: impl Into<String>, argv: Vec<String>, cwd: PathBuf) -> Self {
        Self {
            request_id: request_id.into(),
            argv,
            cwd,
            project_root: None,
            session_id: None,
            session_origin: SessionOrigin::None,
            timeout: DEFAULT_TIMEOUT,
            grace: DEFAULT_GRACE,
            log_limit_bytes: DEFAULT_RUN_LOG_LIMIT,
            resources: ResourceRequest::default(),
        }
    }

    /// Stable digest of everything that makes two `request_id`-keyed
    /// requests the same request. Timing and log limits are excluded on
    /// purpose: retrying a request with a longer timeout must still
    /// deduplicate, not start a second process.
    pub fn fingerprint(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.request_id.as_bytes());
        for arg in &self.argv {
            hasher.update(&[0]);
            hasher.update(arg.as_bytes());
        }
        hasher.update(self.cwd.as_os_str().as_encoded_bytes());
        if let Some(root) = &self.project_root {
            hasher.update(root.as_os_str().as_encoded_bytes());
        }
        if let Some(sid) = self.session_id {
            hasher.update(&sid.as_u64().to_le_bytes());
        }
        hasher.finalize().to_hex().to_string()
    }
}

/// Lifecycle state. Terminal state is always `Finished`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Starting,
    Running,
    Stopping,
    Finished,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Queued => "queued",
            RunState::Starting => "starting",
            RunState::Running => "running",
            RunState::Stopping => "stopping",
            RunState::Finished => "finished",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => RunState::Queued,
            "starting" => RunState::Starting,
            "running" => RunState::Running,
            "stopping" => RunState::Stopping,
            "finished" => RunState::Finished,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        self == RunState::Finished
    }
}

/// Why a run finished. Kept separate from the exit code: a test can exit 0
/// and still leave the cleanup incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The command itself exited (any code).
    Exited,
    /// The command was killed by a signal.
    Signaled,
    /// Execution timeout expired.
    TimedOut,
    /// Cancelled by a caller or Ctrl-C.
    Cancelled,
    /// The command could not be started.
    SpawnFailed,
    /// The supervisor that owned the run went away.
    SupervisorLost,
    /// A resource limit (stage 2) stopped the run.
    ResourceLimit,
}

impl RunOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            RunOutcome::Exited => "exited",
            RunOutcome::Signaled => "signaled",
            RunOutcome::TimedOut => "timed_out",
            RunOutcome::Cancelled => "cancelled",
            RunOutcome::SpawnFailed => "spawn_failed",
            RunOutcome::SupervisorLost => "supervisor_lost",
            RunOutcome::ResourceLimit => "resource_limit",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "exited" => RunOutcome::Exited,
            "signaled" => RunOutcome::Signaled,
            "timed_out" => RunOutcome::TimedOut,
            "cancelled" => RunOutcome::Cancelled,
            "spawn_failed" => RunOutcome::SpawnFailed,
            "supervisor_lost" => RunOutcome::SupervisorLost,
            "resource_limit" => RunOutcome::ResourceLimit,
            _ => return None,
        })
    }
}

/// How completely the managed process group was cleaned up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupState {
    /// Cleanup is still running (or has not started).
    Pending,
    /// Every process the backend could attribute to the run is gone.
    Complete,
    /// Processes attributable to the run may still exist.
    Incomplete,
    /// The backend cannot tell (identity data missing, supervisor lost).
    Unknown,
}

impl CleanupState {
    pub fn as_str(self) -> &'static str {
        match self {
            CleanupState::Pending => "pending",
            CleanupState::Complete => "complete",
            CleanupState::Incomplete => "incomplete",
            CleanupState::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => CleanupState::Pending,
            "complete" => CleanupState::Complete,
            "incomplete" => CleanupState::Incomplete,
            "unknown" => CleanupState::Unknown,
            _ => return None,
        })
    }
}

/// The immutable outcome of a finished run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub outcome: RunOutcome,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub started_at: Option<u64>,
    pub finished_at: u64,
    /// Wall-clock duration, for history only; deadlines use a monotonic clock.
    pub duration_ms: u64,
    pub cleanup: CleanupState,
    /// Human-readable detail for abnormal ends (spawn error, cleanup note).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Per-stream log bounds and truncation markers.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct LogState {
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// What the local backend can actually guarantee, as opposed to what the
/// caller asked for. An unavailable capability is never silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendCapabilities {
    pub queue: Capability,
    pub process_group_cleanup: Capability,
    pub aggregate_memory_limit: Capability,
    pub cpu_quota: Capability,
    pub process_limit: Capability,
    pub filesystem_isolation: Capability,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Enforced by the OS for every run.
    Available,
    /// Observed and acted on by wyd, but not a kernel guarantee. The string
    /// says what the measurement is and what it can miss.
    Monitored(String),
    /// Not available at all; a run requiring it is refused before spawn.
    Unavailable(String),
}

impl Capability {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Capability::Unavailable(reason.into())
    }
}

/// What this build can do on this platform. The memory entry is the honest
/// part: macOS gets monitoring, not a hard limit, and says so.
pub fn backend_capabilities() -> BackendCapabilities {
    let cgroup = crate::platform::cgroup_delegated();
    // The reason must name the right platform: telling a macOS user to enable
    // systemd delegation is noise. Capabilities are fixed when the supervisor
    // starts, so the Linux wording says a restart is needed.
    let no_cgroup = if cfg!(target_os = "macos") {
        "macOS has no cgroup v2: a hard aggregate limit needs a VM backend, \
         which is not implemented"
    } else {
        "no delegated cgroup v2 subtree; run wyd under a systemd unit with \
         Delegate=yes or set WYD_CGROUP_ROOT, then restart the supervisor"
    };
    BackendCapabilities {
        queue: Capability::Available,
        process_group_cleanup: Capability::Available,
        aggregate_memory_limit: memory_capability(cgroup),
        cpu_quota: if cgroup {
            Capability::Available
        } else {
            Capability::unavailable(no_cgroup)
        },
        process_limit: if cgroup {
            Capability::Available
        } else {
            Capability::unavailable(no_cgroup)
        },
        filesystem_isolation: Capability::unavailable(
            "runs share the host filesystem; TMPDIR is a convenience, not a sandbox",
        ),
    }
}

#[cfg(target_os = "macos")]
fn memory_capability(_cgroup: bool) -> Capability {
    Capability::Monitored(
        "sum of RSS over the run's process group, sampled every 200 ms: shared pages can be counted more than once and a brief peak can be missed"
            .into(),
    )
}

/// Linux: a delegated cgroup gives a real `memory.max`; without one the
/// capability is unavailable rather than merely observed.
#[cfg(not(target_os = "macos"))]
fn memory_capability(cgroup: bool) -> Capability {
    if cgroup {
        Capability::Available
    } else {
        Capability::unavailable(
            "no delegated cgroup v2 subtree; run wyd under a systemd unit with \
             Delegate=yes or set WYD_CGROUP_ROOT",
        )
    }
}

/// One finished run plus its retained log state, as read back from storage.
#[derive(Debug, Clone)]
pub struct RunRecord {
    pub id: RunId,
    pub spec: RunSpec,
    pub state: RunState,
    pub result: Option<RunResult>,
    pub logs: LogState,
    pub revision: i64,
    pub created_at: u64,
    pub leader: Option<ProcessIdentity>,
    pub supervisor: Option<String>,
    /// What the backend applied, once the run was admitted.
    pub effective: EffectiveLimits,
    /// Milliseconds spent waiting for a slot.
    pub queue_wait_ms: u64,
    /// Last resource limit event, if any.
    pub limit_event: Option<String>,
}

/// Wrapper exit codes for CLI callers, so a shell can distinguish "the
/// command failed" from "wyd stopped it". The precise reason is always in
/// JSON.
pub mod exit {
    pub const TIMED_OUT: i32 = 124;
    pub const CANCELLED: i32 = 130;
    pub const SPAWN_FAILED: i32 = 125;
    /// A resource limit stopped the run. Distinct from the others on purpose:
    /// a caller must not mistake "wyd stopped it for memory" for a spawn
    /// failure or a timeout.
    pub const RESOURCE_LIMIT: i32 = 137;
}

/// How a resource requirement is enforced. The three states are deliberately
/// distinct: "observed" must never be presented as a kernel limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    /// No requirement; the run is neither limited nor stopped for resources.
    #[default]
    None,
    /// Stop the run when observed usage crosses the threshold. Sampling can
    /// miss a brief peak, so this is a policy, not a guarantee.
    Monitored,
    /// A hard kernel-enforced limit. Refused before spawn when the backend
    /// cannot deliver it — never silently downgraded.
    Hard,
}

/// What a caller asks for. Empty means "whatever the supervisor allows".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRequest {
    /// Reserved against the memory budget at admission, and used as the
    /// monitored threshold when `enforcement` is `monitored`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_millicores: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<u32>,
    #[serde(default)]
    pub enforcement: Enforcement,
    /// How long the request may wait for a slot before it is abandoned.
    /// Separate from the execution timeout, which only starts at spawn.
    #[serde(
        default,
        with = "opt_duration_secs",
        skip_serializing_if = "Option::is_none"
    )]
    pub queue_timeout: Option<Duration>,
}

/// What the backend actually applies to a run. `requested` and `effective`
/// are both reported so a caller can see a refusal instead of guessing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EffectiveLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_millicores: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processes: Option<u32>,
    pub enforcement: Enforcement,
    /// Where the numbers come from, e.g. "sum of RSS over the run's process
    /// group, sampled every 200 ms".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    /// Backend that enforces (or observes) the limits.
    pub backend: String,
}

/// Why a run is still queued, and how long it has waited. Deliberately no
/// exact ETA: the supervisor cannot know when a slot frees.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
    pub waiting_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One queued request as `get_capacity` reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntry {
    pub run_id: String,
    pub project: String,
    pub position: usize,
    pub waiting_ms: u64,
    pub memory_bytes: u64,
    pub reason: String,
}

/// Per-project slot and reservation usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectUsage {
    pub project: String,
    pub running: usize,
    pub reserved_memory_bytes: u64,
}

/// Admission limits in force. A temporary exceedance after a config change is
/// reported, never hidden.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitSummary {
    pub max_parallel: usize,
    pub max_parallel_per_project: usize,
    pub max_queued: usize,
    pub queue_timeout_secs: u64,
    pub memory_budget_bytes: u64,
    pub default_run_memory_bytes: u64,
    pub starvation_after_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_budget_millicores: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pids_budget: Option<u32>,
}

/// Read-only view of what the supervisor can admit right now.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capacity {
    pub limits: LimitSummary,
    pub running: usize,
    pub queued: usize,
    pub slots_free: usize,
    /// Sum of reservations held by runs that have not released them. This is
    /// reserved budget, not measured RAM.
    pub reserved_memory_bytes: u64,
    pub projects: Vec<ProjectUsage>,
    pub queue: Vec<QueueEntry>,
    /// `true` when runs already admitted exceed a newly lowered limit. The
    /// current runs are not killed for it; the UI says so instead.
    pub over_parallel_limit: bool,
    /// Kernel-enforced aggregate caps shared by every run, when the backend
    /// has them. Distinct from the admission budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate_memory_max_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate_cpu_millicores: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate_pids_max: Option<u32>,
    /// How `reserved_memory_bytes` is defined, so nobody reads it as usage.
    pub reservation_note: String,
    pub capabilities: BackendCapabilities,
}

/// `Duration` as whole seconds in JSON, so the wire format stays readable.
mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

/// Optional `Duration` as whole seconds.
mod opt_duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_some(&d.as_secs()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<u64>::deserialize(d)?.map(Duration::from_secs))
    }
}

/// Session ids travel as the same 16-hex-digit string the rest of the API
/// uses, never as a raw integer.
mod opt_session_hex {
    use crate::model::session::RuntimeSessionId;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<RuntimeSessionId>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(id) => s.serialize_some(&id.to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<RuntimeSessionId>, D::Error> {
        match Option::<String>::deserialize(d)?.as_deref() {
            None | Some("") => Ok(None),
            Some(hex) => u64::from_str_radix(hex, 16)
                .map(RuntimeSessionId::from_u64)
                .map(Some)
                .map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrips_through_strings() {
        for s in [
            RunState::Queued,
            RunState::Starting,
            RunState::Running,
            RunState::Stopping,
            RunState::Finished,
        ] {
            assert_eq!(RunState::parse(s.as_str()), Some(s));
        }
        for o in [
            RunOutcome::Exited,
            RunOutcome::Signaled,
            RunOutcome::TimedOut,
            RunOutcome::Cancelled,
            RunOutcome::SpawnFailed,
            RunOutcome::SupervisorLost,
            RunOutcome::ResourceLimit,
        ] {
            assert_eq!(RunOutcome::parse(o.as_str()), Some(o));
        }
        for c in [
            CleanupState::Pending,
            CleanupState::Complete,
            CleanupState::Incomplete,
            CleanupState::Unknown,
        ] {
            assert_eq!(CleanupState::parse(c.as_str()), Some(c));
        }
        assert_eq!(RunState::parse("nope"), None);
    }

    #[test]
    fn fingerprint_ignores_timing_and_log_limits() {
        let a = RunSpec::new("k", vec!["echo".into(), "hi".into()], "/tmp".into());
        let mut b = a.clone();
        b.timeout = Duration::from_secs(1);
        b.grace = Duration::from_secs(9);
        b.log_limit_bytes = 7;
        assert_eq!(a.fingerprint(), b.fingerprint());
        b.argv.push("extra".into());
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn capabilities_never_claim_a_hard_limit_we_lack() {
        let caps = backend_capabilities();
        assert!(matches!(caps.process_group_cleanup, Capability::Available));
        assert!(matches!(caps.queue, Capability::Available));
        // No backend in this build hard-limits cpu or pids.
        assert!(matches!(caps.cpu_quota, Capability::Unavailable(_)));
        assert!(matches!(caps.process_limit, Capability::Unavailable(_)));
        assert!(matches!(
            caps.filesystem_isolation,
            Capability::Unavailable(_)
        ));
        // Memory must never be advertised as a hard limit this build cannot
        // deliver: macOS observes it, Linux has no cgroup backend yet.
        #[cfg(target_os = "macos")]
        assert!(
            matches!(caps.aggregate_memory_limit, Capability::Monitored(_)),
            "macOS memory is monitored, not enforced"
        );
        #[cfg(not(target_os = "macos"))]
        assert!(
            matches!(caps.aggregate_memory_limit, Capability::Unavailable(_)),
            "without a cgroup v2 backend, hard memory must be unavailable"
        );
    }
}
