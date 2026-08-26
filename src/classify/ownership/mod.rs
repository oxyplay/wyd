// ponytail: foundation types consumed by steps 6–7 of the ownership plan;
// dead until wired into the collector.
#![allow(dead_code)]

//! Exact, observed resource ownership over the existing `RuntimeItem` forest.
//!
//! `group()` remains the source of logical resources; this module only reads
//! the produced tree and records which session each resource provably
//! originated from. No heuristic scoring — only directly observed ancestry.

mod resolver;
mod rules;

use crate::model::process::ProcessIdentity;
use crate::model::runtime::{Category, RuntimeItem};
use crate::model::session::{RuntimeSession, RuntimeSessionId, fnv1a};
use std::collections::HashMap;

/// Stable id of one runtime resource, derived from its root process identity.
///
/// The root identity is fixed at first observation: a re-parented survivor
/// keeps this id, and member churn does not mint a new resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId(u64);

impl ResourceId {
    pub fn of_root(root: &ProcessIdentity) -> Self {
        ResourceId(fnv1a(&[
            &root.boot_id.to_le_bytes(),
            &root.pid.to_le_bytes(),
            &root.start_time.to_le_bytes(),
        ]))
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// A resource exactly observed to originate from a session (via direct
/// process ancestry). Carries the metadata the store needs to persist
/// `resources` + `resource_members`.
#[derive(Debug, Clone)]
pub struct OwnedResource {
    pub resource: ResourceId,
    pub kind: Category,
    pub root: ProcessIdentity,
    pub members: Vec<ProcessIdentity>,
    pub session: RuntimeSessionId,
}

/// Result of the exact-ownership pass over one snapshot.
#[derive(Debug, Clone, Default)]
pub struct OwnershipResult {
    pub sessions: Vec<RuntimeSession>,
    pub owned: Vec<OwnedResource>,
}

/// Assign exact ownership by walking the existing `RuntimeItem` forest.
///
/// `identities` maps every pid that has a valid `ProcessIdentity`
/// (`start_time != 0`) to it. An agent with an invalid identity has no
/// session, so its subtree can never be durably owned (live-only).
pub fn derive_ownership(
    items: &[RuntimeItem],
    identities: &HashMap<u32, ProcessIdentity>,
    now: u64,
) -> OwnershipResult {
    let mut out = OwnershipResult::default();
    for item in items {
        walk(item, None, identities, now, &mut out);
    }
    out
}

fn walk(
    item: &RuntimeItem,
    outer: Option<RuntimeSessionId>,
    identities: &HashMap<u32, ProcessIdentity>,
    now: u64,
    out: &mut OwnershipResult,
) {
    let root = item.root_pid.and_then(|pid| identities.get(&pid).copied());

    // A valid agent root starts a session; the nearest enclosing session wins
    // for nested agents. An agent with `start_time == 0` starts none, so its
    // descendants are never durably owned.
    let mut session = outer;
    let mut is_session_root = false;
    if item.category == Category::Agent
        && let Some(root) = root
    {
        let s = RuntimeSession::new(&item.display_name, root, None, now);
        let id = s.id;
        out.sessions.push(s);
        session = Some(id);
        is_session_root = true;
    }

    // Descendants under a session are exactly owned (observed ancestry). The
    // session root itself is not a "resource owned by itself".
    if let (Some(sid), Some(root)) = (session, root)
        && !is_session_root
    {
        let members = item
            .process_ids
            .iter()
            .filter_map(|pid| identities.get(pid).copied())
            .collect();
        out.owned.push(OwnedResource {
            resource: ResourceId::of_root(&root),
            kind: item.category,
            root,
            members,
            session: sid,
        });
    }

    for child in &item.children {
        walk(child, session, identities, now, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::group;
    use crate::model::ProcessInfo;
    use crate::model::boot::BootId;

    const NOW: u64 = 5000;

    fn boot() -> BootId {
        BootId::from_u128(7)
    }

    fn proc(pid: u32, ppid: Option<u32>, name: &str, cmd: &[&str], start: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: ppid,
            name: name.into(),
            command: cmd.iter().map(|s| (*s).to_string()).collect(),
            executable: None,
            cwd: None,
            cpu_percent: 0.0,
            memory_bytes: 40 << 20,
            start_time: start,
            tty: None,
        }
    }

    /// Valid identities (start_time != 0) for every listed pid.
    fn identities(procs: &[ProcessInfo]) -> HashMap<u32, ProcessIdentity> {
        let b = boot();
        procs
            .iter()
            .filter_map(|p| ProcessIdentity::from_process(&b, p).map(|id| (p.pid, id)))
            .collect()
    }

    /// One agent → MCP → Chromium chain, all with valid start times.
    fn agent_chain() -> Vec<ProcessInfo> {
        vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(100, Some(1), "omp", &["omp"], 1000),
            proc(
                110,
                Some(100),
                "node",
                &["node", "chrome-devtools-mcp"],
                1004,
            ),
            proc(120, Some(110), "Chromium", &["Chromium"], 1007),
        ]
    }

    #[test]
    fn agent_tree_assigns_exact_ownership_to_descendants() {
        let procs = agent_chain();
        let items = group(&procs);
        let out = derive_ownership(&items, &identities(&procs), NOW);

        assert_eq!(out.sessions.len(), 1);
        let session = &out.sessions[0];
        assert_eq!(session.agent, "omp");
        assert_eq!(session.started_at, 1000);

        // MCP and Chromium are both owned by the agent session.
        assert_eq!(out.owned.len(), 2);
        for o in &out.owned {
            assert_eq!(o.session, session.id);
        }
        let mcp_root = ProcessIdentity::from_process(&boot(), &procs[2]).unwrap();
        let chrom_root = ProcessIdentity::from_process(&boot(), &procs[3]).unwrap();
        let ids: Vec<ResourceId> = out.owned.iter().map(|o| o.resource).collect();
        assert!(ids.contains(&ResourceId::of_root(&mcp_root)));
        assert!(ids.contains(&ResourceId::of_root(&chrom_root)));
    }

    #[test]
    fn agent_with_zero_start_time_creates_no_session_or_ownership() {
        let mut procs = agent_chain();
        procs[1].start_time = 0; // the agent root loses its identity
        let items = group(&procs);
        let out = derive_ownership(&items, &identities(&procs), NOW);
        assert!(out.sessions.is_empty());
        assert!(out.owned.is_empty());
    }

    #[test]
    fn no_agent_means_no_ownership() {
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(
                20,
                Some(1),
                "node",
                &["node", "node_modules/.bin/vite"],
                200,
            ),
        ];
        let items = group(&procs);
        let out = derive_ownership(&items, &identities(&procs), NOW);
        assert!(out.sessions.is_empty());
        assert!(out.owned.is_empty());
    }

    #[test]
    fn two_agents_produce_two_sessions() {
        let mut procs = agent_chain();
        // A second, unrelated agent tree.
        procs.push(proc(200, Some(1), "codex", &["codex"], 3000));
        procs.push(proc(
            210,
            Some(200),
            "node",
            &["node", "chrome-devtools-mcp"],
            3005,
        ));
        let items = group(&procs);
        let out = derive_ownership(&items, &identities(&procs), NOW);

        let mut agents: Vec<&str> = out.sessions.iter().map(|s| s.agent.as_str()).collect();
        agents.sort();
        assert_eq!(agents, ["codex", "omp"]);
        assert_eq!(out.owned.len(), 3); // MCP + Chromium (omp) + MCP (codex)
    }

    #[test]
    fn resource_id_is_stable_for_same_root() {
        let procs = agent_chain();
        let r = ProcessIdentity::from_process(&boot(), &procs[3]).unwrap();
        assert_eq!(ResourceId::of_root(&r), ResourceId::of_root(&r));
    }

    #[test]
    fn resource_id_differs_across_roots() {
        let procs = agent_chain();
        let a = ProcessIdentity::from_process(&boot(), &procs[2]).unwrap();
        let b = ProcessIdentity::from_process(&boot(), &procs[3]).unwrap();
        assert_ne!(ResourceId::of_root(&a), ResourceId::of_root(&b));
    }
}
