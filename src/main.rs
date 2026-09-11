mod actions;
mod barman;
mod classify;
mod collect;
mod config;
mod demo;
mod mcp;
mod model;
mod output;
mod platform;
mod ports;
mod runner;
mod scanner;
mod server;
mod source;
mod store;
mod trace;
mod tui;
mod web;
use std::io;
use std::path::{Path, PathBuf};
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
use serde_json::{Value, json};

/// See what your dev sessions left running.
#[derive(Parser)]
#[command(name = "wyd", version, about, subcommand_negates_reqs = true)]
struct Cli {
    /// Print JSON and exit (no TUI)
    #[arg(long)]
    json: bool,
    /// Print one item per line and exit (no TUI)
    #[arg(long)]
    plain: bool,
    /// Deterministic synthetic dataset — no host scan (screenshots, demos)
    #[arg(long)]
    demo: bool,
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
    Why {
        pid: u32,
        /// Show the full ancestry tree (path + children) instead of the narrative
        #[arg(long)]
        tree: bool,
    },
    /// List listening ports with the process on each and what started it
    Ports {
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Serve the local runtime API and run supervisor (read + runs)
    Serve {
        /// Internal: started on demand by a client; exits when idle
        #[arg(long, hide = true)]
        auto: bool,
    },
    /// Run a command under wyd's supervisor and print its result
    Run {
        /// Execution timeout, e.g. 120s, 10m
        #[arg(long, default_value = "10m", value_parser = parse_duration)]
        timeout: Duration,
        /// Grace period between soft and forced stop
        #[arg(long, default_value = "3s", value_parser = parse_duration)]
        grace: Duration,
        /// Absolute working directory (default: current directory)
        #[arg(long)]
        cwd: Option<String>,
        /// Reuse an existing run instead of starting a new one
        #[arg(long)]
        request_id: Option<String>,
        /// Memory in MiB: reserved against the run budget, and the stop
        /// threshold when --enforce monitored
        #[arg(long)]
        memory: Option<u64>,
        /// none | monitored | hard. `hard` is refused when the backend cannot
        /// enforce it; it is never silently downgraded.
        #[arg(long, default_value = "none")]
        enforce: String,
        /// How long the request may wait for a slot (separate from --timeout)
        #[arg(long, value_parser = parse_duration)]
        queue_timeout: Option<Duration>,
        /// CPU in millicores (advisory until a Linux backend enforces it)
        #[arg(long)]
        cpu: Option<u32>,
        /// Process-count request (advisory until a Linux backend enforces it)
        #[arg(long)]
        processes: Option<u32>,
        /// Print the structured result instead of the captured output
        #[arg(long)]
        json: bool,
        /// Command and arguments. Shell syntax needs an explicit `sh -c`.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// List managed runs
    Runs {
        /// Only runs in this state: queued|starting|running|stopping|finished
        #[arg(long)]
        state: Option<String>,
        /// Only runs in this project root
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Print a run's captured output
    Logs {
        run_id: i64,
        /// stdout | stderr
        #[arg(long, default_value = "stdout")]
        stream: String,
        /// Keep printing new output as it arrives
        #[arg(long)]
        follow: bool,
    },
    /// Ask a run to stop (idempotent)
    Cancel { run_id: i64 },
    /// Show managed-run admission capacity; with --set, change the limits
    Capacity {
        #[arg(long)]
        json: bool,
        /// Change a limit, e.g. --set max_parallel=2. Repeatable. Active runs
        /// keep running; the new limits apply to admission from now on.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
    },
    /// Run an MCP server over stdio (for coding agents)
    Mcp {
        /// Enable the execution tools (start_run, cancel_run) for this
        /// connection. Off by default: MCP stays read-only.
        #[arg(long)]
        allow_run: bool,
    },
    /// Serve a loopback web dashboard with WebMCP tools
    Web {
        /// Bind address (default: 127.0.0.1)
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Bind port (default: 8732)
        #[arg(long, default_value_t = 8732)]
        port: u16,
        /// Serve deterministic synthetic data instead of the host runtime
        #[arg(long)]
        demo: bool,
        /// Allow binding to a non-loopback address (LAN exposure; discouraged)
        #[arg(long)]
        allow_lan: bool,
    },
    /// Machine-readable JSON API for the wyd-barman menu-bar client (v1)
    Barman {
        #[command(subcommand)]
        cmd: barman::BarmanCmd,
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
                sessions: tracker.sessions(),
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
        Some(Subcmd::Why { pid, tree }) => run_why(pid, tree),
        Some(Subcmd::Ports { json }) => run_ports(json),
        Some(Subcmd::Serve { auto }) => server::serve(auto),
        Some(Subcmd::Run {
            timeout,
            grace,
            cwd,
            request_id,
            memory,
            enforce,
            queue_timeout,
            cpu,
            processes,
            json,
            argv,
        }) => run_command(RunOptions {
            timeout,
            grace,
            cwd,
            request_id,
            memory,
            enforce,
            queue_timeout,
            cpu,
            processes,
            json,
            argv,
        }),
        Some(Subcmd::Runs {
            state,
            project,
            limit,
            json,
        }) => run_runs(state, project, limit, json),
        Some(Subcmd::Logs {
            run_id,
            stream,
            follow,
        }) => run_logs(run_id, stream, follow),
        Some(Subcmd::Cancel { run_id }) => run_cancel(run_id),
        Some(Subcmd::Capacity { json, set }) => run_capacity(json, set),
        Some(Subcmd::Mcp { allow_run }) => mcp::serve_stdio(allow_run),
        Some(Subcmd::Web {
            host,
            port,
            demo,
            allow_lan,
        }) => web::serve(web::WebOptions {
            host,
            port,
            demo,
            allow_lan,
        }),
        Some(Subcmd::Barman { cmd }) => barman::run(cmd),
        None => {
            if cli.json || cli.plain {
                run_cli(cli)
            } else if cli.demo {
                // Deterministic dataset for screenshots: no scanner thread,
                // static snapshot, `r` just redraws.
                let snapshot = Arc::new(RwLock::new(demo::snapshot()));
                let (force_tx, _force_rx) = mpsc::channel::<()>();
                tui::run_tui(snapshot, force_tx)
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
/// Parse `120s` / `10m` / `1h` / plain seconds.
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, unit) = match s.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => (&s[..s.len() - 1], c.to_ascii_lowercase()),
        _ => (s, 's'),
    };
    let value: u64 = num
        .parse()
        .map_err(|_| format!("{s:?} is not a duration like 120s, 10m, 1h"))?;
    let secs = match unit {
        's' => value,
        'm' => value * 60,
        'h' => value * 3600,
        _ => return Err(format!("unknown duration unit {unit:?}")),
    };
    Ok(Duration::from_secs(secs.max(1)))
}

/// Everything `wyd run` accepts.
struct RunOptions {
    timeout: Duration,
    grace: Duration,
    cwd: Option<String>,
    request_id: Option<String>,
    memory: Option<u64>,
    enforce: String,
    queue_timeout: Option<Duration>,
    cpu: Option<u32>,
    processes: Option<u32>,
    json: bool,
    argv: Vec<String>,
}

/// `wyd run`: start under the supervisor, wait, print the result.
fn run_command(opts: RunOptions) -> io::Result<()> {
    use model::run::{Enforcement, RunSpec, exit};
    let RunOptions {
        timeout,
        grace,
        cwd,
        request_id,
        memory,
        enforce,
        queue_timeout,
        cpu,
        processes,
        json,
        argv,
    } = opts;
    runner::client::ensure_supervisor()?;
    let client = runner::client::Client::new();

    let cwd = match cwd {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir()?,
    };
    if !cwd.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--cwd must be an absolute path",
        ));
    }
    // A CLI invocation is a fresh request; the id only exists so a retried
    // call can be deduplicated by a caller that supplies its own.
    let request_id = request_id.unwrap_or_else(|| {
        format!(
            "cli-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    });
    let mut spec = RunSpec::new(request_id, argv, cwd);
    spec.timeout = timeout;
    spec.grace = grace;
    spec.resources.memory_bytes = memory.map(|mb| mb.saturating_mul(1024 * 1024));
    spec.resources.enforcement = match enforce.as_str() {
        "none" => Enforcement::None,
        "monitored" => Enforcement::Monitored,
        "hard" => Enforcement::Hard,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--enforce must be none|monitored|hard, got {other:?}"),
            ));
        }
    };
    spec.resources.queue_timeout = queue_timeout;
    spec.resources.cpu_millicores = cpu;
    spec.resources.processes = processes;

    let env: Vec<(String, String)> = std::env::vars().collect();
    let started = client.start(&spec, &env)?;
    let id: i64 = started
        .run_id
        .parse()
        .map_err(|_| io::Error::other("bad run id from supervisor"))?;
    let view = runner::client::wait_for_finish(&client, model::run::RunId(id))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
    } else {
        print_run_output(&client, model::run::RunId(id), false)?;
        if let Some(detail) = &view.detail {
            eprintln!("wyd: {detail}");
        }
        eprintln!(
            "wyd: run {id} {:?} cleanup={} in {}ms",
            view.outcome.unwrap_or(model::run::RunOutcome::Exited),
            view.cleanup.as_str(),
            view.duration_ms.unwrap_or(0)
        );
    }

    let code = match view.outcome {
        Some(model::run::RunOutcome::Exited) => view.exit_code.unwrap_or(0),
        Some(model::run::RunOutcome::Signaled) => 128 + view.signal.unwrap_or(0),
        Some(model::run::RunOutcome::TimedOut) => exit::TIMED_OUT,
        Some(model::run::RunOutcome::Cancelled) => exit::CANCELLED,
        Some(model::run::RunOutcome::ResourceLimit) => exit::RESOURCE_LIMIT,
        _ => exit::SPAWN_FAILED,
    };
    std::process::exit(code);
}

/// Copy a run's retained output to our own stdout/stderr.
fn print_run_output(
    client: &runner::client::Client,
    id: model::run::RunId,
    follow: bool,
) -> io::Result<()> {
    use runner::logs::Stream;
    let mut cursors = [(Stream::Stdout, 0u64), (Stream::Stderr, 0u64)];
    let mut open = [true, true];
    while open[0] || open[1] {
        for (idx, (stream, cursor)) in cursors.iter_mut().enumerate() {
            if !open[idx] {
                continue;
            }
            let chunk = client.output(id, *stream, *cursor, 64 * 1024)?;
            let data = chunk.get("data").and_then(Value::as_str).unwrap_or("");
            if !data.is_empty() {
                match stream {
                    Stream::Stdout => print!("{data}"),
                    Stream::Stderr => eprint!("{data}"),
                }
                io::Write::flush(&mut io::stdout())?;
            }
            *cursor = chunk
                .get("next_cursor")
                .and_then(Value::as_u64)
                .unwrap_or(*cursor);
            let eof = chunk.get("eof").and_then(Value::as_bool).unwrap_or(true);
            if eof {
                open[idx] = false;
                if chunk.get("truncated").and_then(Value::as_bool) == Some(true) {
                    eprintln!(
                        "wyd: {} output truncated at the configured log limit",
                        stream.as_str()
                    );
                }
            }
        }
        if open[0] || open[1] {
            if !follow {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    Ok(())
}

fn run_runs(
    state: Option<String>,
    project: Option<String>,
    limit: usize,
    json: bool,
) -> io::Result<()> {
    // Reads come from the durable store, so `wyd runs` works whether or not a
    // supervisor is alive.
    let store = store::RuntimeStore::open(&store::RuntimeStore::default_path())?;
    let filter = store::RunFilter {
        project,
        session: None,
        state: state.as_deref().and_then(model::run::RunState::parse),
        limit: Some(limit),
    };
    let runs: Vec<runner::RunView> = store
        .run_list(&filter)?
        .iter()
        .map(runner::RunView::from_record)
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
        return Ok(());
    }
    if runs.is_empty() {
        println!("no runs");
        return Ok(());
    }
    for run in runs {
        let cmd = run.argv.join(" ");
        println!(
            "{:>4}  {:<9} {:<13} {:>8}  {}",
            run.run_id,
            run.state.as_str(),
            run.outcome.map(|o| o.as_str()).unwrap_or("-"),
            format!("{}ms", run.duration_ms.unwrap_or(0)),
            truncate(&cmd, 70),
        );
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn run_logs(run_id: i64, stream: String, follow: bool) -> io::Result<()> {
    use runner::logs::Stream;
    let stream = Stream::parse(&stream)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "stream: stdout | stderr"))?;
    let id = model::run::RunId(run_id);
    let store = store::RuntimeStore::open(&store::RuntimeStore::default_path())?;
    let Some(_record) = store.run_get(id)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("run {run_id} not found"),
        ));
    };
    // Retained output lives on disk; the supervisor is only needed to start
    // or stop a run, never to read one back.
    let paths =
        runner::logs::RunPaths::new(store::RuntimeStore::default_path().with_file_name("runs"));
    let path = paths.stream_file(run_id, stream);
    let mut cursor = 0u64;
    loop {
        let chunk = runner::logs::read_chunk(&path, cursor, 64 * 1024)?;
        match stream {
            Stream::Stdout => print!("{}", chunk.data),
            Stream::Stderr => eprint!("{}", chunk.data),
        }
        io::Write::flush(&mut io::stdout())?;
        cursor = chunk.next_cursor;
        let finished = store
            .run_get(id)?
            .map(|r| r.state.is_terminal())
            .unwrap_or(true);
        if chunk.eof && finished {
            let logs = store.run_get(id)?.map(|r| r.logs).unwrap_or_default();
            let truncated = match stream {
                Stream::Stdout => logs.stdout_truncated,
                Stream::Stderr => logs.stderr_truncated,
            };
            if truncated {
                eprintln!(
                    "wyd: {} output truncated at the configured log limit",
                    stream.as_str()
                );
            }
            return Ok(());
        }
        if chunk.eof && !follow {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `wyd capacity`: admission limits and current usage. Reads the supervisor
/// when one is running; otherwise reports the configured limits and an empty
/// machine, without starting a daemon for a read.
fn run_capacity(json: bool, set: Vec<String>) -> io::Result<()> {
    use model::run::{Capacity, LimitSummary, backend_capabilities};
    if !set.is_empty() {
        let mut limits = serde_json::Map::new();
        for item in &set {
            let (key, value) = item.split_once('=').ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("--set expects KEY=VALUE, got {item:?}"),
                )
            })?;
            let value: u64 = value.parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("--set {key}: {value:?} is not a number"),
                )
            })?;
            match key {
                // The CLI takes MiB; the API takes bytes. The key must change
                // with the unit, or the server silently ignores it.
                "memory_budget_mb" => {
                    limits.insert(
                        "memory_budget_bytes".into(),
                        json!(value.saturating_mul(1024 * 1024)),
                    );
                }
                "default_run_memory_mb" => {
                    limits.insert(
                        "default_run_memory_bytes".into(),
                        json!(value.saturating_mul(1024 * 1024)),
                    );
                }
                "max_parallel"
                | "max_parallel_per_project"
                | "max_queued"
                | "queue_timeout_secs"
                | "starvation_after_secs"
                | "cpu_budget_millicores"
                | "pids_budget" => {
                    limits.insert(key.into(), json!(value));
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown limit {other:?}"),
                    ));
                }
            }
        }
        runner::client::ensure_supervisor()?;
        runner::client::Client::new().set_limits(Value::Object(limits))?;
    }
    let capacity = if server::serve_alive() {
        runner::client::Client::new().capacity()?
    } else {
        let limits = config::Config::global().runs.limits().summary();
        Capacity {
            limits,
            running: 0,
            queued: 0,
            slots_free: config::Config::global().runs.max_parallel.max(1),
            reserved_memory_bytes: 0,
            projects: Vec::new(),
            queue: Vec::new(),
            over_parallel_limit: false,
            aggregate_memory_max_bytes: None,
            aggregate_cpu_millicores: None,
            aggregate_pids_max: None,
            reservation_note: "no supervisor running; these are the configured limits".into(),
            capabilities: backend_capabilities(),
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&capacity)?);
        return Ok(());
    }
    let LimitSummary {
        max_parallel,
        max_parallel_per_project,
        max_queued,
        queue_timeout_secs,
        memory_budget_bytes,
        default_run_memory_bytes,
        ..
    } = capacity.limits;
    println!(
        "slots      {}/{} used ({} per project)",
        capacity.running, max_parallel, max_parallel_per_project
    );
    println!(
        "queue      {} waiting, {} max, {}s timeout",
        capacity.queued, max_queued, queue_timeout_secs
    );
    println!(
        "reserved   {} of {} MiB (default {} MiB per run)",
        capacity.reserved_memory_bytes / (1024 * 1024),
        memory_budget_bytes / (1024 * 1024),
        default_run_memory_bytes / (1024 * 1024),
    );
    if let Some(millicores) = capacity.aggregate_cpu_millicores {
        println!("kernel cpu  {} millicores across all runs", millicores);
    }
    if let Some(pids) = capacity.aggregate_pids_max {
        println!("kernel pids {} across all runs", pids);
    }
    if capacity.over_parallel_limit {
        println!(
            "warning    {} run(s) still active above the new limit; they are not killed",
            capacity.running
        );
    }
    println!("note       {}", capacity.reservation_note);
    for entry in &capacity.queue {
        println!(
            "  queued   #{} run {} {} ({}s)",
            entry.position,
            entry.run_id,
            entry.reason,
            entry.waiting_ms / 1000
        );
    }
    Ok(())
}

fn run_cancel(run_id: i64) -> io::Result<()> {
    let client = runner::client::Client::new();
    let view = client.cancel(model::run::RunId(run_id))?;
    match view {
        Some(view) => {
            println!("run {} {}", view.run_id, view.state.as_str());
            Ok(())
        }
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("run {run_id} not found"),
        )),
    }
}

fn run_upgrade() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let resolved = std::fs::canonicalize(&exe).unwrap_or(exe);
    let (cmd, args) = updater_for(&resolved).ok_or_else(|| {
        io::Error::other("unknown install; try:\n  brew upgrade wyd\n  cargo install wyd")
    })?;
    eprintln!("+ {cmd} {}", args.join(" "));
    if cmd == "brew" {
        return run_brew_upgrade();
    }
    let status = Command::new(cmd).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{cmd} exited {status}")))
    }
}

/// `brew upgrade` silently no-ops when the tap formula has no newer version
/// than what is installed. A new wyd release must bump `Formula/wyd.rb` in
/// oxyplay/homebrew-tap first, or brew users never receive it — so a no-op is
/// reported honestly instead of looking like a successful update.
fn run_brew_upgrade() -> io::Result<()> {
    let out = Command::new("brew").args(["upgrade", "wyd"]).output()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    eprint!("{text}");
    if !out.status.success() {
        return Err(io::Error::other("brew upgrade exited with an error"));
    }
    if text.contains("already installed") || text.contains("already up-to-date") {
        println!(
            "no newer version from the tap — either you are current, or the\n\
             homebrew formula (oxyplay/tap) has not been bumped for this release"
        );
    }
    Ok(())
}

fn updater_for(exe: &Path) -> Option<(&'static str, &'static [&'static str])> {
    let p = exe.to_string_lossy();
    if p.contains("Cellar/wyd") {
        Some(("brew", &["upgrade", "wyd"]))
    } else if p.contains("/.cargo/") || p.contains("/cargo/bin/") {
        Some(("cargo", &["install", "wyd"]))
    } else if p.contains("/.local/bin/") {
        // curl-installed (https://wyd.sh/install.sh): re-run the installer —
        // it detects the platform, verifies the checksum, and replaces in place.
        Some(("sh", &["-c", "curl -fsSL https://wyd.sh/install.sh | sh"]))
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
/// from durable provenance (contract §15), falling back to system-source
/// detection (systemd/launchd/cron/tmux/ssh/…) when no agent session owns it.
///
/// Exit codes: 0 = cleanly owned (session active), 1 = warning (owner ended
/// or no recorded owner), 2 = pid not running or not identifiable,
/// 5 = internal error. `--tree` prints the ancestry tree and exits 0/2.
fn run_why(pid: u32, tree: bool) -> io::Result<()> {
    let code = match why_inner(pid, tree) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wyd why: {e}");
            std::process::exit(5);
        }
    };
    std::process::exit(code);
}

fn why_inner(pid: u32, tree: bool) -> io::Result<i32> {
    let mut scanner = SysinfoProcessScanner::new();
    let processes = scanner
        .scan()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let Some(proc) = processes.iter().find(|p| p.pid == pid) else {
        eprintln!("pid {pid} is not running");
        return Ok(2);
    };

    // `--tree` is a pure structure view: no provenance needed.
    if tree {
        print!("{}", source::render_tree(pid, &processes));
        return Ok(0);
    }

    let mut store = store::RuntimeStore::open(&store::RuntimeStore::default_path())?;
    let now = now();
    let boot = store.boot_id_for_epoch(platform::SystemBoot.current_boot_epoch()?, now)?;

    // Resolve start_time from the live process.
    let Some(identity) = ProcessIdentity::from_process(&boot, proc) else {
        eprintln!("pid {pid} has no stable identity (start_time unavailable)");
        return Ok(2);
    };

    println!("{} pid {pid}", proc.label());
    match store.explain_process(&boot, pid, identity.start_time)? {
        Some(exp) => {
            print_session_owner(&store, &exp);
            Ok(if exp.session.ended_at.is_some() { 1 } else { 0 })
        }
        None => {
            // Maybe the pid IS a session root.
            match store.session_for_root(&boot, pid, identity.start_time)? {
                Some(s) => {
                    println!(
                        "session root of: {} {} ({} since {})",
                        s.agent,
                        s.id,
                        if s.ended_at.is_some() {
                            "ended"
                        } else {
                            "active"
                        },
                        s.started_at
                    );
                    Ok(if s.ended_at.is_some() { 1 } else { 0 })
                }
                None => {
                    print_source(&processes, proc);
                    Ok(1)
                }
            }
        }
    }
}

/// Fallback for a pid with no recorded owner: name the system source it
/// still descends from, with the ancestry chain as evidence.
fn print_source(processes: &[model::ProcessInfo], target: &model::ProcessInfo) {
    let report = source::detect(target.pid, processes);
    println!("no recorded owner");
    println!("source:   {}", report.source.label());
    let mut parts: Vec<String> = report
        .chain
        .iter()
        .map(|p| format!("{} ({})", p.name, p.pid))
        .collect();
    parts.push(format!("{} ({})", target.name, target.pid));
    println!("ancestry: {}", parts.join(" → "));
}

/// `wyd ports`: list every listening port with the process on it and what
/// started it — the owning agent session, or the system source when no
/// session owns it.
fn run_ports(json: bool) -> io::Result<()> {
    let mut scanner = SysinfoProcessScanner::new();
    let processes = scanner
        .scan()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let listening = scanner::ports::scan().map_err(|e| io::Error::other(e.to_string()))?;

    let prov = ports_provenance();
    let entries = ports::collect(&listening, &processes, prov.as_ref());

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".into())
        );
    } else {
        println!("{}", ports::render_plain(&entries));
    }
    Ok(())
}

/// Best-effort provenance for port owner attribution: `None` when the store
/// or boot identity is unavailable, so `wyd ports` still lists system sources.
fn ports_provenance() -> Option<ports::Provenance> {
    let mut store = store::RuntimeStore::open(&store::RuntimeStore::default_path()).ok()?;
    let epoch = platform::SystemBoot.current_boot_epoch().ok()?;
    let boot = store.boot_id_for_epoch(epoch, now()).ok()?;
    Some(ports::Provenance { store, boot })
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
            for e in &c.evidence {
                println!("    evidence: {} ({})", e.kind.as_str(), e.value);
            }
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
    let snap = if cli.demo {
        demo::snapshot()
    } else {
        session_aware_snapshot(collect::snapshot())
    };
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
        let (cmd, args) = updater_for(Path::new("/Users/x/.local/bin/wyd")).unwrap();
        assert_eq!(cmd, "sh");
        assert!(args[1].contains("wyd.sh/install.sh"));
        assert!(updater_for(Path::new("/usr/local/bin/wyd")).is_none());
    }
}
