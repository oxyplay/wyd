use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::model::{Category, ProcessInfo, RuntimeItem, RuntimeState, Suspicion, SuspicionReason};

/// Score grouped items. Persistent services are never leftovers.
pub fn mark(items: &mut [RuntimeItem], processes: &[ProcessInfo], cfg: &Config) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let present: HashSet<u32> = processes.iter().map(|p| p.pid).collect();
    for item in items {
        mark_one(item, None, processes, &present, cfg, now);
    }
}

fn mark_one(
    item: &mut RuntimeItem,
    parent: Option<Category>,
    processes: &[ProcessInfo],
    present: &HashSet<u32>,
    cfg: &Config,
    now: u64,
) {
    for child in &mut item.children {
        mark_one(child, Some(item.category), processes, present, cfg, now);
    }

    if item.category == Category::Agent {
        item.suspicion = None;
        return;
    }
    if exempt(item, processes, cfg) {
        item.state = RuntimeState::Persistent;
        item.suspicion = None;
        return;
    }

    let mut reasons = Vec::new();
    let mut score: u16 = 0;

    let orphaned = parent.is_none()
        && matches!(
            item.category,
            Category::Mcp
                | Category::Browser
                | Category::DevServer
                | Category::UnknownDev
                | Category::LanguageServer
        );

    if orphaned
        && matches!(
            item.category,
            Category::Mcp | Category::Browser | Category::LanguageServer
        )
    {
        reasons.push(SuspicionReason::OwningAgentMissing);
        score += 50;
    }
    if item.category == Category::Mcp && parent.is_none() {
        reasons.push(SuspicionReason::McpOwnerMissing);
        score += 25;
    }
    if item.category == Category::Browser && parent.is_none() {
        reasons.push(SuspicionReason::HeadlessBrowserDetached);
        score += 30;
    }

    if let Some(p) = root_proc(item, processes) {
        if reparented(p, present) && orphaned {
            reasons.push(SuspicionReason::ParentExited);
            score += 40;
        }
        if !has_tty_ancestor(p, processes) && orphaned {
            reasons.push(SuspicionReason::NoTerminalAncestor);
            score += 15;
        }
        if item.category == Category::DevServer {
            let age_h = now.saturating_sub(p.start_time) / 3600;
            if p.start_time > 0 && age_h >= cfg.leftovers.server_age_hours {
                reasons.push(SuspicionReason::LongRunningDevServer);
                score += 20;
            }
        }
    }

    let score = score.min(100) as u8;
    if score >= 30 {
        item.state = RuntimeState::Suspicious;
        item.suspicion = Some(Suspicion { score, reasons });
    }
}

fn exempt(item: &RuntimeItem, processes: &[ProcessInfo], cfg: &Config) -> bool {
    if matches!(item.category, Category::Database | Category::DevService) {
        return true;
    }
    let Some(p) = root_proc(item, processes) else {
        return false;
    };
    let hay = format!(
        "{} {} {}",
        p.name.to_ascii_lowercase(),
        p.command.join(" ").to_ascii_lowercase(),
        p.executable
            .as_ref()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    );
    // Homebrew in the *node* path is not a persistent service — leftover
    // LSPs launched via brew-installed node must still score as leftovers.
    if !matches!(item.category, Category::LanguageServer)
        && (hay.contains("homebrew")
            || hay.contains("valet")
            || hay.contains("com.docker")
            || hay.contains("docker desktop"))
    {
        return true;
    }
    cfg.persistent
        .commands
        .iter()
        .any(|c| hay.contains(&c.to_ascii_lowercase()))
}

fn root_proc<'a>(item: &RuntimeItem, processes: &'a [ProcessInfo]) -> Option<&'a ProcessInfo> {
    let pid = item.root_pid?;
    processes.iter().find(|p| p.pid == pid)
}

fn reparented(p: &ProcessInfo, present: &HashSet<u32>) -> bool {
    match p.parent_pid {
        None => true,
        Some(1) => true,
        Some(ppid) => !present.contains(&ppid),
    }
}

fn has_tty_ancestor(start: &ProcessInfo, processes: &[ProcessInfo]) -> bool {
    let mut pid = start.pid;
    let mut guard = 0;
    while guard < 64 {
        guard += 1;
        let Some(p) = processes.iter().find(|x| x.pid == pid) else {
            break;
        };
        if p.tty.is_some() {
            return true;
        }
        pid = match p.parent_pid {
            Some(ppid) => ppid,
            None => break,
        };
    }
    false
}

pub fn leftover_ram(items: &[RuntimeItem]) -> u64 {
    items
        .iter()
        .map(|i| {
            let here = if i.state == RuntimeState::Suspicious {
                i.memory_bytes
            } else {
                0
            };
            here + leftover_ram(&i.children)
        })
        .sum()
}

pub fn leftover_count(items: &[RuntimeItem]) -> usize {
    items
        .iter()
        .map(|i| usize::from(i.state == RuntimeState::Suspicious) + leftover_count(&i.children))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::group;
    use crate::model::ProcessInfo;

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

    #[test]
    fn detached_mcp_is_leftover() {
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(10, Some(1), "node", &["node", "chrome-devtools-mcp"], 100),
        ];
        let mut items = group(&procs);
        mark(&mut items, &procs, &Config::default());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].state, RuntimeState::Suspicious);
        let s = items[0].suspicion.as_ref().unwrap();
        assert!(s.reasons.contains(&SuspicionReason::OwningAgentMissing));
        assert!(s.reasons.contains(&SuspicionReason::McpOwnerMissing));
        assert!(s.score >= 60);
    }

    #[test]
    fn orphaned_lsp_is_leftover_nested_is_not() {
        let detached = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(
                10,
                Some(1),
                "node",
                &[
                    "node",
                    "/x/.npm/_npx/x/node_modules/.bin/copilot-language-server",
                    "--stdio",
                ],
                100,
            ),
        ];
        let mut detached = detached;
        detached[1].executable = Some("/opt/homebrew/bin/node".into());
        let mut items = group(&detached);
        mark(&mut items, &detached, &Config::default());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].display_name, "copilot");
        assert_eq!(items[0].state, RuntimeState::Suspicious);
        assert!(
            items[0]
                .suspicion
                .as_ref()
                .unwrap()
                .reasons
                .contains(&SuspicionReason::OwningAgentMissing)
        );

        let nested = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(10, Some(1), "omp", &["omp"], 100),
            proc(
                11,
                Some(10),
                "node",
                &[
                    "node",
                    "/x/.npm/_npx/x/node_modules/.bin/copilot-language-server",
                    "--stdio",
                ],
                100,
            ),
        ];
        let mut items = group(&nested);
        mark(&mut items, &nested, &Config::default());
        assert_eq!(items[0].category, Category::Agent);
        assert_eq!(items[0].children[0].display_name, "copilot");
        assert_eq!(items[0].children[0].state, RuntimeState::Active);
        assert!(items[0].children[0].suspicion.is_none());
    }

    #[test]
    fn mcp_under_agent_is_not_leftover() {
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(10, Some(1), "omp", &["omp"], 100),
            proc(11, Some(10), "node", &["node", "chrome-devtools-mcp"], 100),
        ];
        let mut items = group(&procs);
        mark(&mut items, &procs, &Config::default());
        assert_eq!(items[0].category, Category::Agent);
        assert_eq!(items[0].state, RuntimeState::Active);
        assert_eq!(items[0].children[0].state, RuntimeState::Active);
        assert!(items[0].children[0].suspicion.is_none());
    }

    #[test]
    fn old_vite_is_leftover_postgres_is_not() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(
                20,
                Some(1),
                "node",
                &["node", "node_modules/.bin/vite"],
                now - 20 * 3600,
            ),
            proc(30, Some(1), "postgres", &["postgres"], now - 100 * 3600),
        ];
        let mut items = group(&procs);
        mark(&mut items, &procs, &Config::default());
        let vite = items.iter().find(|i| i.display_name == "vite").unwrap();
        let pg = items.iter().find(|i| i.display_name == "postgres").unwrap();
        assert_eq!(vite.state, RuntimeState::Suspicious);
        assert!(
            vite.suspicion
                .as_ref()
                .unwrap()
                .reasons
                .contains(&SuspicionReason::LongRunningDevServer)
        );
        assert_eq!(pg.state, RuntimeState::Persistent);
        assert!(pg.suspicion.is_none());
    }

    #[test]
    fn config_persistent_command_exempts() {
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(10, Some(1), "node", &["node", "chrome-devtools-mcp"], 100),
        ];
        let mut cfg = Config::default();
        cfg.persistent.commands.push("chrome-devtools-mcp".into());
        let mut items = group(&procs);
        mark(&mut items, &procs, &cfg);
        assert_eq!(items[0].state, RuntimeState::Persistent);
    }
}
