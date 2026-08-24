mod actions;
mod classify;
mod collect;
mod config;
mod model;
mod output;
mod platform;
mod scanner;
mod tui;

use std::io;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use clap::Parser;
use parking_lot::RwLock;

use classify::{ProjectCache, attach, group};
use model::RuntimeSnapshot;
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
}

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Background process scanner: the TUI never scans directly, it reads the
/// latest snapshot. Sending on `force` triggers an immediate rescan (`r`).
/// CPU usage needs two refreshes to produce deltas, so the scanner keeps
/// its `System` alive for the process lifetime.
fn scanner_loop(snapshot: Arc<RwLock<RuntimeSnapshot>>, force: mpsc::Receiver<()>) {
    let mut scanner = SysinfoProcessScanner::new();
    let mut projects = ProjectCache::with_roots(config::Config::global().project_roots());
    let mut docker = model::DockerSnapshot::default();
    let mut version = 0u64;
    loop {
        let next = (|| -> scanner::Result<RuntimeSnapshot> {
            let processes = scanner.scan()?;
            let ports = scanner::ports::scan().unwrap_or_default();
            let mut logical_items = group(&processes);
            attach(&mut logical_items, &processes, &ports, &mut projects);
            classify::mark(&mut logical_items, &processes, config::Config::global());
            version += 1;
            if version == 1 || version.is_multiple_of(3) {
                docker = crate::scanner::docker::scan_blocking();
            }
            let (used, total) = scanner.memory();
            Ok(RuntimeSnapshot {
                logical_items,
                processes,
                docker: docker.clone(),
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
    if cli.json || cli.plain {
        return run_cli(cli);
    }
    let snapshot = Arc::new(RwLock::new(RuntimeSnapshot::default()));
    let (force_tx, force_rx) = mpsc::channel::<()>();
    thread::spawn({
        let snapshot = Arc::clone(&snapshot);
        move || scanner_loop(snapshot, force_rx)
    });
    tui::run_tui(snapshot, force_tx)
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
