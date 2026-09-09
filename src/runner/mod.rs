//! Managed runs: one local supervisor, one state machine, one owner of the
//! process groups wyd starts.
//!
//! Nothing here blocks the API: a per-run worker thread owns the spawn, the
//! wait, the stop sequence and the drains, and publishes every transition
//! through the run's condvar. Callers wait with a bounded `wait_ms` instead
//! of holding a request open.
//!
//! Stage 1 guarantees process-group cleanup on macOS and Linux and nothing
//! more. A command that leaves its group (`setsid`, double fork, an external
//! daemon) is reported as an escape, not silently claimed as covered.

pub mod client;
pub mod logs;

use crate::model::boot::BootId;
use crate::model::process::ProcessIdentity;
use crate::model::run::{
    BackendCapabilities, CleanupState, DEFAULT_TOTAL_LOG_LIMIT, LogState, RUN_RETENTION, RunId,
    RunOutcome, RunRecord, RunResult, RunSpec, RunState, SessionOrigin,
};
use crate::platform::BootIdentityProvider;
use crate::platform::pgroup;
use crate::store::{RunFilter, RunInsert, RunStep, RuntimeStore};
use logs::{GlobalBudget, LogChunk, LogSink, RunPaths, Stream};
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often a running run re-checks its deadline and the cancel flag.
const TICK: Duration = Duration::from_millis(100);
/// How often the group is sampled for new members, so a child that outlives
/// the leader is still identifiable before we signal it.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(200);
/// How long to wait for the group to empty after SIGKILL before declaring
/// the cleanup incomplete.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Upper bound on a single `get_run` long-poll.
pub const MAX_WAIT_MS: u64 = 60_000;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mutable run state, guarded by the slot mutex.
struct RunInner {
    state: RunState,
    cleanup: CleanupState,
    revision: i64,
    result: Option<RunResult>,
    pgid: Option<u32>,
    detail: Option<String>,
}

/// One run as the supervisor sees it: durable metadata plus the live handles.
struct RunSlot {
    id: RunId,
    spec: RunSpec,
    inner: Mutex<RunInner>,
    cv: Condvar,
    cancel: AtomicBool,
    /// Identities this run may signal: every process ever observed in its
    /// group. Nothing outside this set is ever signaled.
    known: Mutex<HashSet<ProcessIdentity>>,
    /// Shared with the drain threads, so a read reflects bytes written right
    /// up to the moment of the read.
    logs: Arc<Mutex<LogState>>,
}

impl RunSlot {
    fn is_terminal(&self) -> bool {
        self.inner.lock().state.is_terminal()
    }

    fn snapshot(&self) -> (RunState, CleanupState, i64, Option<RunResult>, LogState) {
        let inner = self.inner.lock();
        (
            inner.state,
            inner.cleanup,
            inner.revision,
            inner.result.clone(),
            *self.logs.lock(),
        )
    }

    fn remember(&self, who: ProcessIdentity) -> bool {
        self.known.lock().insert(who)
    }

    fn known(&self) -> HashSet<ProcessIdentity> {
        self.known.lock().clone()
    }
}

/// The supervisor. Cheap to share: every field is either immutable or behind
/// a lock, and the store is the single writer for run rows.
pub struct Supervisor {
    db: Mutex<RuntimeStore>,
    registry: Mutex<HashMap<RunId, Arc<RunSlot>>>,
    paths: RunPaths,
    budget: Arc<GlobalBudget>,
    boot_id: BootId,
    identity: String,
    capabilities: BackendCapabilities,
}

/// Result of `start`: the run and whether this call created it.
#[derive(Debug, Clone)]
pub struct StartResult {
    pub id: RunId,
    pub created: bool,
}

/// The API-facing view of a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunView {
    pub run_id: String,
    pub request_id: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub project_root: Option<String>,
    pub session_id: Option<String>,
    pub state: RunState,
    pub outcome: Option<RunOutcome>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub cleanup: CleanupState,
    pub revision: i64,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub duration_ms: Option<u64>,
    pub detail: Option<String>,
    pub logs: LogState,
    /// Identity of the supervisor that owns (or owned) the run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<String>,
    pub capabilities: BackendCapabilities,
    /// Lifecycle events newer than the caller's `after_revision`, when asked
    /// for. Empty on a plain read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<EventView>,
}

/// One lifecycle event as the API shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventView {
    pub revision: i64,
    pub at: u64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl RunView {
    /// Build a view straight from a durable record, for readers that have no
    /// supervisor (the TUI and the web dashboard when no daemon is running).
    /// Identical to the live path except that there are no events: an event
    /// cursor is a live-supervisor concept.
    pub fn from_record(r: &RunRecord) -> RunView {
        RunView {
            run_id: r.id.to_string(),
            request_id: r.spec.request_id.clone(),
            argv: r.spec.argv.clone(),
            cwd: r.spec.cwd.to_string_lossy().into_owned(),
            project_root: r
                .spec
                .project_root
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            session_id: r.spec.session_id.map(|s| s.to_string()),
            state: r.state,
            outcome: r.result.as_ref().map(|x| x.outcome),
            exit_code: r.result.as_ref().and_then(|x| x.exit_code),
            signal: r.result.as_ref().and_then(|x| x.signal),
            cleanup: r
                .result
                .as_ref()
                .map(|x| x.cleanup)
                .unwrap_or(CleanupState::Unknown),
            revision: r.revision,
            created_at: r.created_at,
            started_at: r.result.as_ref().and_then(|x| x.started_at),
            finished_at: r.result.as_ref().map(|x| x.finished_at),
            duration_ms: r.result.as_ref().map(|x| x.duration_ms),
            detail: r.result.as_ref().and_then(|x| x.detail.clone()),
            logs: r.logs,
            supervisor: r.supervisor.clone(),
            capabilities: BackendCapabilities::stage1(),
            events: Vec::new(),
        }
    }
}

impl Supervisor {
    /// Open the supervisor over the default state store and run root.
    pub fn open() -> io::Result<Arc<Self>> {
        let db = RuntimeStore::open(&RuntimeStore::default_path())?;
        let paths = RunPaths::new(RuntimeStore::default_path().with_file_name("runs"));
        Self::new(db, paths)
    }

    /// Open a supervisor over an explicit store and run root (tests).
    pub fn new(mut db: RuntimeStore, paths: RunPaths) -> io::Result<Arc<Self>> {
        std::fs::create_dir_all(paths.root())?;
        let epoch = crate::platform::SystemBoot.current_boot_epoch()?;
        let boot_id = db.boot_id_for_epoch(epoch, now_secs())?;
        let used = scan_log_bytes(paths.root());
        let supervisor = Arc::new(Self {
            db: Mutex::new(db),
            registry: Mutex::new(HashMap::new()),
            paths,
            budget: Arc::new(GlobalBudget::new(DEFAULT_TOTAL_LOG_LIMIT, used)),
            boot_id,
            identity: format!("{}:{}", std::process::id(), boot_id),
            capabilities: BackendCapabilities::stage1(),
        });
        supervisor.recover()?;
        supervisor.prune();
        Ok(supervisor)
    }

    pub fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Runs currently owned by this supervisor that have not finished.
    pub fn active_runs(&self) -> usize {
        self.registry
            .lock()
            .values()
            .filter(|s| !s.is_terminal())
            .count()
    }

    /// Start a run, or return the existing one for a repeated `request_id`.
    /// `env` is the caller's environment, held only in memory.
    pub fn start(
        self: &Arc<Self>,
        spec: RunSpec,
        env: Vec<(String, String)>,
    ) -> io::Result<StartResult> {
        if spec.argv.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "run needs a non-empty argv",
            ));
        }
        if !spec.cwd.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "run cwd must be absolute",
            ));
        }
        let env_bytes: usize = env.iter().map(|(k, v)| k.len() + v.len() + 2).sum();
        if env_bytes > crate::model::run::MAX_ENV_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "run environment too large",
            ));
        }

        let now = now_secs();
        let id = {
            let mut db = self.db.lock();
            match db.run_insert(&spec, &self.identity, &self.boot_id, now)? {
                RunInsert::Created(id) => id,
                RunInsert::Existing(id) => {
                    drop(db);
                    // Make sure the live slot exists (it may be a recovered run).
                    self.slot_or_load(id)?;
                    return Ok(StartResult { id, created: false });
                }
                RunInsert::Conflict(id) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "request_id {:?} already belongs to run {id} with a different command",
                            spec.request_id
                        ),
                    ));
                }
            }
        };

        let slot = Arc::new(RunSlot {
            id,
            spec: spec.clone(),
            inner: Mutex::new(RunInner {
                state: RunState::Queued,
                cleanup: CleanupState::Pending,
                revision: 0,
                result: None,
                pgid: None,
                detail: None,
            }),
            cv: Condvar::new(),
            cancel: AtomicBool::new(false),
            known: Mutex::new(HashSet::new()),
            logs: Arc::new(Mutex::new(LogState::default())),
        });
        self.registry.lock().insert(id, Arc::clone(&slot));
        self.db.lock().run_event(id, 0, "created", None, now)?;

        let sup = Arc::clone(self);
        let worker = Arc::clone(&slot);
        thread::Builder::new()
            .name(format!("wyd-run-{id}"))
            .spawn(move || sup.run_worker(worker, env))?;
        Ok(StartResult { id, created: true })
    }

    /// Current view of one run, optionally waiting for a revision newer than
    /// `after_revision` for at most `wait_ms` (capped at [`MAX_WAIT_MS`]).
    pub fn get(
        &self,
        id: RunId,
        after_revision: Option<i64>,
        wait_ms: u64,
    ) -> io::Result<Option<RunView>> {
        let slot = self.slot_or_load(id)?;
        if let (Some(after), Some(slot)) = (after_revision, &slot) {
            let mut inner = slot.inner.lock();
            if inner.revision <= after && !inner.state.is_terminal() && wait_ms > 0 {
                let deadline = Instant::now() + Duration::from_millis(wait_ms.min(MAX_WAIT_MS));
                while inner.revision <= after && !inner.state.is_terminal() {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    slot.cv.wait_for(&mut inner, left);
                }
            }
        }
        self.view(id, slot.as_ref(), after_revision)
    }

    /// Read a bounded chunk of one stream.
    pub fn output(
        &self,
        id: RunId,
        stream: Stream,
        cursor: u64,
        max_bytes: usize,
    ) -> io::Result<Option<LogChunk>> {
        if self.db.lock().run_get(id)?.is_none() {
            return Ok(None);
        }
        let mut chunk = logs::read_chunk(&self.paths.stream_file(id.0, stream), cursor, max_bytes)?;
        if let Some(slot) = self.slot_or_load(id)? {
            let logs = *slot.logs.lock();
            chunk.truncated = match stream {
                Stream::Stdout => logs.stdout_truncated,
                Stream::Stderr => logs.stderr_truncated,
            };
        } else {
            let logs = self
                .db
                .lock()
                .run_get(id)?
                .map(|r| r.logs)
                .unwrap_or_default();
            chunk.truncated = match stream {
                Stream::Stdout => logs.stdout_truncated,
                Stream::Stderr => logs.stderr_truncated,
            };
        }
        Ok(Some(chunk))
    }

    /// Runs, newest first.
    pub fn list(&self, filter: &RunFilter) -> io::Result<Vec<RunView>> {
        let records = self.db.lock().run_list(filter)?;
        Ok(records.iter().map(|r| self.view_record(r)).collect())
    }

    /// Ask a run to stop. Idempotent: repeating it only returns current
    /// status, and a run that already finished is left alone.
    pub fn cancel(&self, id: RunId) -> io::Result<Option<RunView>> {
        let slot = self.slot_or_load(id)?;
        let Some(slot) = slot else {
            return Ok(None);
        };
        if !slot.is_terminal() {
            slot.cancel.store(true, Ordering::SeqCst);
            slot.cv.notify_all();
        }
        self.view(id, Some(&slot), None)
    }

    /// Ask every live run to stop. Used by a graceful supervisor shutdown so
    /// active runs end as `cancelled` with a real cleanup report instead of
    /// being discovered later as `supervisor_lost`.
    pub fn cancel_all(&self) {
        for slot in self.registry.lock().values() {
            if !slot.is_terminal() {
                slot.cancel.store(true, Ordering::SeqCst);
                slot.cv.notify_all();
            }
        }
    }

    fn slot_or_load(&self, id: RunId) -> io::Result<Option<Arc<RunSlot>>> {
        if let Some(slot) = self.registry.lock().get(&id) {
            return Ok(Some(Arc::clone(slot)));
        }
        // Not live here: a finished run from a previous supervisor, or a run
        // this process has not adopted. Rebuild a read-only slot from storage
        // so views and output still work.
        let Some(record) = self.db.lock().run_get(id)? else {
            return Ok(None);
        };
        let slot = Arc::new(RunSlot {
            id,
            spec: record.spec.clone(),
            inner: Mutex::new(RunInner {
                state: record.state,
                cleanup: record
                    .result
                    .as_ref()
                    .map(|r| r.cleanup)
                    .unwrap_or(CleanupState::Unknown),
                revision: record.revision,
                result: record.result.clone(),
                pgid: None,
                detail: record.result.as_ref().and_then(|r| r.detail.clone()),
            }),
            cv: Condvar::new(),
            cancel: AtomicBool::new(false),
            known: Mutex::new(HashSet::new()),
            logs: Arc::new(Mutex::new(record.logs)),
        });
        Ok(Some(slot))
    }

    fn view(
        &self,
        id: RunId,
        slot: Option<&Arc<RunSlot>>,
        after_revision: Option<i64>,
    ) -> io::Result<Option<RunView>> {
        let events = match after_revision {
            Some(after) => self
                .db
                .lock()
                .run_events(id, after)?
                .into_iter()
                .map(|e| EventView {
                    revision: e.revision,
                    at: e.at,
                    kind: e.kind,
                    detail: e.detail,
                })
                .collect(),
            None => Vec::new(),
        };
        if let Some(slot) = slot {
            let (state, cleanup, revision, result, logs) = slot.snapshot();
            let record = self.db.lock().run_get(id)?;
            let created_at = record.as_ref().map(|r| r.created_at).unwrap_or(0);
            return Ok(Some(RunView {
                run_id: id.to_string(),
                request_id: slot.spec.request_id.clone(),
                argv: slot.spec.argv.clone(),
                cwd: slot.spec.cwd.to_string_lossy().into_owned(),
                project_root: slot
                    .spec
                    .project_root
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                session_id: slot.spec.session_id.map(|s| s.to_string()),
                state,
                outcome: result.as_ref().map(|r| r.outcome),
                exit_code: result.as_ref().and_then(|r| r.exit_code),
                signal: result.as_ref().and_then(|r| r.signal),
                cleanup,
                revision,
                created_at,
                started_at: result.as_ref().and_then(|r| r.started_at),
                finished_at: result.as_ref().map(|r| r.finished_at),
                duration_ms: result.as_ref().map(|r| r.duration_ms),
                detail: result
                    .as_ref()
                    .and_then(|r| r.detail.clone())
                    .or_else(|| slot.inner.lock().detail.clone()),
                logs,
                supervisor: self.db.lock().run_get(id)?.and_then(|r| r.supervisor),
                capabilities: BackendCapabilities::stage1(),
                events,
            }));
        }
        Ok(self
            .db
            .lock()
            .run_get(id)?
            .as_ref()
            .map(|r| self.view_record(r)))
    }

    fn view_record(&self, r: &RunRecord) -> RunView {
        RunView::from_record(r)
    }
    /// Finish an unfinished run left behind by a dead supervisor: signal the
    /// processes we can still prove belong to it, never guess an exit code.
    fn recover(self: &Arc<Self>) -> io::Result<()> {
        let unfinished = self.db.lock().run_unfinished()?;
        for record in unfinished {
            let same_boot = record
                .leader
                .map(|l| l.boot_id == self.boot_id)
                .unwrap_or(false);
            let known: HashSet<ProcessIdentity> = self
                .db
                .lock()
                .run_processes(record.id)?
                .into_iter()
                .map(|(identity, _)| identity)
                .collect();
            let mut signaled = 0usize;
            if same_boot {
                // Only groups with a verifiable member are ever signaled.
                let groups: HashSet<u32> = known
                    .iter()
                    .filter(|i| pgroup::identity_holds(i))
                    .map(|i| pgroup::pgid_of(i.pid).unwrap_or(i.pid))
                    .filter(|pgid| pgroup::group_has_known_member(*pgid, &self.boot_id, &known))
                    .collect();
                for pgid in &groups {
                    pgroup::signal_group(*pgid, libc::SIGTERM).ok();
                    signaled += 1;
                }
                thread::sleep(Duration::from_millis(200));
                for pgid in &groups {
                    pgroup::signal_group(*pgid, libc::SIGKILL).ok();
                }
            }
            let survivors = known.iter().filter(|i| pgroup::identity_holds(i)).count();
            let cleanup = if !same_boot {
                CleanupState::Unknown
            } else if survivors == 0 {
                CleanupState::Complete
            } else {
                CleanupState::Incomplete
            };
            let detail = if same_boot {
                format!(
                    "supervisor lost; reaped {signaled} orphaned group(s), {survivors} survivor(s)"
                )
            } else {
                "supervisor lost before the machine rebooted; processes cannot be attributed".into()
            };
            let revision = record.revision + 1;
            self.db.lock().run_finish(
                record.id,
                revision,
                &RunResult {
                    outcome: RunOutcome::SupervisorLost,
                    exit_code: None,
                    signal: None,
                    started_at: record.result.as_ref().and_then(|r| r.started_at),
                    finished_at: now_secs(),
                    duration_ms: 0,
                    cleanup,
                    detail: Some(detail),
                },
                record.logs,
            )?;
        }
        Ok(())
    }

    /// Drop run rows and log directories older than [`RUN_RETENTION`].
    fn prune(self: &Arc<Self>) {
        let before = now_secs().saturating_sub(RUN_RETENTION.as_secs());
        let Ok(ids) = self.db.lock().run_prune(before) else {
            return;
        };
        for id in ids {
            let freed = logs::dir_bytes(&self.paths.dir(id.0));
            if self.paths.remove(id.0).is_ok() {
                self.budget.release(freed);
            }
        }
    }

    /// The run worker: everything after the row exists, on its own thread.
    fn run_worker(self: Arc<Self>, slot: Arc<RunSlot>, env: Vec<(String, String)>) {
        if self.transition(
            &slot,
            RunState::Starting,
            CleanupState::Pending,
            "starting",
            None,
        ) {
            if slot.cancel.load(Ordering::SeqCst) {
                self.finish(
                    &slot,
                    RunOutcome::Cancelled,
                    None,
                    None,
                    CleanupState::Complete,
                    Some("cancelled before spawn".into()),
                    None,
                );
                return;
            }
        } else {
            return; // superseded (e.g. recovered): never spawn.
        }

        let id = slot.id;
        if let Err(e) = self.paths.prepare(id.0) {
            self.finish(
                &slot,
                RunOutcome::SpawnFailed,
                None,
                None,
                CleanupState::Complete,
                Some(format!("run directory: {e}")),
                None,
            );
            return;
        }

        let started_mono = Instant::now();
        let started_wall = SystemTime::now();
        let mut cmd = Command::new(&slot.spec.argv[0]);
        cmd.args(&slot.spec.argv[1..])
            .current_dir(&slot.spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let tmp = self.paths.tmp(id.0);
        cmd.env("TMPDIR", &tmp).env("TMP", &tmp).env("TEMP", &tmp);
        pgroup::isolate(&mut cmd);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                self.finish(
                    &slot,
                    RunOutcome::SpawnFailed,
                    None,
                    None,
                    CleanupState::Complete,
                    Some(format!("spawn failed: {e}")),
                    None,
                );
                return;
            }
        };

        let pgid = child.id();
        let leader = identity_of(child.id(), &self.boot_id);
        slot.inner.lock().pgid = Some(pgid);
        if let Some(leader) = &leader {
            slot.remember(*leader);
            self.db
                .lock()
                .run_record_process(id, leader, "leader", now_secs())
                .ok();
            self.db.lock().run_set_leader(id, leader, now_secs()).ok();
        }
        self.transition(
            &slot,
            RunState::Running,
            CleanupState::Pending,
            "running",
            None,
        );

        // Drains: the child must never block on a full pipe.
        let log_state = Arc::clone(&slot.logs);
        let run_over = Arc::new(AtomicBool::new(false));
        let out_thread = self.drain(
            id,
            Stream::Stdout,
            child.stdout.take(),
            Arc::clone(&log_state),
            slot.spec.log_limit_bytes,
            Arc::clone(&run_over),
        );
        let err_thread = self.drain(
            id,
            Stream::Stderr,
            child.stderr.take(),
            Arc::clone(&log_state),
            slot.spec.log_limit_bytes,
            Arc::clone(&run_over),
        );

        // Reap the leader on its own thread so the deadline stays responsive.
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let reaper = thread::spawn(move || {
            let status = child.wait();
            let _ = status_tx.send(status);
        });

        let mut stop_reason: Option<RunOutcome> = None;
        let mut leader_status: Option<io::Result<std::process::ExitStatus>> = None;
        let mut last_sample = Instant::now();
        loop {
            if let Ok(status) = status_rx.try_recv() {
                leader_status = Some(status);
                break;
            }
            if last_sample.elapsed() >= SAMPLE_INTERVAL {
                last_sample = Instant::now();
                self.sample_group(&slot, pgid);
            }
            if slot.cancel.load(Ordering::SeqCst) {
                stop_reason = Some(RunOutcome::Cancelled);
                break;
            }
            if elapsed(&started_mono, &started_wall) >= slot.spec.timeout {
                stop_reason = Some(RunOutcome::TimedOut);
                break;
            }
            thread::sleep(TICK);
        }

        let mut cleanup = CleanupState::Complete;
        if stop_reason.is_some() {
            // Timeout or cancel: run the one stop algorithm, then reap.
            cleanup = self.stop_group(&slot, pgid, stop_reason == Some(RunOutcome::TimedOut));
            if leader_status.is_none() {
                leader_status = status_rx.recv_timeout(Duration::from_secs(5)).ok();
            }
        } else if pgroup::group_alive(pgid) {
            // The command exited but left processes behind in its group.
            cleanup = self.stop_group(&slot, pgid, false);
        }
        reaper.join().ok();

        let outcome = match (stop_reason, &leader_status) {
            (Some(reason), _) => reason,
            (None, Some(Ok(status))) => exit_outcome(*status),
            (None, _) => RunOutcome::Signaled,
        };
        let (exit_code, signal) = match &leader_status {
            Some(Ok(status)) => (status.code(), signal_of(status)),
            _ => (None, None),
        };

        // A descendant that escaped the group can hold our pipe open past
        // the run's death. Stop storing, then wait only briefly for the
        // drains; the run's own result must never depend on a stranger's
        // pipe.
        run_over.store(true, Ordering::SeqCst);
        for handle in [out_thread, err_thread] {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
        }
        let logs = *log_state.lock();

        let detail = leader_status
            .as_ref()
            .and_then(|s| s.as_ref().ok())
            .and_then(status_detail)
            .or_else(|| slot.inner.lock().detail.clone());
        self.finish(
            &slot,
            outcome,
            exit_code,
            signal,
            cleanup,
            detail,
            Some((started_wall, logs)),
        );
    }

    /// Publish a transition in memory and durably. Only the run's worker
    /// thread calls this, so per-run ordering is guaranteed.
    fn transition(
        &self,
        slot: &Arc<RunSlot>,
        state: RunState,
        cleanup: CleanupState,
        kind: &str,
        detail: Option<String>,
    ) -> bool {
        let revision = {
            let mut inner = slot.inner.lock();
            if inner.state.is_terminal() {
                return false;
            }
            inner.revision += 1;
            inner.state = state;
            inner.cleanup = cleanup;
            if let Some(d) = &detail {
                inner.detail = Some(d.clone());
            }
            inner.revision
        };
        slot.cv.notify_all();
        self.db
            .lock()
            .run_transition(
                slot.id,
                revision,
                RunStep {
                    state,
                    cleanup,
                    kind,
                    detail: detail.as_deref(),
                },
                now_secs(),
            )
            .unwrap_or(false)
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        slot: &Arc<RunSlot>,
        outcome: RunOutcome,
        exit_code: Option<i32>,
        signal: Option<i32>,
        cleanup: CleanupState,
        detail: Option<String>,
        timing: Option<(SystemTime, LogState)>,
    ) {
        let (started_at, duration_ms, logs) = match timing {
            Some((start, logs)) => (
                Some(
                    start
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                ),
                start.elapsed().map(|d| d.as_millis() as u64).unwrap_or(0),
                logs,
            ),
            None => (None, 0, *slot.logs.lock()),
        };
        let result = RunResult {
            outcome,
            exit_code,
            signal,
            started_at,
            finished_at: now_secs(),
            duration_ms,
            cleanup,
            detail,
        };
        let revision = {
            let mut inner = slot.inner.lock();
            if inner.state.is_terminal() {
                return;
            }
            inner.revision += 1;
            inner.state = RunState::Finished;
            inner.cleanup = cleanup;
            inner.result = Some(result.clone());
            inner.revision
        };
        slot.cv.notify_all();
        let _ = self.db.lock().run_finish(slot.id, revision, &result, logs);
        self.registry.lock().remove(&slot.id);
    }

    /// Record every live member of the group, so a child that outlives the
    /// leader is still attributable when we stop it.
    fn sample_group(&self, slot: &Arc<RunSlot>, pgid: u32) {
        let fresh = pgroup::group_identities(pgid, &self.boot_id);
        for identity in fresh {
            if slot.remember(identity) {
                let role = if identity.pid == pgid {
                    "leader"
                } else {
                    "member"
                };
                self.db
                    .lock()
                    .run_record_process(slot.id, &identity, role, now_secs())
                    .ok();
            }
        }
    }

    /// The one stop algorithm used by timeout, cancel and group-exit: mark
    /// stopping, SIGTERM the group, escalate after the grace period, then
    /// report how much of the group is really gone.
    fn stop_group(&self, slot: &Arc<RunSlot>, pgid: u32, forced: bool) -> CleanupState {
        let reason = if forced { "timeout" } else { "stop" };
        self.transition(
            slot,
            RunState::Stopping,
            CleanupState::Pending,
            reason,
            None,
        );
        self.sample_group(slot, pgid);

        // Never signal a group we can no longer attribute to this run.
        if !self.group_is_ours(slot, pgid) {
            return CleanupState::Unknown;
        }
        pgroup::signal_group(pgid, libc::SIGTERM).ok();

        let grace = slot.spec.grace.max(Duration::from_millis(50));
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if !pgroup::group_alive(pgid) {
                return CleanupState::Complete;
            }
            thread::sleep(Duration::from_millis(20));
        }

        // Still there: escalate, but only while a member we recorded is
        // alive. If nothing verifiable remains, this pgid may have been
        // reused and we must not signal it.
        if !self.group_is_ours(slot, pgid) {
            return CleanupState::Unknown;
        }
        pgroup::signal_group(pgid, libc::SIGKILL).ok();
        let deadline = Instant::now() + KILL_GRACE;
        while Instant::now() < deadline {
            if !pgroup::group_alive(pgid) {
                return CleanupState::Complete;
            }
            thread::sleep(Duration::from_millis(20));
        }
        CleanupState::Incomplete
    }

    /// `true` when at least one live group member is a process this run
    /// actually started (boot + pid + start time still match).
    fn group_is_ours(&self, slot: &Arc<RunSlot>, pgid: u32) -> bool {
        let known = slot.known();
        if known.is_empty() {
            return false;
        }
        pgroup::group_identities(pgid, &self.boot_id)
            .iter()
            .any(|live| known.contains(live))
    }

    fn drain(
        &self,
        id: RunId,
        stream: Stream,
        pipe: Option<impl io::Read + Send + 'static>,
        state: Arc<Mutex<LogState>>,
        limit: u64,
        run_over: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        let path = self.paths.stream_file(id.0, stream);
        let budget = Arc::clone(&self.budget);
        thread::spawn(move || {
            let Some(mut pipe) = pipe else {
                return;
            };
            let Ok(mut sink) = LogSink::open(&path, stream, limit, state, budget) else {
                // A log file we cannot open must not stall the child: keep
                // draining and drop the bytes.
                let mut buf = [0u8; 8192];
                while matches!(pipe.read(&mut buf), Ok(n) if n > 0) {}
                return;
            };
            let mut buf = [0u8; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if run_over.load(Ordering::SeqCst) {
                            continue; // run finished; drain but store nothing
                        }
                        if sink.write_all_bounded(&buf[..n]).is_err() {
                            // Disk full: keep draining, stop storing.
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            sink.finish().ok();
        })
    }
}

/// A deadline that also survives suspend: a machine that slept through its
/// timeout is overdue on the wall clock even though the monotonic clock
/// paused. A forward wall-clock jump can only make the stop earlier, never
/// later, and the run is reported as `timed_out` either way.
fn elapsed(mono: &Instant, wall: &SystemTime) -> Duration {
    mono.elapsed().max(wall.elapsed().unwrap_or_default())
}

fn exit_outcome(status: std::process::ExitStatus) -> RunOutcome {
    use std::os::unix::process::ExitStatusExt;
    if status.signal().is_some() {
        RunOutcome::Signaled
    } else {
        RunOutcome::Exited
    }
}

fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

fn status_detail(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => Some(format!("exit code {code}")),
        (None, Some(sig)) => Some(format!("terminated by signal {sig}")),
        _ => None,
    }
}

/// Live identity of `pid`, or `None` when the OS will not tell us its start
/// time (in which case it is never signaled on its own). A just-spawned
/// process can take a moment to appear in the process table.
fn identity_of(pid: u32, boot_id: &BootId) -> Option<ProcessIdentity> {
    for _ in 0..10 {
        if let Some(found) = pgroup::identity_of_pid(pid, boot_id) {
            return Some(found);
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Bytes already retained under the run root, so the global log cap survives
/// a supervisor restart.
fn scan_log_bytes(root: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            if entry.path().is_dir() {
                logs::dir_bytes(&entry.path())
            } else {
                entry.metadata().map(|m| m.len()).unwrap_or(0)
            }
        })
        .sum()
}

/// Session provenance for a run started by a caller that named a session.
pub fn caller_session(session: Option<crate::model::session::RuntimeSessionId>) -> SessionOrigin {
    match session {
        Some(_) => SessionOrigin::CallerReported,
        None => SessionOrigin::None,
    }
}

/// An in-memory supervisor rooted at `root`, for tests.
#[cfg(test)]
pub fn for_tests(root: &std::path::Path) -> io::Result<Arc<Supervisor>> {
    let db = RuntimeStore::open_in_memory()?;
    Supervisor::new(db, RunPaths::new(root.join("runs")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::SystemBoot;

    fn sup(name: &str) -> Arc<Supervisor> {
        let root = std::env::temp_dir().join(format!(
            "wyd-runner-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for_tests(&root).unwrap()
    }

    fn spec(name: &str, script: &str) -> RunSpec {
        let mut spec = RunSpec::new(
            format!("req-{name}"),
            vec!["/bin/sh".into(), "-c".into(), script.into()],
            std::env::temp_dir(),
        );
        spec.timeout = Duration::from_secs(10);
        spec.grace = Duration::from_millis(300);
        spec
    }

    fn wait(sup: &Arc<Supervisor>, id: RunId) -> RunView {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let view = sup.get(id, None, 0).unwrap().expect("run exists");
            if view.state.is_terminal() {
                return view;
            }
            assert!(Instant::now() < deadline, "run never finished: {view:?}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn text(sup: &Arc<Supervisor>, id: RunId, stream: Stream) -> String {
        sup.output(id, stream, 0, 1 << 20).unwrap().unwrap().data
    }

    #[test]
    fn captures_exit_code_and_both_streams() {
        let sup = sup("streams");
        let started = sup
            .start(
                spec("streams", "echo out; echo err >&2; exit 3"),
                Vec::new(),
            )
            .unwrap();
        let view = wait(&sup, started.id);
        assert_eq!(view.outcome, Some(RunOutcome::Exited));
        assert_eq!(view.exit_code, Some(3));
        assert_eq!(view.cleanup, CleanupState::Complete);
        assert_eq!(text(&sup, started.id, Stream::Stdout).trim(), "out");
        assert_eq!(text(&sup, started.id, Stream::Stderr).trim(), "err");
    }

    #[test]
    fn timeout_escalates_past_ignored_term() {
        let sup = sup("timeout");
        let mut spec = spec("timeout", "trap '' TERM; sleep 30");
        spec.timeout = Duration::from_millis(200);
        spec.grace = Duration::from_millis(200);
        let started = sup.start(spec, Vec::new()).unwrap();
        let view = wait(&sup, started.id);
        assert_eq!(view.outcome, Some(RunOutcome::TimedOut));
        assert_eq!(view.cleanup, CleanupState::Complete);
    }

    #[test]
    fn cancel_stops_a_running_command() {
        let sup = sup("cancel");
        let started = sup.start(spec("cancel", "sleep 30"), Vec::new()).unwrap();
        // Wait until it is really running before cancelling.
        let deadline = Instant::now() + Duration::from_secs(5);
        while sup.get(started.id, None, 0).unwrap().unwrap().state != RunState::Running {
            assert!(Instant::now() < deadline, "run never reached running");
            thread::sleep(Duration::from_millis(10));
        }
        sup.cancel(started.id).unwrap();
        let view = wait(&sup, started.id);
        assert_eq!(view.outcome, Some(RunOutcome::Cancelled));
        assert_eq!(view.cleanup, CleanupState::Complete);
        // Cancelling again is safe and reports the finished run.
        let again = sup.cancel(started.id).unwrap().unwrap();
        assert_eq!(again.outcome, Some(RunOutcome::Cancelled));
    }

    #[test]
    fn worker_that_outlives_its_leader_is_cleaned_up() {
        let sup = sup("orphan");
        // The shell exits immediately; the background sleep stays in the
        // run's process group.
        let started = sup
            .start(spec("orphan", "sleep 30 & echo $!; exit 0"), Vec::new())
            .unwrap();
        let view = wait(&sup, started.id);
        assert_eq!(view.outcome, Some(RunOutcome::Exited));
        assert_eq!(view.cleanup, CleanupState::Complete);
        let worker: u32 = text(&sup, started.id, Stream::Stdout)
            .trim()
            .parse()
            .unwrap();
        for _ in 0..100 {
            if pgroup::identity_of_pid(worker, &sup.boot_id).is_none() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("worker {worker} survived the run's cleanup");
    }

    #[test]
    fn repeated_request_id_does_not_start_a_second_process() {
        let sup = sup("dedupe");
        let first = sup.start(spec("dedupe", "echo once"), Vec::new()).unwrap();
        let second = sup.start(spec("dedupe", "echo once"), Vec::new()).unwrap();
        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.id, second.id);
        // A different command under the same key is a conflict, not a second run.
        let conflict = sup.start(spec("dedupe", "echo different"), Vec::new());
        assert_eq!(conflict.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn output_beyond_the_limit_is_dropped_and_marked() {
        let sup = sup("bounded");
        let mut spec = spec("bounded", "printf '0123456789abcdefghij'");
        spec.log_limit_bytes = 10;
        let started = sup.start(spec, Vec::new()).unwrap();
        let view = wait(&sup, started.id);
        assert!(view.logs.stdout_truncated, "truncation must be reported");
        assert_eq!(view.logs.stdout_bytes, 10);
        assert_eq!(text(&sup, started.id, Stream::Stdout), "0123456789");
    }

    #[test]
    fn spawn_failure_is_reported_without_guessing_an_exit_code() {
        let sup = sup("spawnfail");
        let mut spec = spec("spawnfail", "true");
        spec.argv = vec!["/nonexistent/wyd-no-such-binary".into()];
        let started = sup.start(spec, Vec::new()).unwrap();
        let view = wait(&sup, started.id);
        assert_eq!(view.outcome, Some(RunOutcome::SpawnFailed));
        assert_eq!(view.exit_code, None);
        assert_eq!(view.cleanup, CleanupState::Complete);
    }

    #[test]
    fn get_run_reports_events_after_a_revision() {
        let sup = sup("events");
        let started = sup.start(spec("events", "exit 0"), Vec::new()).unwrap();
        wait(&sup, started.id);
        let view = sup.get(started.id, Some(0), 0).unwrap().unwrap();
        let kinds: Vec<&str> = view.events.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"starting"), "events: {kinds:?}");
        assert!(kinds.contains(&"running"), "events: {kinds:?}");
        assert!(kinds.contains(&"finished"), "events: {kinds:?}");
    }
    #[test]
    fn recovery_reaps_orphans_from_a_dead_supervisor() {
        let root = std::env::temp_dir().join(format!(
            "wyd-recovery-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        let mut db = RuntimeStore::open_in_memory().unwrap();
        let boot = db
            .boot_id_for_epoch(SystemBoot.current_boot_epoch().unwrap(), now_secs())
            .unwrap();

        // A worker left behind by a supervisor that died: it is in its own
        // process group, and its shell parent is already gone.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 30 >/dev/null 2>&1 & echo $!");
        pgroup::isolate(&mut cmd);
        let mut child = cmd.stdout(Stdio::piped()).spawn().unwrap();
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
        child.wait().unwrap();
        let worker: u32 = out.trim().parse().unwrap();
        let identity = identity_of(worker, &boot).expect("live worker identity");

        // The unfinished run the dead supervisor left in the store.
        let spec = spec("recovery", "sleep 30");
        let RunInsert::Created(id) = db.run_insert(&spec, "dead", &boot, now_secs()).unwrap()
        else {
            panic!("expected a new run");
        };
        db.run_set_leader(id, &identity, now_secs()).unwrap();
        db.run_record_process(id, &identity, "leader", now_secs())
            .unwrap();
        db.run_transition(
            id,
            1,
            RunStep {
                state: RunState::Running,
                cleanup: CleanupState::Pending,
                kind: "running",
                detail: None,
            },
            now_secs(),
        )
        .unwrap();

        // Opening a new supervisor recovers the run and reaps the orphan.
        let sup = Supervisor::new(db, RunPaths::new(root.join("runs"))).unwrap();
        let view = sup.get(id, None, 0).unwrap().unwrap();
        assert_eq!(view.state, RunState::Finished);
        assert_eq!(view.outcome, Some(RunOutcome::SupervisorLost));
        assert_eq!(view.exit_code, None, "no exit code may be invented");
        assert_eq!(view.cleanup, CleanupState::Complete);
        for _ in 0..100 {
            if pgroup::identity_of_pid(worker, &boot).is_none() {
                let _ = std::fs::remove_dir_all(&root);
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("orphaned worker {worker} survived recovery");
    }
    #[test]
    fn cancel_all_stops_every_live_run() {
        let sup = sup("cancel-all");
        let first = sup
            .start(spec("cancel-all-1", "sleep 30"), Vec::new())
            .unwrap();
        let second = sup
            .start(spec("cancel-all-2", "sleep 30"), Vec::new())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while [first.id, second.id]
            .iter()
            .any(|id| sup.get(*id, None, 0).unwrap().unwrap().state != RunState::Running)
        {
            assert!(Instant::now() < deadline, "runs never reached running");
            thread::sleep(Duration::from_millis(10));
        }
        sup.cancel_all();
        for id in [first.id, second.id] {
            let view = wait(&sup, id);
            assert_eq!(view.outcome, Some(RunOutcome::Cancelled));
            assert_eq!(view.cleanup, CleanupState::Complete);
        }
    }
}
