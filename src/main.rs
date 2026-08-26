mod actions;
mod classify;
mod collect;
mod config;
mod model;
mod output;
mod platform;
mod scanner;
mod store;
mod tui;

use std::io;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use clap::Parser;
use classify::{ProjectCache, attach, group};
use model::RuntimeSnapshot;
use model::process::ProcessIdentity;
use parking_lot::RwLock;
use platform::BootIdentityProvider;
use scanner::{ProcessScanner, processes::SysinfoProcessScanner};

/// See what your dev sessions left running.
#[derive(Parser)]
#[command(name = "wyd", version, about)]
struct Cli {
    /// Print JSON and exit (no TUI)
    #[arg(long)]
    json: bool,
    /// Print one item per line and exit (no TUI)
    #[arg(long)]
    plain: bool,
    /// leftovers | mcp | agents | docker | project
    filter: Option<String>,
    /// Project name when filter is `project`
    name: Option<String>,
    #[command(subcommand)]
    command: Option<Subcmd>,
}

#[derive(clap::Subcommand)]
enum Subcmd {
    /// Update wyd via brew or cargo (detected from the binary path)
    Upgrade,
    /// Delete unused anonymous volumes
    Prune {
        /// List what would be deleted without deleting anything
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Explain which session owns a process (from recorded provenance)
    Why { pid: u32 },
}

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Background process scanner: the TUI never scans directly, it reads the
/// latest snapshot. Sending on `force` triggers an immediate rescan (`r`).
/// CPU usage needs two refreshes to produce deltas, so the scanner keeps
/// its `System` alive for the process lifetime.
fn scanner_loop(snapshot: Arc<RwLock<RuntimeSnapshot>>, force: mpsc::Receiver<()>) {
    let mut scanner = SysinfoProcessScanner::new();
    let mut projects = ProjectCache::with_roots(config::Config::global().project_roots());
    let mut docker = Arc::new(model::DockerSnapshot::default());
    let mut version = 0u64;
    let mut tracker = collect::OwnershipTracker::new();
    loop {
        let next = (|| -> scanner::Result<RuntimeSnapshot> {
            let processes = scanner.scan()?;
            let ports = scanner::ports::scan().unwrap_or_default();
            let mut logical_items = group(&processes);
            attach(&mut logical_items, &processes, &ports, &mut projects);
            classify::mark(&mut logical_items, &processes, config::Config::global());
            tracker.record(&processes, &logical_items);
            tracker.layer_session_leftovers(&mut logical_items, &processes);
            version += 1;
            if version == 1 || version.is_multiple_of(3) {
                docker = Arc::new(crate::scanner::docker::scan_blocking());
            }
            let (used, total) = scanner.memory();
            Ok(RuntimeSnapshot {
                logical_items,
                processes,
                docker: Arc::clone(&docker),
                total_memory_bytes: total,
                used_memory_bytes: used,
                cpu_percent: scanner.cpu_percent(),
                version,
            })
        })();
        // Scanner failures degrade the UI, never crash it: keep the last
        // good snapshot on error.
        if let Ok(snap) = next {
            *snapshot.write() = snap;
        }

        if force
            .recv_timeout(REFRESH_INTERVAL)
            .is_err_and(|e| e == mpsc::RecvTimeoutError::Disconnected)
        {
            return; // TUI exited.
        }
    }
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Subcmd::Upgrade) => run_upgrade(),
        Some(Subcmd::Prune { dry_run, yes }) => run_prune(dry_run, yes),
        Some(Subcmd::Why { pid }) => run_why(pid),
        None => {
            if cli.json || cli.plain {
                run_cli(cli)
            } else {
                let snapshot = Arc::new(RwLock::new(RuntimeSnapshot::default()));
                let (force_tx, force_rx) = mpsc::channel::<()>();
                thread::spawn({
                    let snapshot = Arc::clone(&snapshot);
                    move || scanner_loop(snapshot, force_rx)
                });
                tui::run_tui(snapshot, force_tx)
            }
        }
    }
}

fn run_upgrade() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let resolved = std::fs::canonicalize(&exe).unwrap_or(exe);
    let (cmd, args) = updater_for(&resolved).ok_or_else(|| {
        io::Error::other("unknown install; try:\n  brew upgrade wyd\n  cargo install wyd")
    })?;
    eprintln!("+ {cmd} {}", args.join(" "));
    let status = Command::new(cmd).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{cmd} exited {status}")))
    }
}

fn updater_for(exe: &Path) -> Option<(&'static str, &'static [&'static str])> {
    let p = exe.to_string_lossy();
    if p.contains("Cellar/wyd") {
        Some(("brew", &["upgrade", "wyd"]))
    } else if p.contains("/.cargo/") || p.contains("/cargo/bin/") {
        Some(("cargo", &["install", "wyd"]))
    } else {
        None
    }
}

fn run_prune(dry_run: bool, yes: bool) -> io::Result<()> {
    use std::io::Write;

    let snap = collect::snapshot();
    if !snap.docker.ok {
        println!("docker not running");
        return Ok(());
    }
    let (count, bytes) = snap.docker.prunable_stats();
    if count == 0 {
        println!("nothing to prune");
        return Ok(());
    }
    println!("{count} anonymous volumes · {}", mb(bytes));
    if dry_run {
        return Ok(());
    }
    if !yes {
        print!("delete? [y/N] ");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        if !line.trim().eq_ignore_ascii_case("y") {
            println!("aborted");
            return Ok(());
        }
    }
    let ids = snap.docker.prunable_ids();
    let (deleted, _) =
        actions::docker::prune_anonymous_volumes_blocking(&ids).map_err(io::Error::other)?;
    println!("pruned {deleted} volumes");
    Ok(())
}

fn mb(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1}G", bytes as f64 / (1 << 30) as f64)
    } else {
        format!("{}M", bytes / (1 << 20))
    }
}

/// `wyd --json sessions`: list recorded sessions from the store.
fn run_sessions_json() -> io::Result<()> {
    let store = store::RuntimeStore::open(&store::RuntimeStore::default_path())?;
    let sessions = store.sessions()?;
    let arr: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id.to_string(),
                "agent": s.agent,
                "project": s.project,
                "state": if s.ended_at.is_some() { "ended" } else { "active" },
                "started_at": s.started_at,
                "last_seen_at": s.last_seen_at,
                "ended_at": s.ended_at,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&arr).unwrap());
    Ok(())
}

/// `wyd why <pid>`: reconstruct a process's origin session and attribution
/// from durable provenance (contract §15).
fn run_why(pid: u32) -> io::Result<()> {
    let mut store = store::RuntimeStore::open(&store::RuntimeStore::default_path())?;
    let now = now();
    let boot = store.boot_id_for_epoch(platform::SystemBoot.current_boot_epoch()?, now)?;

    // Resolve start_time from the live process.
    let mut scanner = SysinfoProcessScanner::new();
    let processes = scanner
        .scan()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let Some(proc) = processes.iter().find(|p| p.pid == pid) else {
        return Err(io::Error::other(format!("pid {pid} is not running")));
    };
    let Some(identity) = ProcessIdentity::from_process(&boot, proc) else {
        return Err(io::Error::other(format!(
            "pid {pid} has no stable identity (start_time unavailable)"
        )));
    };

    println!("{} pid {pid}", proc.label());
    match store.explain_process(&boot, pid, identity.start_time)? {
        Some(exp) => {
            print_session_owner(&store, &exp);
        }
        None => {
            // Maybe the pid IS a session root.
            match store.session_for_root(&boot, pid, identity.start_time)? {
                Some(s) => println!(
                    "session root of: {} {} ({} since {})",
                    s.agent,
                    s.id,
                    if s.ended_at.is_some() {
                        "ended"
                    } else {
                        "active"
                    },
                    s.started_at
                ),
                None => println!("pid {pid}: no recorded owner"),
            }
        }
    }
    Ok(())
}

fn print_session_owner(store: &store::RuntimeStore, exp: &store::Explanation) {
    let s = &exp.session;
    println!("origin session: {} {}", s.agent, s.id);
    if let Some(p) = &s.project {
        println!("project:        {p}");
    }
    match s.ended_at {
        Some(e) => println!("session:        ended at {e}"),
        None => println!("session:        active (since {})", s.started_at),
    }

    if let Ok(Some(d)) = store.latest_decision(exp.resource_id) {
        println!(
            "attribution:    {} (resolver v{})",
            d.verdict, d.resolver_version
        );
        if let Some(w) = d.winner_session {
            println!("winner:         session {w}");
        }
        for c in &d.candidates {
            if c.rejected_reason.is_some() {
                continue;
            }
            println!(
                "  candidate {}: anchor {} {}{}{}{} = {}",
                c.session,
                c.anchor_kind,
                c.anchor_score,
                sign(c.project_support),
                sign(c.temporal_support),
                sign(c.relationship_support),
                c.total,
            );
        }
    }
}

fn sign(v: u8) -> String {
    if v == 0 {
        String::new()
    } else {
        format!(" +{v}")
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn run_cli(cli: Cli) -> io::Result<()> {
    if cli.json && cli.plain {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "use only one of --json or --plain",
        ));
    }
    if cli
        .filter
        .as_deref()
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
        == Some("sessions")
    {
        if cli.json {
            return run_sessions_json();
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "`sessions` requires --json",
        ));
    }
    let filter = output::Filter::parse(cli.filter.as_deref())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    if filter == output::Filter::Project && cli.name.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "filter `project` needs a name: wyd --json project myapp",
        ));
    }
    let snap = session_aware_snapshot(collect::snapshot());
    let project = cli.name.as_deref();
    let text = if cli.json {
        output::render_json(&snap, filter, project)
    } else {
        output::render_plain(&snap, filter, project)
    };
    println!("{text}");
    Ok(())
}

/// Layer session-ended leftover marks onto a CLI snapshot (mirrors the TUI
/// tracker). Falls back to the unmodified snapshot when the store is absent.
fn session_aware_snapshot(mut snap: model::RuntimeSnapshot) -> model::RuntimeSnapshot {
    let Ok(mut store) = store::RuntimeStore::open(&store::RuntimeStore::default_path()) else {
        return snap;
    };
    let now = now();
    let Ok(epoch) = platform::SystemBoot.current_boot_epoch() else {
        return snap;
    };
    let Ok(boot) = store.boot_id_for_epoch(epoch, now) else {
        return snap;
    };
    collect::apply_session_leftovers(&mut snap.logical_items, &snap.processes, &store, &boot);
    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updater_detects_brew_and_cargo() {
        assert_eq!(
            updater_for(Path::new("/opt/homebrew/Cellar/wyd/0.4.1/bin/wyd")).map(|(c, _)| c),
            Some("brew")
        );
        assert_eq!(
            updater_for(Path::new("/Users/x/.cargo/bin/wyd")).map(|(c, _)| c),
            Some("cargo")
        );
        assert!(updater_for(Path::new("/usr/local/bin/wyd")).is_none());
    }
}
