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
use parking_lot::RwLock;
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

fn run_cli(cli: Cli) -> io::Result<()> {
    if cli.json && cli.plain {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "use only one of --json or --plain",
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
    let snap = collect::snapshot();
    let project = cli.name.as_deref();
    let text = if cli.json {
        output::render_json(&snap, filter, project)
    } else {
        output::render_plain(&snap, filter, project)
    };
    println!("{text}");
    Ok(())
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
