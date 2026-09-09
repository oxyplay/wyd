//! Admission control: slots, memory reservations and a bounded, fair queue.
//!
//! Pure state machine — no OS calls and no clock of its own. Callers pass a
//! **logical** time (elapsed since the supervisor started), which the
//! supervisor computes as `max(monotonic, wall)` so a deadline still expires
//! after the machine sleeps. Using a raw `Instant` here would silently pause
//! every queue deadline during suspend.
//!
//! Ordering is FIFO inside a project and round-robin between projects, with
//! one exception: a request that has waited past `starvation_after` is
//! admitted ahead of later short runs, so a big job cannot be bypassed
//! forever by a stream of small ones.

use crate::model::run::{LimitSummary, ProjectUsage, QueueEntry, RunId};
use std::collections::VecDeque;
use std::time::Duration;

/// Capacity rules in force. Changes affect admission of new runs; a run
/// already admitted keeps its slot until it finishes.
#[derive(Debug, Clone)]
pub struct Limits {
    pub max_parallel: usize,
    pub max_parallel_per_project: usize,
    pub max_queued: usize,
    pub queue_timeout: Duration,
    /// Total memory that may be reserved by running runs.
    pub memory_budget_bytes: u64,
    /// Reservation used when a caller asks for no particular amount.
    pub default_run_memory_bytes: u64,
    /// How long a request may be bypassed before it wins the next slot.
    pub starvation_after: Duration,
    /// Aggregate CPU cap for all runs, in millicores. Admission does not use
    /// it; the cgroup backend turns it into a kernel limit.
    pub cpu_budget_millicores: Option<u32>,
    /// Aggregate process-count cap for all runs.
    pub pids_budget: Option<u32>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_parallel: 4,
            max_parallel_per_project: 2,
            max_queued: 32,
            queue_timeout: Duration::from_secs(600),
            // Reservations are a budgeting device, not a memory measurement.
            memory_budget_bytes: 8 * 1024 * 1024 * 1024,
            default_run_memory_bytes: 512 * 1024 * 1024,
            starvation_after: Duration::from_secs(60),
            cpu_budget_millicores: None,
            pids_budget: None,
        }
    }
}

impl Limits {
    pub fn summary(&self) -> LimitSummary {
        LimitSummary {
            max_parallel: self.max_parallel,
            max_parallel_per_project: self.max_parallel_per_project,
            max_queued: self.max_queued,
            queue_timeout_secs: self.queue_timeout.as_secs(),
            memory_budget_bytes: self.memory_budget_bytes,
            default_run_memory_bytes: self.default_run_memory_bytes,
            starvation_after_secs: self.starvation_after.as_secs(),
            cpu_budget_millicores: self.cpu_budget_millicores,
            pids_budget: self.pids_budget,
        }
    }
}

/// Why a request could not even be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Larger than the whole budget: queueing it would be a lie.
    Impossible { requested: u64, budget: u64 },
    /// The queue is full.
    QueueFull { max_queued: usize },
}

/// The admission decision for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Capacity reserved; the caller may spawn.
    Start {
        memory_bytes: u64,
    },
    /// Waiting for capacity. `position` is 1-based.
    Queued {
        position: usize,
    },
    Rejected(RejectReason),
}

struct Queued {
    id: RunId,
    project: String,
    memory_bytes: u64,
    /// Logical time when the request entered the queue.
    enqueued_at: Duration,
    /// Logical time after which the request gives up.
    deadline: Duration,
}

struct Running {
    id: RunId,
    project: String,
    memory_bytes: u64,
}

/// Slots and reservations. One instance per supervisor, behind its lock.
pub struct Scheduler {
    limits: Limits,
    running: Vec<Running>,
    queue: VecDeque<Queued>,
    /// Reservations kept after a run finished with an incomplete cleanup:
    /// the processes may still be alive, so the capacity stays spent until a
    /// supervisor restart re-evaluates it.
    held: Vec<Running>,
}

impl Scheduler {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            running: Vec::new(),
            queue: VecDeque::new(),
            held: Vec::new(),
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Replace the limits. Runs already admitted keep their slots and are not
    /// touched; the new numbers govern admission from here on. A temporary
    /// exceedance (more running than the new cap) is reported, not hidden.
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// Admit, queue or reject one request. `memory_bytes` is already resolved
    /// against the default by the caller.
    pub fn submit(
        &mut self,
        id: RunId,
        project: &str,
        memory_bytes: u64,
        queue_timeout: Option<Duration>,
        now: Duration,
    ) -> Admission {
        if memory_bytes > self.limits.memory_budget_bytes {
            return Admission::Rejected(RejectReason::Impossible {
                requested: memory_bytes,
                budget: self.limits.memory_budget_bytes,
            });
        }
        if self.can_admit(project, memory_bytes) {
            self.running.push(Running {
                id,
                project: project.to_string(),
                memory_bytes,
            });
            return Admission::Start { memory_bytes };
        }
        if self.queue.len() >= self.limits.max_queued {
            return Admission::Rejected(RejectReason::QueueFull {
                max_queued: self.limits.max_queued,
            });
        }
        self.queue.push_back(Queued {
            id,
            project: project.to_string(),
            memory_bytes,
            enqueued_at: now,
            deadline: now + queue_timeout.unwrap_or(self.limits.queue_timeout),
        });
        Admission::Queued {
            position: self.queue.len(),
        }
    }

    /// Release a slot, optionally holding the reservation because the
    /// cleanup was not complete. Returns the ids admitted as a result, in the
    /// order they were admitted; the caller starts them.
    pub fn release(&mut self, id: RunId, hold: bool, now: Duration) -> Vec<RunId> {
        let held = if hold {
            self.running.iter().find(|r| r.id == id).map(|r| Running {
                id: r.id,
                project: r.project.clone(),
                memory_bytes: r.memory_bytes,
            })
        } else {
            None
        };
        let before = self.running.len();
        self.running.retain(|r| r.id != id);
        if let Some(held) = held {
            self.held.push(held);
        }
        if self.running.len() == before {
            return Vec::new();
        }
        let mut admitted = Vec::new();
        while let Some(next) = self.pick_next(now) {
            admitted.push(next);
        }
        admitted
    }

    /// Drop a request that never started. `true` when it was in the queue.
    pub fn cancel_queued(&mut self, id: RunId) -> bool {
        let before = self.queue.len();
        self.queue.retain(|q| q.id != id);
        self.queue.len() != before
    }

    /// Queued requests whose deadline has passed, removed from the queue.
    pub fn expire(&mut self, now: Duration) -> Vec<RunId> {
        let mut expired = Vec::new();
        self.queue.retain(|q| {
            if now >= q.deadline {
                expired.push(q.id);
                false
            } else {
                true
            }
        });
        expired
    }

    /// 1-based queue position, if the request is waiting.
    pub fn position(&self, id: RunId) -> Option<usize> {
        self.queue.iter().position(|q| q.id == id).map(|i| i + 1)
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    pub fn queued_count(&self) -> usize {
        self.queue.len()
    }

    pub fn slots_free(&self) -> usize {
        self.limits.max_parallel.saturating_sub(self.running.len())
    }

    /// Reserved memory held by running runs plus reservations still held
    /// after an incomplete cleanup.
    pub fn reserved(&self) -> u64 {
        self.running
            .iter()
            .chain(self.held.iter())
            .map(|r| r.memory_bytes)
            .sum()
    }

    /// Reservations held for runs whose cleanup was not complete.
    pub fn held_reservations(&self) -> usize {
        self.held.len()
    }

    pub fn projects(&self) -> Vec<ProjectUsage> {
        let mut out: Vec<ProjectUsage> = Vec::new();
        for run in &self.running {
            match out.iter_mut().find(|p| p.project == run.project) {
                Some(p) => {
                    p.running += 1;
                    p.reserved_memory_bytes += run.memory_bytes;
                }
                None => out.push(ProjectUsage {
                    project: run.project.clone(),
                    running: 1,
                    reserved_memory_bytes: run.memory_bytes,
                }),
            }
        }
        out.sort_by(|a, b| a.project.cmp(&b.project));
        out
    }

    /// The queue as reported to clients, with a reason but no invented ETA.
    pub fn queue_entries(&self, now: Duration) -> Vec<QueueEntry> {
        self.queue
            .iter()
            .enumerate()
            .map(|(i, q)| QueueEntry {
                run_id: q.id.to_string(),
                project: q.project.clone(),
                position: i + 1,
                waiting_ms: now.saturating_sub(q.enqueued_at).as_millis() as u64,
                memory_bytes: q.memory_bytes,
                reason: if self.project_slots_free(&q.project) == 0 {
                    format!(
                        "project {} is at its {}-run limit",
                        q.project, self.limits.max_parallel_per_project
                    )
                } else if self.slots_free() == 0 {
                    format!("all {} slots are busy", self.limits.max_parallel)
                } else {
                    "waiting for a memory reservation".to_string()
                },
            })
            .collect()
    }

    fn can_admit(&self, project: &str, memory_bytes: u64) -> bool {
        self.slots_free() > 0
            && self.project_slots_free(project) > 0
            && self.reserved() + memory_bytes <= self.limits.memory_budget_bytes
    }

    fn project_slots_free(&self, project: &str) -> usize {
        self.limits
            .max_parallel_per_project
            .saturating_sub(self.running.iter().filter(|r| r.project == project).count())
    }

    /// Pick and admit one queued request that fits, or `None`.
    ///
    /// A starved request wins first. Otherwise a project with no running run
    /// is preferred, so one busy project cannot monopolize the queue; within
    /// that choice the oldest request goes first.
    fn pick_next(&mut self, now: Duration) -> Option<RunId> {
        let index = self.candidate_index(now)?;
        let next = self.queue.remove(index)?;
        self.running.push(Running {
            id: next.id,
            project: next.project,
            memory_bytes: next.memory_bytes,
        });
        Some(next.id)
    }

    fn candidate_index(&self, now: Duration) -> Option<usize> {
        if self.slots_free() == 0 {
            return None;
        }
        let fits = |q: &Queued| {
            self.project_slots_free(&q.project) > 0
                && self.reserved() + q.memory_bytes <= self.limits.memory_budget_bytes
        };
        let starved = self.queue.iter().enumerate().find(|(_, q)| {
            fits(q) && now.saturating_sub(q.enqueued_at) >= self.limits.starvation_after
        });
        if let Some((i, _)) = starved {
            return Some(i);
        }
        // Round-robin: prefer a project that is not running right now.
        let idle_project = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, q)| {
                fits(q)
                    && self.project_slots_free(&q.project) == self.limits.max_parallel_per_project
            })
            .min_by_key(|(_, q)| q.enqueued_at);
        if let Some((i, _)) = idle_project {
            return Some(i);
        }
        self.queue
            .iter()
            .enumerate()
            .filter(|(_, q)| fits(q))
            .min_by_key(|(_, q)| q.enqueued_at)
            .map(|(i, _)| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            max_parallel: 2,
            max_parallel_per_project: 1,
            max_queued: 3,
            queue_timeout: Duration::from_secs(60),
            memory_budget_bytes: 1000,
            default_run_memory_bytes: 100,
            starvation_after: Duration::from_secs(30),
            cpu_budget_millicores: None,
            pids_budget: None,
        }
    }

    fn sched() -> (Scheduler, Duration) {
        (Scheduler::new(limits()), Duration::ZERO)
    }

    #[test]
    fn parallel_and_per_project_caps_are_respected() {
        let (mut s, t) = sched();
        assert!(matches!(
            s.submit(RunId(1), "a", 100, None, t),
            Admission::Start { .. }
        ));
        // Same project, its own cap is 1.
        assert_eq!(
            s.submit(RunId(2), "a", 100, None, t),
            Admission::Queued { position: 1 }
        );
        // Other project gets the second global slot.
        assert!(matches!(
            s.submit(RunId(3), "b", 100, None, t),
            Admission::Start { .. }
        ));
        // Global cap reached.
        assert_eq!(
            s.submit(RunId(4), "c", 100, None, t),
            Admission::Queued { position: 2 }
        );
        assert_eq!(s.running_count(), 2);
        assert_eq!(s.slots_free(), 0);
    }

    #[test]
    fn request_larger_than_the_budget_is_rejected_not_queued() {
        let (mut s, t) = sched();
        assert_eq!(
            s.submit(RunId(1), "a", 5000, None, t),
            Admission::Rejected(RejectReason::Impossible {
                requested: 5000,
                budget: 1000
            })
        );
        assert_eq!(s.queued_count(), 0, "an impossible request must not queue");
    }

    #[test]
    fn queue_is_bounded() {
        let (mut s, t) = sched();
        assert!(matches!(
            s.submit(RunId(1), "a", 100, None, t),
            Admission::Start { .. }
        ));
        assert!(matches!(
            s.submit(RunId(2), "b", 100, None, t),
            Admission::Start { .. }
        ));
        for i in 0..3 {
            assert!(matches!(
                s.submit(RunId(10 + i), "q", 100, None, t),
                Admission::Queued { .. }
            ));
        }
        assert_eq!(
            s.submit(RunId(20), "q", 100, None, t),
            Admission::Rejected(RejectReason::QueueFull { max_queued: 3 })
        );
    }

    #[test]
    fn reservations_never_exceed_the_budget() {
        let (mut s, t) = sched();
        // Budget 1000, global cap 2 → two 600-byte reservations cannot both run.
        assert!(matches!(
            s.submit(RunId(1), "a", 600, None, t),
            Admission::Start { .. }
        ));
        assert_eq!(
            s.submit(RunId(2), "b", 600, None, t),
            Admission::Queued { position: 1 }
        );
        assert!(s.reserved() <= s.limits().memory_budget_bytes);
        // Releasing frees the budget for the queued run.
        assert_eq!(s.release(RunId(1), false, t), vec![RunId(2)]);
        assert_eq!(s.reserved(), 600);
    }

    #[test]
    fn projects_do_not_starve_each_other() {
        let (mut s, t) = sched();
        assert!(matches!(
            s.submit(RunId(1), "a", 100, None, t),
            Admission::Start { .. }
        ));
        assert!(matches!(
            s.submit(RunId(2), "b", 100, None, t),
            Admission::Start { .. }
        ));
        // Two more from project a queue first, then one from b.
        s.submit(RunId(3), "a", 100, None, t);
        s.submit(RunId(4), "a", 100, None, t);
        s.submit(RunId(5), "b", 100, None, t);
        // b still runs (run 2), so its cap is full too; freeing run 1 (project
        // a) must admit a's oldest queued run, not b's.
        assert_eq!(s.release(RunId(1), false, t), vec![RunId(3)]);
        // Now b's slot frees.
        assert_eq!(s.release(RunId(2), false, t), vec![RunId(5)]);
        assert_eq!(s.release(RunId(3), false, t), vec![RunId(4)]);
    }

    #[test]
    fn a_starved_request_wins_the_next_slot() {
        let (mut s, t) = sched();
        assert!(matches!(
            s.submit(RunId(1), "a", 100, None, t),
            Admission::Start { .. }
        ));
        assert!(matches!(
            s.submit(RunId(2), "b", 100, None, t),
            Admission::Start { .. }
        ));
        s.submit(RunId(3), "c", 100, None, t); // big job, waits
        s.submit(RunId(4), "d", 100, None, t); // arrives later, same project caps
        let later = t + Duration::from_secs(31);
        assert_eq!(
            s.release(RunId(1), false, later),
            vec![RunId(3)],
            "the oldest request wins after the starvation window"
        );
        assert_eq!(s.release(RunId(2), false, later), vec![RunId(4)]);
    }

    #[test]
    fn cancelling_a_queued_request_removes_it_without_admitting() {
        let (mut s, t) = sched();
        s.submit(RunId(1), "a", 100, None, t);
        s.submit(RunId(2), "b", 100, None, t);
        s.submit(RunId(3), "c", 100, None, t);
        assert!(s.cancel_queued(RunId(3)));
        assert!(!s.cancel_queued(RunId(3)), "already gone");
        assert_eq!(s.release(RunId(1), false, t), Vec::<RunId>::new());
        assert_eq!(s.queued_count(), 0);
    }

    #[test]
    fn queue_timeout_expires_requests() {
        let (mut s, t) = sched();
        s.submit(RunId(1), "a", 100, None, t);
        s.submit(RunId(2), "b", 100, None, t);
        s.submit(RunId(3), "c", 100, None, t);
        assert!(s.expire(t + Duration::from_secs(30)).is_empty());
        assert_eq!(s.expire(t + Duration::from_secs(61)), vec![RunId(3)]);
        assert_eq!(s.queued_count(), 0);
    }

    #[test]
    fn queue_entries_report_reason_and_wait_without_an_eta() {
        let (mut s, t) = sched();
        s.submit(RunId(1), "a", 100, None, t);
        s.submit(RunId(2), "a", 100, None, t);
        let entries = s.queue_entries(t + Duration::from_secs(5));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].position, 1);
        assert_eq!(entries[0].waiting_ms, 5000);
        assert!(
            entries[0].reason.contains("project a"),
            "{}",
            entries[0].reason
        );
    }
    /// Logical time keeps moving while the machine sleeps (the supervisor
    /// feeds `max(monotonic, wall)`), so a suspend longer than the queue
    /// timeout must expire the request on wake, not extend its wait.
    #[test]
    fn a_suspend_longer_than_the_queue_timeout_expires_the_request() {
        let (mut s, t) = sched();
        s.submit(RunId(1), "a", 100, None, t);
        s.submit(RunId(2), "b", 100, None, t);
        s.submit(RunId(3), "c", 100, None, t);
        let after_sleep = t + Duration::from_secs(3600);
        assert_eq!(s.expire(after_sleep), vec![RunId(3)]);
        assert_eq!(s.queued_count(), 0);
    }
}
