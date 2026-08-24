use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::model::{ListeningPort, ProcessInfo, Project, RuntimeItem};

const MARKERS: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "Cargo.toml",
    "go.mod",
    "composer.json",
    "docker-compose.yml",
    "compose.yml",
];

/// Directory → project cache. Same path is never walked twice.
#[derive(Debug, Default)]
pub struct ProjectCache {
    by_dir: HashMap<PathBuf, Option<Project>>,
}

impl ProjectCache {
    pub fn detect(&mut self, start: &Path) -> Option<Project> {
        let start = normalize(start);
        let mut walked = Vec::new();
        let mut cur = start.as_path();
        loop {
            if let Some(hit) = self.by_dir.get(cur) {
                let result = hit.clone();
                for dir in walked {
                    self.by_dir.insert(dir, result.clone());
                }
                return result;
            }
            if cur.join(".git").exists() {
                let project = Some(project_at(cur));
                for dir in walked {
                    self.by_dir.insert(dir, project.clone());
                }
                self.by_dir.insert(cur.to_path_buf(), project.clone());
                return project;
            }
            walked.push(cur.to_path_buf());
            match cur.parent() {
                Some(parent) if parent != cur => cur = parent,
                _ => break,
            }
        }

        // No git: nearest marker directory, closest to `start`.
        let mut found = None;
        for dir in &walked {
            if MARKERS.iter().any(|m| dir.join(m).exists()) {
                found = Some(project_at(dir));
                break;
            }
        }
        for dir in walked {
            self.by_dir.insert(dir, found.clone());
        }
        found
    }
}

fn project_at(root: &Path) -> Project {
    let name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_string();
    Project {
        name,
        root: root.to_path_buf(),
    }
}

fn normalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Attach listening ports and project roots to grouped items.
pub fn attach(
    items: &mut [RuntimeItem],
    processes: &[ProcessInfo],
    ports: &[ListeningPort],
    cache: &mut ProjectCache,
) {
    let by_pid: HashMap<u32, &ProcessInfo> = processes.iter().map(|p| (p.pid, p)).collect();
    for item in items {
        attach_one(item, &by_pid, ports, cache);
    }
}

fn attach_one(
    item: &mut RuntimeItem,
    by_pid: &HashMap<u32, &ProcessInfo>,
    ports: &[ListeningPort],
    cache: &mut ProjectCache,
) {
    let pids: std::collections::HashSet<u32> = item.process_ids.iter().copied().collect();
    let mut item_ports: Vec<ListeningPort> = ports
        .iter()
        .filter(|p| pids.contains(&p.pid))
        .cloned()
        .collect();
    item_ports.sort_by_key(|p| p.port);
    item_ports.dedup_by_key(|p| p.port);
    item.ports = item_ports;

    item.project = project_for(item, by_pid, cache);

    for child in &mut item.children {
        attach_one(child, by_pid, ports, cache);
        if item.project.is_none() {
            item.project = child.project.clone();
        }
    }
}

fn project_for(
    item: &RuntimeItem,
    by_pid: &HashMap<u32, &ProcessInfo>,
    cache: &mut ProjectCache,
) -> Option<Project> {
    let mut pid = item.root_pid?;
    let mut guard = 0;
    while guard < 64 {
        guard += 1;
        let Some(proc) = by_pid.get(&pid) else {
            break;
        };
        if let Some(cwd) = &proc.cwd
            && let Some(project) = cache.detect(cwd)
        {
            return Some(project);
        }
        pid = proc.parent_pid?;
    }
    None
}

/// `~/Work/foo` when the path sits under $HOME.
pub fn short_path(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(rest) = path.strip_prefix(home)
    {
        if rest.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wyd-proj-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn git_root_wins_and_is_cached() {
        let root = tmp("git");
        fs::create_dir(root.join(".git")).unwrap();
        let nested = root.join("src").join("app");
        fs::create_dir_all(&nested).unwrap();

        let mut cache = ProjectCache::default();
        let a = cache.detect(&nested).unwrap();
        assert_eq!(a.name, root.file_name().unwrap().to_str().unwrap());
        assert_eq!(a.root, root.canonicalize().unwrap());

        // Second lookup must not depend on the directory still walking.
        fs::remove_dir_all(root.join(".git")).unwrap();
        let b = cache.detect(&nested).unwrap();
        assert_eq!(a, b);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn falls_back_to_package_json() {
        let root = tmp("pkg");
        fs::write(root.join("package.json"), "{}").unwrap();
        let mut cache = ProjectCache::default();
        let p = cache.detect(&root).unwrap();
        assert_eq!(p.root, root.canonicalize().unwrap());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_dir_is_none() {
        let root = tmp("empty");
        let mut cache = ProjectCache::default();
        assert!(cache.detect(&root).is_none());
        assert!(cache.detect(&root).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn short_path_uses_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(short_path(Path::new(&home)), "~");
        assert_eq!(short_path(&PathBuf::from(&home).join("Work/x")), "~/Work/x");
    }

    #[test]
    fn attach_joins_listen_port_and_git_project() {
        use crate::classify::group;
        use crate::model::{ListeningPort, ProcessInfo, Protocol};
        use std::net::{IpAddr, Ipv4Addr};

        let root = tmp("vite-app");
        fs::create_dir(root.join(".git")).unwrap();

        let proc = |pid: u32, ppid: Option<u32>, name: &str, cmd: &[&str]| ProcessInfo {
            pid,
            parent_pid: ppid,
            name: name.into(),
            command: cmd.iter().map(|s| (*s).to_string()).collect(),
            executable: None,
            cwd: Some(root.clone()),
            cpu_percent: 0.0,
            memory_bytes: 10,
            start_time: 0,
            tty: None,
        };
        let processes = vec![
            proc(1, None, "launchd", &["launchd"]),
            proc(10, Some(1), "node", &["node", "node_modules/.bin/vite"]),
        ];
        let mut items = group(&processes);
        let ports = vec![ListeningPort {
            protocol: Protocol::Tcp,
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 5173,
            pid: 10,
        }];
        attach(&mut items, &processes, &ports, &mut ProjectCache::default());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].display_name, "vite");
        assert_eq!(items[0].ports[0].label(), ":5173");
        assert_eq!(
            items[0].project.as_ref().map(|p| p.root.clone()),
            Some(root.canonicalize().unwrap())
        );
        let _ = fs::remove_dir_all(&root);
    }
}
