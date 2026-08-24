mod classify;
mod model;
mod platform;
mod scanner;

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use parking_lot::RwLock;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

use classify::group;
use model::{Category, ProcessInfo, RuntimeItem, RuntimeSnapshot};
use scanner::{ProcessScanner, processes::SysinfoProcessScanner};

/// See what your dev sessions left running.
#[derive(Parser)]
#[command(name = "wyd", version, about)]
struct Cli {}

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const EVENT_POLL: Duration = Duration::from_millis(100);

/// Background process scanner: the TUI never scans directly, it reads the
/// latest snapshot. Sending on `force` triggers an immediate rescan (`r`).
/// CPU usage needs two refreshes to produce deltas, so the scanner keeps
/// its `System` alive for the process lifetime.
fn scanner_loop(snapshot: Arc<RwLock<RuntimeSnapshot>>, force: mpsc::Receiver<()>) {
    let mut scanner = SysinfoProcessScanner::new();
    let mut version = 0u64;
    loop {
        let next = (|| -> scanner::Result<RuntimeSnapshot> {
            let processes = scanner.scan()?;
            let (used, total) = scanner.memory();
            version += 1;
            Ok(RuntimeSnapshot {
                logical_items: group(&processes),
                processes,
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
    let _cli = Cli::parse();

    let snapshot = Arc::new(RwLock::new(RuntimeSnapshot::default()));
    let (force_tx, force_rx) = mpsc::channel::<()>();
    thread::spawn({
        let snapshot = Arc::clone(&snapshot);
        move || scanner_loop(snapshot, force_rx)
    });

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = run(&mut terminal, &snapshot, &force_tx);
    drop(force_tx);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    result
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    snapshot: &Arc<RwLock<RuntimeSnapshot>>,
    force: &mpsc::Sender<()>,
) -> io::Result<()> {
    let mut drawn_version = u64::MAX; // force first draw
    let mut scroll = 0u16;
    loop {
        let snap = snapshot.read();
        if snap.version != drawn_version {
            drawn_version = snap.version;
            terminal.draw(|f| ui(f, &snap, scroll))?;
        }
        drop(snap);

        if event::poll(EVENT_POLL)? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('r') => {
                        let _ = force.send(());
                    }
                    KeyCode::Up => scroll = scroll.saturating_sub(1),
                    KeyCode::Down => scroll = scroll.saturating_add(1),
                    _ => {}
                }
            }
            terminal.draw(|f| ui(f, &snapshot.read(), scroll))?;
        }
    }
}

fn ui(frame: &mut Frame, snap: &RuntimeSnapshot, scroll: u16) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

    let outer = Block::default()
        .title(format!(
            " wyd ── RAM {}/{} │ CPU {:.0}% │ {} items ",
            fmt_bytes(snap.used_memory_bytes),
            fmt_bytes(snap.total_memory_bytes),
            snap.cpu_percent,
            count_items(&snap.logical_items),
        ))
        .borders(Borders::ALL);
    let inner = outer.inner(main);
    frame.render_widget(outer, main);

    let lines: Vec<Line> = if snap.processes.is_empty() {
        vec![Line::from(" scanning…")]
    } else if snap.logical_items.is_empty() {
        vec![Line::from(" no matching dev processes")]
    } else {
        let mut lines = Vec::new();
        let overview = overview_line(&snap.logical_items);
        if !overview.is_empty() {
            lines.push(Line::from(overview));
            lines.push(Line::from(""));
        }
        let by_pid: HashMap<u32, &ProcessInfo> =
            snap.processes.iter().map(|p| (p.pid, p)).collect();
        render_items(&snap.logical_items, 0, &by_pid, &mut lines);
        lines
    };
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);

    frame.render_widget(
        Paragraph::new(" ↑↓ scroll  r refresh  q quit")
            .style(Style::default().add_modifier(Modifier::DIM)),
        footer,
    );
}

fn render_items(
    items: &[RuntimeItem],
    depth: usize,
    by_pid: &HashMap<u32, &ProcessInfo>,
    out: &mut Vec<Line>,
) {
    let last = items.len().saturating_sub(1);
    for (i, item) in items.iter().enumerate() {
        let indent = "  ".repeat(depth);
        let marker = if depth == 0 {
            "● "
        } else if i == last {
            "└ "
        } else {
            "├ "
        };
        let proc = item.root_pid.and_then(|pid| by_pid.get(&pid).copied());
        let age = proc
            .map(|p| fmt_age(p.start_time))
            .unwrap_or_else(|| "—".into());
        let mut line = format!(
            "{indent}{marker}{:<32} {:>9} {:>5.1}%  {age}",
            truncate(&item.title(), 32),
            fmt_bytes(item.memory_bytes),
            item.cpu_percent,
        );
        if item.state == model::RuntimeState::Persistent {
            line.push_str("  persistent");
        }
        if let Some(p) = proc {
            if let Some(tty) = &p.tty {
                line.push_str("  ");
                line.push_str(tty);
            }
            if depth == 0
                && let Some(cwd) = &p.cwd
            {
                line.push_str("  ");
                line.push_str(&cwd.display().to_string());
            }
        }
        out.push(Line::from(line));
        render_items(&item.children, depth + 1, by_pid, out);
    }
}

fn count_items(items: &[RuntimeItem]) -> usize {
    items.iter().map(|i| 1 + count_items(&i.children)).sum()
}

fn overview_line(items: &[RuntimeItem]) -> String {
    let mut counts: HashMap<Category, u32> = HashMap::new();
    fn add(items: &[RuntimeItem], counts: &mut HashMap<Category, u32>) {
        for item in items {
            *counts.entry(item.category).or_insert(0) += item.process_ids.len() as u32;
            add(&item.children, counts);
        }
    }
    add(items, &mut counts);
    const ORDER: [Category; 8] = [
        Category::Agent,
        Category::Mcp,
        Category::Browser,
        Category::DevServer,
        Category::LanguageServer,
        Category::Database,
        Category::DevService,
        Category::UnknownDev,
    ];
    ORDER
        .iter()
        .filter_map(|c| {
            let n = counts.get(c).copied().unwrap_or(0);
            (n > 0).then(|| format!("{} {n}", c.label()))
        })
        .collect::<Vec<_>>()
        .join("   ")
}

fn fmt_bytes(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    if bytes >= GIB {
        format!("{:.1}G", bytes as f64 / GIB as f64)
    } else {
        format!("{}M", bytes / MIB)
    }
}

fn fmt_age(start_time: u64) -> String {
    if start_time == 0 {
        return "—".into();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let secs = now.saturating_sub(start_time);
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn fixture_snapshot() -> RuntimeSnapshot {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let proc = |pid: u32, ppid: Option<u32>, name: &str, cmd: &[&str], mem: u64| ProcessInfo {
            pid,
            parent_pid: ppid,
            name: name.into(),
            command: cmd.iter().map(|s| (*s).to_string()).collect(),
            executable: None,
            cwd: None,
            cpu_percent: 1.0,
            memory_bytes: mem,
            start_time: now - 120,
            tty: None,
        };
        let processes = vec![
            proc(1, None, "launchd", &["launchd"], 10 << 20),
            proc(100, Some(1), "omp", &["omp"], 300 << 20),
            proc(
                110,
                Some(100),
                "node",
                &["node", "/x/chrome-devtools-mcp/index.js"],
                48 << 20,
            ),
            proc(111, Some(110), "Chromium", &["Chromium"], 200 << 20),
            proc(
                112,
                Some(110),
                "Chromium Helper",
                &["Chromium Helper"],
                80 << 20,
            ),
        ];
        RuntimeSnapshot {
            logical_items: group(&processes),
            processes,
            total_memory_bytes: 32 << 30,
            used_memory_bytes: 7 << 30,
            cpu_percent: 12.0,
            version: 1,
        }
    }

    #[test]
    fn renders_classified_tree_hides_os() {
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let snap = fixture_snapshot();
        terminal.draw(|f| ui(f, &snap, 0)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        for expected in [
            "wyd",
            "RAM 7.0G/32.0G",
            "CPU 12%",
            "3 items",
            "Agents 1",
            "MCP 1",
            "Browsers 2",
            "● omp",
            "└ chrome-devtools-mcp",
            "Chromium ×2",
            "r refresh",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}:\n{rendered}"
            );
        }
        assert!(
            !rendered.contains("launchd"),
            "OS process leaked into default view:\n{rendered}"
        );
    }

    #[test]
    fn renders_empty_snapshot_as_scanning() {
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| ui(f, &RuntimeSnapshot::default(), 0))
            .unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("scanning…"), "{rendered}");
    }

    #[test]
    fn fmt_helpers() {
        assert_eq!(fmt_bytes(0), "0M");
        assert_eq!(fmt_bytes(512 << 20), "512M");
        assert_eq!(fmt_bytes(2 << 30), "2.0G");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(fmt_age(0), "—");
    }
}
