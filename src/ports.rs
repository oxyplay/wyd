//! `wyd ports`: one entry per listening port — the process on it and what
//! started it. The origin is the owning agent session from durable
//! provenance, or the system source (systemd/launchd/cron/tmux/ssh/…) when no
//! session owns it.

use serde::Serialize;

use crate::model::boot::BootId;
use crate::model::process::ProcessIdentity;
use crate::model::{ListeningPort, ProcessInfo};
use crate::source;
use crate::store::RuntimeStore;

/// Resolved provenance for owner attribution. Absent when the store or boot
/// identity is unavailable — `wyd ports` then reports only the system source.
pub struct Provenance {
    pub store: RuntimeStore,
    pub boot: BootId,
}

/// One listening port with its owning process and origin.
///
/// `owner_*` are present together when an agent session owns the listener;
/// `source` is present otherwise and names the system source. `process` is
/// absent when the pid is not in the current process snapshot.
#[derive(Debug, Serialize)]
pub struct PortEntry {
    pub address: String,
    pub port: u16,
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// One entry per port, sorted by port then address for stable output.
pub fn collect(
    ports: &[ListeningPort],
    processes: &[ProcessInfo],
    prov: Option<&Provenance>,
) -> Vec<PortEntry> {
    let mut entries: Vec<PortEntry> = ports.iter().map(|p| entry(p, processes, prov)).collect();
    entries.sort_by(|a, b| (a.port, a.address.as_str()).cmp(&(b.port, b.address.as_str())));
    entries
}

fn entry(port: &ListeningPort, processes: &[ProcessInfo], prov: Option<&Provenance>) -> PortEntry {
    let proc = processes.iter().find(|p| p.pid == port.pid);
    let mut e = PortEntry {
        address: port.address.to_string(),
        port: port.port,
        pid: port.pid,
        process: proc.map(|p| p.name.clone()),
        owner_agent: None,
        owner_session: None,
        owner_state: None,
        source: None,
    };

    // Owner attribution needs a live identity and durable provenance; without
    // either, fall back to the system source (best effort).
    if let (Some(prov), Some(proc)) = (prov, proc)
        && let Some(identity) = ProcessIdentity::from_process(&prov.boot, proc)
        && let Ok(Some(exp)) = prov
            .store
            .explain_process(&prov.boot, port.pid, identity.start_time)
    {
        e.owner_agent = Some(exp.session.agent.clone());
        e.owner_session = Some(exp.session.id.to_string());
        e.owner_state = Some(
            if exp.session.ended_at.is_some() {
                "ended"
            } else {
                "active"
            }
            .to_string(),
        );
        return e;
    }

    e.source = Some(source::detect(port.pid, processes).source.label());
    e
}

/// One line per port: `address:port  process (pid)  origin`.
pub fn render_plain(entries: &[PortEntry]) -> String {
    if entries.is_empty() {
        return "none".into();
    }
    let mut lines = Vec::with_capacity(entries.len());
    for e in entries {
        let proc = match &e.process {
            Some(n) => format!("{n} ({})", e.pid),
            None => format!("pid {}", e.pid),
        };
        let who = match (&e.owner_agent, &e.owner_state) {
            (Some(a), Some(st)) => format!("session {a} ({st})"),
            _ => e.source.clone().unwrap_or_else(|| "unknown".into()),
        };
        lines.push(format!(
            "{}:{}  {}  {}",
            display_addr(&e.address),
            e.port,
            proc,
            who
        ));
    }
    lines.join("\n")
}

/// `[::1]` for IPv6, `127.0.0.1` otherwise — so `addr:port` stays readable.
fn display_addr(address: &str) -> String {
    if address.contains(':') {
        format!("[{address}]")
    } else {
        address.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn port(port: u16, pid: u32) -> ListeningPort {
        ListeningPort {
            protocol: crate::model::Protocol::Tcp,
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            pid,
        }
    }

    fn proc(pid: u32, ppid: Option<u32>, name: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: ppid,
            name: name.into(),
            command: vec![name.into()],
            executable: None,
            cwd: None,
            cpu_percent: 0.0,
            memory_bytes: 0,
            start_time: 1000 + pid as u64,
            tty: None,
        }
    }

    #[test]
    fn sorts_by_port() {
        let ports = vec![port(8080, 10), port(3000, 11), port(5001, 12)];
        let out = collect(&ports, &[], None);
        assert_eq!(
            out.iter().map(|e| e.port).collect::<Vec<_>>(),
            vec![3000, 5001, 8080]
        );
    }

    #[test]
    fn process_name_is_resolved_and_absent_tolerated() {
        let procs = vec![proc(10, Some(1), "node")];
        let out = collect(&[port(3000, 10), port(3001, 99)], &procs, None);
        assert_eq!(out[0].process.as_deref(), Some("node"));
        assert_eq!(out[1].process, None);
    }

    #[test]
    fn source_is_reported_without_provenance() {
        // node's parent is launchd (pid 1): source = launchd.
        let procs = vec![proc(1, None, "launchd"), proc(10, Some(1), "node")];
        let out = collect(&[port(3000, 10)], &procs, None);
        assert_eq!(out[0].source.as_deref(), Some("launchd"));
        assert!(out[0].owner_agent.is_none());
    }

    #[test]
    fn plain_renders_session_and_source_rows() {
        let entries = vec![
            PortEntry {
                address: "127.0.0.1".into(),
                port: 3000,
                pid: 10,
                process: Some("node".into()),
                owner_agent: Some("opencode".into()),
                owner_session: Some("abc123".into()),
                owner_state: Some("active".into()),
                source: None,
            },
            PortEntry {
                address: "0.0.0.0".into(),
                port: 5001,
                pid: 11,
                process: Some("postgres".into()),
                owner_agent: None,
                owner_session: None,
                owner_state: None,
                source: Some("systemd (postgresql.service)".into()),
            },
        ];
        let text = render_plain(&entries);
        assert!(text.contains("127.0.0.1:3000  node (10)  session opencode (active)"));
        assert!(text.contains("0.0.0.0:5001  postgres (11)  systemd (postgresql.service)"));
    }

    #[test]
    fn plain_empty_is_none() {
        assert_eq!(render_plain(&[]), "none");
    }

    #[test]
    fn ipv6_is_bracketed_in_plain() {
        let entries = vec![PortEntry {
            address: "::".into(),
            port: 3306,
            pid: 1,
            process: Some("db".into()),
            owner_agent: None,
            owner_session: None,
            owner_state: None,
            source: Some("launchd".into()),
        }];
        assert!(render_plain(&entries).contains("[::]:3306  db (1)  launchd"));
    }

    /// End-to-end owner attribution: an agent session owns the listener, so
    /// `owner_*` is set and `source` is left empty.
    #[test]
    fn owner_session_is_reported() {
        use crate::classify::{group, ownership::derive_ownership};
        use crate::model::process::ProcessIdentity;
        use std::collections::HashMap;

        fn iproc(pid: u32, ppid: Option<u32>, name: &str, cmd: &[&str], start: u64) -> ProcessInfo {
            ProcessInfo {
                pid,
                parent_pid: ppid,
                name: name.into(),
                command: cmd.iter().map(|s| s.to_string()).collect(),
                executable: None,
                cwd: None,
                cpu_percent: 0.0,
                memory_bytes: 0,
                start_time: start,
                tty: None,
            }
        }

        let boot = BootId::from_u128(7);
        let procs = vec![
            iproc(1, None, "launchd", &["launchd"], 1),
            iproc(100, Some(1), "omp", &["omp"], 1000),
            iproc(
                110,
                Some(100),
                "node",
                &["node", "chrome-devtools-mcp"],
                1004,
            ),
        ];
        let identities: HashMap<u32, ProcessIdentity> = procs
            .iter()
            .filter_map(|p| ProcessIdentity::from_process(&boot, p).map(|id| (p.pid, id)))
            .collect();
        let out = derive_ownership(&group(&procs), &identities, 5000);

        let mut store = RuntimeStore::open_in_memory().unwrap();
        store.apply_ownership(&out, 5000).unwrap();

        let prov = Provenance { store, boot };
        let ports = [ListeningPort {
            protocol: crate::model::Protocol::Tcp,
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 3000,
            pid: 110,
        }];
        let entries = collect(&ports, &procs, Some(&prov));
        assert_eq!(entries[0].owner_agent.as_deref(), Some("omp"));
        assert_eq!(entries[0].owner_state.as_deref(), Some("active"));
        assert_eq!(entries[0].process.as_deref(), Some("node"));
        assert!(entries[0].source.is_none());
    }
}
