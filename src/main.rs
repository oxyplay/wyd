mod actions;
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

use actions::process::{self, Identity, Signal};
use classify::{ProjectCache, attach, group, short_path};
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
    let mut projects = ProjectCache::default();
    let mut version = 0u64;
    loop {
        let next = (|| -> scanner::Result<RuntimeSnapshot> {
            let processes = scanner.scan()?;
            let ports = scanner::ports::scan().unwrap_or_default();
            let mut logical_items = group(&processes);
            attach(&mut logical_items, &processes, &ports, &mut projects);
            let (used, total) = scanner.memory();
            version += 1;
            Ok(RuntimeSnapshot {
                logical_items,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Details,
    Confirm { force: bool },
}

struct App {
    selected: usize,
    scroll: u16,
    mode: Mode,
    frozen: Vec<Identity>,
    frozen_title: String,
}

impl App {
    fn new() -> Self {
        Self {
            selected: 0,
            scroll: 0,
            mode: Mode::List,
            frozen: Vec::new(),
            frozen_title: String::new(),
        }
    }
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    snapshot: &Arc<RwLock<RuntimeSnapshot>>,
    force: &mpsc::Sender<()>,
) -> io::Result<()> {
    let mut drawn_version = u64::MAX;
    let mut app = App::new();
    loop {
        let snap = snapshot.read();
        let n = count_items(&snap.logical_items);
        if n == 0 {
            app.selected = 0;
        } else if app.selected >= n {
            app.selected = n - 1;
        }
        if snap.version != drawn_version {
            drawn_version = snap.version;
            terminal.draw(|f| ui(f, &snap, &app))?;
        }
        drop(snap);

        if event::poll(EVENT_POLL)? {
            if let Event::Key(key) = event::read()? {
                let snap = snapshot.read();
                match handle_key(key.code, &snap, &mut app, force) {
                    KeyResult::Quit => return Ok(()),
                    KeyResult::Continue => {}
                }
            }
            terminal.draw(|f| ui(f, &snapshot.read(), &app))?;
        }
    }
}

enum KeyResult {
    Quit,
    Continue,
}

fn handle_key(
    code: KeyCode,
    snap: &RuntimeSnapshot,
    app: &mut App,
    force: &mpsc::Sender<()>,
) -> KeyResult {
    match app.mode {
        Mode::List => match code {
            KeyCode::Char('q') | KeyCode::Esc => KeyResult::Quit,
            KeyCode::Char('r') => {
                let _ = force.send(());
                KeyResult::Continue
            }
            KeyCode::Up => {
                app.selected = app.selected.saturating_sub(1);
                app.scroll = app.selected.saturating_sub(2) as u16;
                KeyResult::Continue
            }
            KeyCode::Down => {
                let n = count_items(&snap.logical_items);
                if n > 0 {
                    app.selected = (app.selected + 1).min(n - 1);
                }
                app.scroll = app.selected.saturating_sub(2) as u16;
                KeyResult::Continue
            }
            KeyCode::Enter => {
                if count_items(&snap.logical_items) > 0 {
                    app.mode = Mode::Details;
                }
                KeyResult::Continue
            }
            KeyCode::Char('k') => open_confirm(snap, app, false),
            KeyCode::Char('K') => open_confirm(snap, app, true),
            _ => KeyResult::Continue,
        },
        Mode::Details => match code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                app.mode = Mode::List;
                KeyResult::Continue
            }
            KeyCode::Char('k') => open_confirm(snap, app, false),
            KeyCode::Char('K') => open_confirm(snap, app, true),
            _ => KeyResult::Continue,
        },
        Mode::Confirm { force: kill_force } => match code {
            KeyCode::Esc | KeyCode::Char('n') => {
                app.mode = Mode::List;
                KeyResult::Continue
            }
            KeyCode::Char('y') => {
                let signal = if kill_force {
                    Signal::Kill
                } else {
                    Signal::Term
                };
                let _ = process::send(&app.frozen, signal);
                app.mode = Mode::List;
                app.frozen.clear();
                let _ = force.send(());
                KeyResult::Continue
            }
            _ => KeyResult::Continue,
        },
    }
}

fn open_confirm(snap: &RuntimeSnapshot, app: &mut App, kill_force: bool) -> KeyResult {
    let Some(item) = nth_item(&snap.logical_items, app.selected) else {
        return KeyResult::Continue;
    };
    app.frozen = process::identities_for(item, &snap.processes);
    app.frozen_title = item.title();
    app.mode = Mode::Confirm { force: kill_force };
    KeyResult::Continue
}

fn nth_item(items: &[RuntimeItem], mut n: usize) -> Option<&RuntimeItem> {
    fn walk<'a>(items: &'a [RuntimeItem], n: &mut usize) -> Option<&'a RuntimeItem> {
        for item in items {
            if *n == 0 {
                return Some(item);
            }
            *n -= 1;
            if let Some(hit) = walk(&item.children, n) {
                return Some(hit);
            }
        }
        None
    }
    walk(items, &mut n)
}

fn ui(frame: &mut Frame, snap: &RuntimeSnapshot, app: &App) {
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

    match app.mode {
        Mode::List => {
            let lines = list_lines(snap, app.selected);
            frame.render_widget(Paragraph::new(lines).scroll((app.scroll, 0)), inner);
        }
        Mode::Details => {
            let lines = details_lines(snap, app.selected);
            frame.render_widget(Paragraph::new(lines), inner);
        }
        Mode::Confirm { force } => {
            frame.render_widget(Paragraph::new(confirm_lines(app, force)), inner);
        }
    }

    let hint = match app.mode {
        Mode::List => " ↑↓ select  enter details  k kill  K force  r refresh  q quit",
        Mode::Details => " k kill  K force  esc back",
        Mode::Confirm { force: true } => " y force kill  n/esc cancel",
        Mode::Confirm { force: false } => " y terminate  n/esc cancel",
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().add_modifier(Modifier::DIM)),
        footer,
    );
}

fn list_lines(snap: &RuntimeSnapshot, selected: usize) -> Vec<Line<'static>> {
    if snap.processes.is_empty() {
        return vec![Line::from(" scanning…")];
    }
    if snap.logical_items.is_empty() {
        return vec![Line::from(" no matching dev processes")];
    }
    let mut lines = Vec::new();
    let overview = overview_line(&snap.logical_items);
    if !overview.is_empty() {
        lines.push(Line::from(overview));
        lines.push(Line::from(""));
    }
    let by_pid: HashMap<u32, &ProcessInfo> = snap.processes.iter().map(|p| (p.pid, p)).collect();
    let mut idx = 0usize;
    render_items(
        &snap.logical_items,
        0,
        &by_pid,
        selected,
        &mut idx,
        &mut lines,
    );
    lines
}

fn render_items(
    items: &[RuntimeItem],
    depth: usize,
    by_pid: &HashMap<u32, &ProcessInfo>,
    selected: usize,
    idx: &mut usize,
    out: &mut Vec<Line<'static>>,
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
        let mut text = format!(
            "{indent}{marker}{:<32} {:>9} {:>5.1}%  {age}",
            truncate(&item.title(), 32),
            fmt_bytes(item.memory_bytes),
            item.cpu_percent,
        );
        if item.state == model::RuntimeState::Persistent {
            text.push_str("  persistent");
        }
        if !item.ports.is_empty() {
            let shown: Vec<String> = item.ports.iter().take(3).map(|p| p.label()).collect();
            text.push_str("  ");
            text.push_str(&shown.join(","));
            if item.ports.len() > 3 {
                text.push_str(&format!("+{}", item.ports.len() - 3));
            }
        }
        if let Some(project) = &item.project {
            text.push_str("  ");
            text.push_str(&short_path(&project.root));
        } else if let Some(p) = proc
            && let Some(tty) = &p.tty
        {
            text.push_str("  ");
            text.push_str(tty);
        }
        let style = if *idx == selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        out.push(Line::from(text).style(style));
        *idx += 1;
        render_items(&item.children, depth + 1, by_pid, selected, idx, out);
    }
}

fn details_lines(snap: &RuntimeSnapshot, selected: usize) -> Vec<Line<'static>> {
    let Some(item) = nth_item(&snap.logical_items, selected) else {
        return vec![Line::from(" no item")];
    };
    let by_pid: HashMap<u32, &ProcessInfo> = snap.processes.iter().map(|p| (p.pid, p)).collect();
    let proc = item.root_pid.and_then(|pid| by_pid.get(&pid).copied());
    let mut lines = vec![
        Line::from(format!("{}  {}", item.title(), item.category.label())),
        Line::from(""),
    ];
    if let Some(p) = proc {
        lines.push(Line::from(format!(
            "PID  {}    PPID  {}",
            p.pid,
            p.parent_pid
                .map(|n| n.to_string())
                .unwrap_or_else(|| "—".into())
        )));
        lines.push(Line::from(format!(
            "RAM  {}    CPU  {:.1}%    age  {}",
            fmt_bytes(p.memory_bytes),
            p.cpu_percent,
            fmt_age(p.start_time)
        )));
        if !p.command.is_empty() {
            lines.push(Line::from(format!("cmd  {}", p.command.join(" "))));
        }
        if let Some(cwd) = &p.cwd {
            lines.push(Line::from(format!("cwd  {}", short_path(cwd))));
        }
        if let Some(tty) = &p.tty {
            lines.push(Line::from(format!("tty  {tty}")));
        }
    }
    if let Some(project) = &item.project {
        lines.push(Line::from(format!(
            "project  {}  {}",
            project.name,
            short_path(&project.root)
        )));
    }
    if !item.ports.is_empty() {
        let ports = item
            .ports
            .iter()
            .map(|p| format!("{}:{}", p.address, p.port))
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(Line::from(format!("ports  {ports}")));
    }
    lines.push(Line::from(format!(
        "processes  {}",
        item.process_ids
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    )));
    if !item.children.is_empty() {
        lines.push(Line::from(format!(
            "children  {}",
            item.children
                .iter()
                .map(|c| c.title())
                .collect::<Vec<_>>()
                .join("  ")
        )));
    }
    lines
}

fn confirm_lines(app: &App, force: bool) -> Vec<Line<'static>> {
    let verb = if force { "Force kill" } else { "Terminate" };
    let mut lines = vec![
        Line::from(format!(
            "{verb} {} ({} processes)?",
            app.frozen_title,
            app.frozen.len()
        )),
        Line::from(""),
    ];
    for id in app.frozen.iter().take(12) {
        lines.push(Line::from(format!(
            "  PID {}  start {}",
            id.pid, id.start_time
        )));
    }
    if app.frozen.len() > 12 {
        lines.push(Line::from(format!("  … {} more", app.frozen.len() - 12)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "Identity is re-checked (PID + start time) before each signal.",
    ));
    lines
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
        terminal.draw(|f| ui(f, &snap, &App::new())).unwrap();
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
            .draw(|f| ui(f, &RuntimeSnapshot::default(), &App::new()))
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

    #[test]
    fn details_show_pid_and_command() {
        let snap = fixture_snapshot();
        let text: String = details_lines(&snap, 0)
            .into_iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("PID  100"), "{text}");
        assert!(text.contains("omp"), "{text}");
        assert!(text.contains("children"), "{text}");
    }

    #[test]
    fn confirm_lists_frozen_pids() {
        let snap = fixture_snapshot();
        let mut app = App::new();
        open_confirm(&snap, &mut app, false);
        assert!(!app.frozen.is_empty());
        let text: String = confirm_lines(&app, false)
            .into_iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Terminate omp"), "{text}");
        assert!(text.contains("PID 100"), "{text}");
    }

    #[test]
    fn enter_does_not_confirm_kill() {
        let snap = fixture_snapshot();
        let mut app = App::new();
        open_confirm(&snap, &mut app, false);
        let (tx, _rx) = mpsc::channel();
        handle_key(KeyCode::Enter, &snap, &mut app, &tx);
        assert!(matches!(app.mode, Mode::Confirm { force: false }));
    }
}
