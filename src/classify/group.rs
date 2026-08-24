use std::collections::{HashMap, HashSet};

use crate::classify::Forest;
use crate::classify::rules::{self, Class};
use crate::model::{Category, ProcessInfo, RuntimeItem, RuntimeState};

/// Group a process snapshot into logical items. Unclassified OS processes
/// are dropped. Browser helpers collapse to `Chromium ×N` under their MCP.
pub fn group(processes: &[ProcessInfo]) -> Vec<RuntimeItem> {
    if processes.is_empty() {
        return Vec::new();
    }

    let forest = Forest::build(processes);
    let by_pid: HashMap<u32, &ProcessInfo> = processes.iter().map(|p| (p.pid, p)).collect();
    let class: HashMap<u32, Class> = processes
        .iter()
        .filter_map(|p| rules::classify(p).map(|c| (p.pid, c)))
        .collect();

    let mut keep_browser = HashSet::new();
    for p in processes {
        let Some(c) = class.get(&p.pid) else { continue };
        if c.category != Category::Browser {
            continue;
        }
        if rules::browser_cmd_looks_dev(p) || has_dev_browser_ancestor(p.pid, &by_pid, &class) {
            keep_browser.insert(p.pid);
        }
    }

    let is_classified = |pid: u32| -> bool {
        match class.get(&pid) {
            Some(c) if c.category == Category::Browser => keep_browser.contains(&pid),
            Some(_) => true,
            None => false,
        }
    };

    let wrapper: HashSet<u32> = processes
        .iter()
        .filter(|p| {
            class
                .get(&p.pid)
                .is_some_and(|c| c.category == Category::UnknownDev)
                && has_classified_descendant(p.pid, &forest, is_classified)
        })
        .map(|p| p.pid)
        .collect();

    let is_item = |pid: u32| -> bool {
        let Some(c) = class.get(&pid) else {
            return false;
        };
        if !is_classified(pid) || wrapper.contains(&pid) || c.category == Category::Browser {
            return false;
        }
        // A `node`/`python` with no argv is not a useful item — we cannot
        // say what it is. Wrappers with classified children are already skipped.
        if c.category == Category::UnknownDev
            && by_pid.get(&pid).is_none_or(|p| p.command.is_empty())
        {
            return false;
        }
        true
    };

    let mut parent_item: HashMap<u32, u32> = HashMap::new();
    for p in processes {
        if !is_item(p.pid) {
            continue;
        }
        if let Some(anc) = nearest_item_ancestor(p.pid, &by_pid, is_item) {
            parent_item.insert(p.pid, anc);
        }
    }

    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut roots = Vec::new();
    for p in processes {
        if !is_item(p.pid) {
            continue;
        }
        match parent_item.get(&p.pid) {
            Some(&par) => kids.entry(par).or_default().push(p.pid),
            None => roots.push(p.pid),
        }
    }

    // Stable: biggest first.
    let mem = |pid: u32| by_pid.get(&pid).map_or(0, |p| p.memory_bytes);
    roots.sort_by_key(|pid| std::cmp::Reverse(mem(*pid)));
    for v in kids.values_mut() {
        v.sort_by_key(|pid| std::cmp::Reverse(mem(*pid)));
    }

    let mut items: Vec<RuntimeItem> = roots
        .into_iter()
        .map(|pid| build_item(pid, &kids, &by_pid, &class, &forest, &keep_browser))
        .collect();
    for item in &mut items {
        collapse_same(item);
    }
    items
}

fn build_item(
    pid: u32,
    kids: &HashMap<u32, Vec<u32>>,
    by_pid: &HashMap<u32, &ProcessInfo>,
    class: &HashMap<u32, Class>,
    forest: &Forest,
    keep_browser: &HashSet<u32>,
) -> RuntimeItem {
    let p = by_pid[&pid];
    let c = &class[&pid];
    let mut children: Vec<RuntimeItem> = kids
        .get(&pid)
        .into_iter()
        .flatten()
        .map(|&k| build_item(k, kids, by_pid, class, forest, keep_browser))
        .collect();

    if c.category == Category::Mcp
        && let Some(rollup) = rollup_browsers(pid, forest, by_pid, class, keep_browser)
    {
        children.push(rollup);
    }

    RuntimeItem {
        category: c.category,
        display_name: c.display_name.clone(),
        root_pid: Some(pid),
        process_ids: vec![pid],
        memory_bytes: p.memory_bytes,
        cpu_percent: p.cpu_percent,
        state: if c.category == Category::Database {
            RuntimeState::Persistent
        } else {
            RuntimeState::Active
        },
        ports: Vec::new(),
        project: None,
        children,
    }
}

fn rollup_browsers(
    mcp_pid: u32,
    forest: &Forest,
    by_pid: &HashMap<u32, &ProcessInfo>,
    class: &HashMap<u32, Class>,
    keep_browser: &HashSet<u32>,
) -> Option<RuntimeItem> {
    let mut pids = Vec::new();
    let mut stack = forest.children(mcp_pid).to_vec();
    while let Some(pid) = stack.pop() {
        stack.extend_from_slice(forest.children(pid));
        if keep_browser.contains(&pid) {
            pids.push(pid);
        }
    }
    if pids.is_empty() {
        return None;
    }
    pids.sort_unstable();
    let memory_bytes = pids
        .iter()
        .map(|pid| by_pid.get(pid).map_or(0, |p| p.memory_bytes))
        .sum();
    let cpu_percent = pids
        .iter()
        .map(|pid| by_pid.get(pid).map_or(0.0, |p| p.cpu_percent))
        .sum();
    let display_name = class
        .get(&pids[0])
        .map(|c| c.display_name.clone())
        .unwrap_or_else(|| "Chromium".into());
    Some(RuntimeItem {
        category: Category::Browser,
        display_name,
        root_pid: Some(pids[0]),
        process_ids: pids,
        memory_bytes,
        cpu_percent,
        state: RuntimeState::Active,
        ports: Vec::new(),
        project: None,
        children: Vec::new(),
    })
}

fn has_dev_browser_ancestor(
    pid: u32,
    by_pid: &HashMap<u32, &ProcessInfo>,
    class: &HashMap<u32, Class>,
) -> bool {
    let mut cur = by_pid.get(&pid).and_then(|p| p.parent_pid);
    let mut guard = 0;
    while let Some(ppid) = cur {
        guard += 1;
        if guard > 64 {
            break;
        }
        if class.get(&ppid).is_some_and(|c| {
            matches!(c.category, Category::Mcp | Category::Agent)
                || c.display_name.contains("playwright")
        }) {
            return true;
        }
        cur = by_pid.get(&ppid).and_then(|p| p.parent_pid);
    }
    false
}

fn has_classified_descendant(
    pid: u32,
    forest: &Forest,
    is_classified: impl Fn(u32) -> bool,
) -> bool {
    let mut stack = forest.children(pid).to_vec();
    while let Some(c) = stack.pop() {
        if is_classified(c) {
            return true;
        }
        stack.extend_from_slice(forest.children(c));
    }
    false
}

fn nearest_item_ancestor(
    pid: u32,
    by_pid: &HashMap<u32, &ProcessInfo>,
    is_item: impl Fn(u32) -> bool,
) -> Option<u32> {
    let mut cur = by_pid.get(&pid).and_then(|p| p.parent_pid);
    let mut guard = 0;
    while let Some(ppid) = cur {
        guard += 1;
        if guard > 64 {
            break;
        }
        if is_item(ppid) {
            return Some(ppid);
        }
        cur = by_pid.get(&ppid).and_then(|p| p.parent_pid);
    }
    None
}

/// Fold helper processes that share the parent's category and name
/// (`omp` workers, `php-fpm` children) into the parent item.
fn collapse_same(item: &mut RuntimeItem) {
    for child in &mut item.children {
        collapse_same(child);
    }
    let mut kept = Vec::new();
    let mut absorbed = Vec::new();
    for child in std::mem::take(&mut item.children) {
        if child.category == item.category && child.display_name == item.display_name {
            item.process_ids.extend(child.process_ids);
            item.memory_bytes += child.memory_bytes;
            item.cpu_percent += child.cpu_percent;
            absorbed.extend(child.children);
        } else {
            kept.push(child);
        }
    }
    kept.extend(absorbed);
    item.children = kept;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: Option<u32>, name: &str, cmd: &[&str], mem: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: ppid,
            name: name.into(),
            command: cmd.iter().map(|s| (*s).to_string()).collect(),
            executable: None,
            cwd: None,
            cpu_percent: 0.0,
            memory_bytes: mem,
            start_time: 0,
            tty: None,
        }
    }

    fn titles(items: &[RuntimeItem]) -> Vec<String> {
        items.iter().map(|i| i.title()).collect()
    }

    #[test]
    fn agent_tree_rolls_up_chromium() {
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 10),
            proc(100, Some(1), "ghostty", &["ghostty"], 50),
            proc(110, Some(100), "zsh", &["zsh"], 5),
            proc(120, Some(110), "omp", &["omp"], 300),
            proc(
                125,
                Some(120),
                "npm",
                &["npm", "exec", "chrome-devtools-mcp"],
                20,
            ),
            proc(
                130,
                Some(125),
                "node",
                &["node", "/x/chrome-devtools-mcp/index.js"],
                40,
            ),
            proc(140, Some(130), "Chromium", &["Chromium"], 500),
            proc(141, Some(130), "Chromium Helper", &["Chromium Helper"], 200),
        ];
        let items = group(&procs);
        assert_eq!(titles(&items), ["omp"]);
        assert_eq!(titles(&items[0].children), ["chrome-devtools-mcp"]);
        assert_eq!(titles(&items[0].children[0].children), ["Chromium ×2"]);
        assert_eq!(items[0].children[0].children[0].memory_bytes, 700);
    }

    #[test]
    fn detached_mcp_is_root() {
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(10, Some(1), "node", &["node", "playwright-mcp"], 40),
            proc(11, Some(10), "Chromium", &["Chromium"], 100),
        ];
        let items = group(&procs);
        assert_eq!(titles(&items), ["playwright-mcp"]);
        assert_eq!(titles(&items[0].children), ["Chromium"]);
    }

    #[test]
    fn desktop_chrome_is_hidden() {
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(20, Some(1), "Google Chrome", &["Google Chrome"], 800),
            proc(21, Some(20), "Chrome Helper", &["Chrome Helper"], 200),
        ];
        assert!(group(&procs).is_empty());
    }

    #[test]
    fn old_vite_and_postgres() {
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(
                30,
                Some(1),
                "node",
                &["node", "node_modules/.bin/vite"],
                180,
            ),
            proc(40, Some(1), "postgres", &["postgres"], 80),
        ];
        let items = group(&procs);
        assert_eq!(titles(&items), ["vite", "postgres"]);
        assert_eq!(items[0].category, Category::DevServer);
        assert_eq!(items[1].category, Category::Database);
        assert_eq!(items[1].state, RuntimeState::Persistent);
    }

    #[test]
    fn two_agents_and_language_servers() {
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(10, Some(1), "omp", &["omp"], 100),
            proc(11, Some(10), "node", &["node", "chrome-devtools-mcp"], 10),
            proc(20, Some(1), "opencode", &["opencode"], 200),
            proc(21, Some(20), "node", &["node", "playwright-mcp"], 10),
            proc(50, Some(1), "rust-analyzer", &["rust-analyzer"], 90),
            proc(
                51,
                Some(1),
                "typescript-language-server",
                &["typescript-language-server"],
                40,
            ),
        ];
        let items = group(&procs);
        assert_eq!(
            titles(&items),
            [
                "opencode",
                "omp",
                "rust-analyzer",
                "typescript-language-server"
            ]
        );
    }

    #[test]
    fn hides_os_only_snapshot() {
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(2, Some(1), "kernel_task", &["kernel_task"], 1),
        ];
        assert!(group(&procs).is_empty());
    }

    #[test]
    fn empty_cmd_node_is_hidden_scripted_node_is_not() {
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(10, Some(1), "node", &[], 20),
            proc(20, Some(1), "node", &["node", "server.js"], 30),
        ];
        let items = group(&procs);
        assert_eq!(titles(&items), ["node"]);
        assert_eq!(items[0].root_pid, Some(20));
    }

    #[test]
    fn nested_omp_helpers_collapse() {
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(10, Some(1), "omp", &["omp"], 100),
            proc(11, Some(10), "omp", &["omp"], 50),
            proc(12, Some(11), "omp", &["omp"], 25),
            proc(13, Some(10), "node", &["node", "chrome-devtools-mcp"], 10),
        ];
        let items = group(&procs);
        assert_eq!(titles(&items), ["omp ×3"]);
        assert_eq!(titles(&items[0].children), ["chrome-devtools-mcp"]);
        assert_eq!(items[0].memory_bytes, 175);
    }
}
