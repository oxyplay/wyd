//! Runtime session identity and model.
//!
//! A Wyd session is **one observed invocation of an agent runtime process** —
//! not a chat, conversation, task or vendor session. Restarting the agent
//! process creates a new session.
//!
//! The session id must reproduce across Wyd restarts (same boot, same process
//! invocation → same id), so it uses a stable FNV-1a over the identity parts,
//! not `std`'s `DefaultHasher` (whose algorithm is not version-stable).

use crate::model::boot::BootId;
use crate::model::process::ProcessIdentity;
use crate::model::project::Project;

/// Stable id of one agent runtime invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeSessionId(u64);

impl RuntimeSessionId {
    pub fn new(boot_id: &BootId, root: &ProcessIdentity, agent: &str) -> Self {
        RuntimeSessionId(fnv1a(&[
            &boot_id.to_le_bytes(),
            &root.pid.to_le_bytes(),
            &root.start_time.to_le_bytes(),
            agent.as_bytes(),
        ]))
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }

    pub fn from_u64(v: u64) -> Self {
        RuntimeSessionId(v)
    }
}

impl std::fmt::Display for RuntimeSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// One observed invocation of an agent runtime process.
///
/// `project` is metadata and is not part of identity: an agent changing cwd
/// does not create a new session.
#[derive(Debug, Clone)]
pub struct RuntimeSession {
    pub id: RuntimeSessionId,
    pub agent: String,
    pub root: ProcessIdentity,
    pub project: Option<Project>,
    pub started_at: u64,
    /// ponytail: not yet read by live code; persisted via `now` in
    /// `apply_ownership`. Kept for the future session view.
    #[allow(dead_code)]
    pub last_seen_at: u64,
    pub ended_at: Option<u64>,
}

impl RuntimeSession {
    pub fn new(agent: &str, root: ProcessIdentity, project: Option<Project>, now: u64) -> Self {
        let id = RuntimeSessionId::new(&root.boot_id, &root, agent);
        Self {
            id,
            agent: agent.to_string(),
            root,
            project,
            started_at: root.start_time,
            last_seen_at: now,
            ended_at: None,
        }
    }
}

/// Lightweight, snapshot-friendly view of a session for display.
/// `active` = not yet ended.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: RuntimeSessionId,
    pub agent: String,
    pub project: Option<String>,
    pub active: bool,
    pub started_at: u64,
}

/// FNV-1a 64-bit, stable across builds and runs. Shared by session and
/// resource id derivation.
pub(crate) fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &b in *part {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot(id: u128) -> BootId {
        BootId::from_u128(id)
    }

    fn root(boot_id: BootId, pid: u32, start: u64) -> ProcessIdentity {
        ProcessIdentity {
            boot_id,
            pid,
            start_time: start,
        }
    }

    #[test]
    fn id_is_stable_for_same_invocation() {
        let a = RuntimeSessionId::new(&boot(1), &root(boot(1), 100, 1000), "claude");
        let b = RuntimeSessionId::new(&boot(1), &root(boot(1), 100, 1000), "claude");
        assert_eq!(a, b);
    }

    #[test]
    fn restart_is_a_new_session() {
        // Same PID, different start_time = a new process invocation.
        let a = RuntimeSessionId::new(&boot(1), &root(boot(1), 100, 1000), "claude");
        let b = RuntimeSessionId::new(&boot(1), &root(boot(1), 100, 5000), "claude");
        assert_ne!(a, b);
    }

    #[test]
    fn different_agent_is_a_new_session() {
        let a = RuntimeSessionId::new(&boot(1), &root(boot(1), 100, 1000), "claude");
        let b = RuntimeSessionId::new(&boot(1), &root(boot(1), 100, 1000), "codex");
        assert_ne!(a, b);
    }

    #[test]
    fn different_boot_is_a_new_session() {
        let a = RuntimeSessionId::new(&boot(1), &root(boot(1), 100, 1000), "claude");
        let b = RuntimeSessionId::new(&boot(2), &root(boot(2), 100, 1000), "claude");
        assert_ne!(a, b);
    }

    #[test]
    fn project_is_not_part_of_identity() {
        // Project is metadata: the same invocation is the same session
        // regardless of project, and cwd changes never mint a new id.
        let base = root(boot(1), 100, 1000);
        let a = Project {
            name: "queryknight".into(),
            root: "/src/queryknight".into(),
        };
        let b = Project {
            name: "databoundary".into(),
            root: "/src/databoundary".into(),
        };
        let s1 = RuntimeSession::new("claude", base, Some(a), 5000);
        let s2 = RuntimeSession::new("claude", base, Some(b), 5000);
        assert_eq!(s1.id, s2.id, "project must not change session identity");
    }

    #[test]
    fn new_session_uses_process_start_time() {
        let s = RuntimeSession::new("claude", root(boot(1), 100, 1000), None, 5000);
        assert_eq!(s.started_at, 1000, "started_at = process start_time");
        assert_eq!(s.last_seen_at, 5000);
        assert_eq!(s.ended_at, None);
    }
}
