use std::collections::{HashMap, HashSet};

use crate::model::ProcessInfo;

/// Parent→child ancestry forest built from one process snapshot.
///
/// A process whose parent is absent from the snapshot (re-parented to
/// launchd/systemd, or the parent exited) becomes a root. The forest is
/// honest about what is actually visible — it does not invent origin.
#[derive(Debug, Default)]
pub struct Forest {
    children: HashMap<u32, Vec<u32>>,
    #[cfg(test)]
    roots: Vec<u32>,
    #[cfg(test)]
    pids: HashSet<u32>,
}

impl Forest {
    pub fn build(processes: &[ProcessInfo]) -> Self {
        let present: HashSet<u32> = processes.iter().map(|p| p.pid).collect();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        #[cfg(test)]
        let mut roots = Vec::new();

        for p in processes {
            match p.parent_pid {
                Some(ppid) if present.contains(&ppid) => {
                    children.entry(ppid).or_default().push(p.pid);
                }
                #[cfg(test)]
                _ => roots.push(p.pid),
                #[cfg(not(test))]
                _ => {}
            }
        }
        // Stable, useful default order: biggest consumers first.
        let mem: HashMap<u32, u64> = processes.iter().map(|p| (p.pid, p.memory_bytes)).collect();
        #[cfg(test)]
        roots.sort_by_key(|pid| std::cmp::Reverse(mem.get(pid).copied().unwrap_or(0)));
        for kids in children.values_mut() {
            kids.sort_by_key(|pid| std::cmp::Reverse(mem.get(pid).copied().unwrap_or(0)));
        }

        Self {
            children,
            #[cfg(test)]
            roots,
            #[cfg(test)]
            pids: present,
        }
    }

    #[cfg(test)]
    pub fn roots(&self) -> &[u32] {
        &self.roots
    }

    pub fn children(&self, pid: u32) -> &[u32] {
        self.children.get(&pid).map_or(&[], Vec::as_slice)
    }

    #[cfg(test)]
    pub fn is_detached(&self, p: &ProcessInfo) -> bool {
        p.parent_pid.is_some_and(|ppid| !self.pids.contains(&ppid))
    }

    /// Depth-first pre-order traversal: `(pid, depth)`, roots at depth 0.
    /// Cycle-safe against malformed snapshots (visited set).
    #[cfg(test)]
    pub fn preorder(&self) -> Vec<(u32, usize)> {
        let mut out = Vec::new();
        let mut visited = HashSet::new();
        self.walk(self.roots.iter().copied(), &mut visited, &mut out);
        // Cycles have no root; still visit every pid.
        let leftovers: Vec<u32> = self
            .pids
            .iter()
            .copied()
            .filter(|pid| !visited.contains(pid))
            .collect();
        self.walk(leftovers, &mut visited, &mut out);
        out
    }
    #[cfg(test)]
    fn walk(
        &self,
        starts: impl IntoIterator<Item = u32>,
        visited: &mut HashSet<u32>,
        out: &mut Vec<(u32, usize)>,
    ) {
        let starts: Vec<u32> = starts.into_iter().collect();
        let mut stack: Vec<(u32, usize)> = starts.into_iter().map(|pid| (pid, 0)).rev().collect();
        while let Some((pid, depth)) = stack.pop() {
            if !visited.insert(pid) {
                continue;
            }
            out.push((pid, depth));
            if let Some(kids) = self.children.get(&pid) {
                for &kid in kids.iter().rev() {
                    stack.push((kid, depth + 1));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: Option<u32>, name: &str, mem: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: ppid,
            name: name.into(),
            command: vec![name.into()],
            executable: None,
            cwd: None,
            cpu_percent: 0.0,
            memory_bytes: mem,
            start_time: 0,
            tty: None,
        }
    }

    /// Ghostty → zsh → omp → MCP → Chromium ×2, plus one orphan.
    fn fixture() -> Vec<ProcessInfo> {
        vec![
            proc(1, None, "launchd", 10),
            proc(100, Some(1), "ghostty", 50),
            proc(110, Some(100), "zsh", 5),
            proc(120, Some(110), "omp", 300),
            proc(130, Some(120), "chrome-devtools-mcp", 40),
            proc(140, Some(130), "Chromium", 500),
            proc(141, Some(130), "Chromium Helper", 200),
            // Detached: parent 999 absent from snapshot.
            proc(150, Some(999), "Chromium", 400),
        ]
    }

    #[test]
    fn builds_parent_child_links() {
        let forest = Forest::build(&fixture());
        assert_eq!(forest.children(130), &[140, 141]); // mem desc
        assert_eq!(forest.children(110), &[120]);
        assert!(forest.children(150).is_empty());
    }

    #[test]
    fn reparented_process_becomes_root() {
        let forest = Forest::build(&fixture());
        let roots = forest.roots();
        assert!(roots.contains(&1));
        assert!(roots.contains(&150), "orphan must surface as root");
        assert!(forest.is_detached(&fixture()[7]));
        assert!(!forest.is_detached(&fixture()[5]));
    }

    #[test]
    fn preorder_depths_are_correct() {
        let forest = Forest::build(&fixture());
        let order = forest.preorder();
        let depth_of = |pid: u32| order.iter().find(|(p, _)| *p == pid).unwrap().1;
        assert_eq!(depth_of(1), 0);
        assert_eq!(depth_of(100), 1);
        assert_eq!(depth_of(120), 3);
        assert_eq!(depth_of(140), 5);
        assert_eq!(depth_of(150), 0);
        assert_eq!(order.len(), 8, "every process visited exactly once");
    }

    #[test]
    fn preorder_roots_sorted_by_memory_desc() {
        let forest = Forest::build(&fixture());
        let roots: Vec<u32> = forest
            .preorder()
            .into_iter()
            .filter(|(_, d)| *d == 0)
            .map(|(p, _)| p)
            .collect();
        assert_eq!(roots, &[150, 1]); // 400 MB orphan before 10 MB launchd
    }

    #[test]
    fn cycle_in_malformed_snapshot_terminates() {
        let procs = vec![proc(1, Some(2), "a", 1), proc(2, Some(1), "b", 1)];
        let order = Forest::build(&procs).preorder();
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn empty_snapshot() {
        let forest = Forest::build(&[]);
        assert!(forest.roots().is_empty());
        assert!(forest.preorder().is_empty());
    }
}
