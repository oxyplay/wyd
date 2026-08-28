mod draw;
mod rows;

use std::collections::HashSet;
use std::io;
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseEventKind},
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
    },
};
use parking_lot::RwLock;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Position, Rect},
};

use crate::actions::process::{self, Identity, Signal};
use crate::config;
use crate::model::{DockerResource, RuntimeSnapshot};

use draw::{hits, ui};
use rows::{Focus, Row, Section, overview, rows as visible_rows};

const EVENT_POLL: Duration = Duration::from_millis(100);

#[derive(Clone, PartialEq, Eq)]
enum Mode {
    List,
    Details,
    Help,
    ConfirmKill { force: bool },
    ConfirmDocker,
    ConfirmPrune,
}

struct App {
    focus: Focus,
    section: Section,
    ov_sel: usize,
    selected: usize,
    scroll: u16,
    detail_scroll: u16,
    mode: Mode,
    marked: HashSet<usize>,
    query: String,
    filtering: bool,
    project: Option<String>,
    frozen: Vec<Identity>,
    frozen_title: String,
    frozen_docker: Vec<DockerResource>,
}

impl App {
    fn new() -> Self {
        Self {
            focus: Focus::Runtime,
            section: Section::All,
            ov_sel: 0,
            selected: 0,
            scroll: 0,
            detail_scroll: 0,
            mode: Mode::List,
            marked: HashSet::new(),
            query: String::new(),
            filtering: false,
            project: None,
            frozen: Vec::new(),
            frozen_title: String::new(),
            frozen_docker: Vec::new(),
        }
    }

    fn rows<'a>(&self, snap: &'a RuntimeSnapshot) -> Vec<Row<'a>> {
        visible_rows(snap, self.section, self.project.as_deref(), &self.query)
    }

    fn clamp(&mut self, snap: &RuntimeSnapshot) {
        let n = self.rows(snap).len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
        let ov = overview(snap).len();
        if ov == 0 {
            self.ov_sel = 0;
        } else if self.ov_sel >= ov {
            self.ov_sel = ov - 1;
        }
    }

    fn reset_runtime(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.marked.clear();
    }
}

enum KeyResult {
    Quit,
    Continue,
}

pub fn run_tui(snapshot: Arc<RwLock<RuntimeSnapshot>>, force: mpsc::Sender<()>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = run(&mut terminal, &snapshot, &force);
    drop(force);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    result
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
        app.clamp(&snap);
        if snap.version != drawn_version {
            drawn_version = snap.version;
            terminal.draw(|f| ui(f, &snap, &mut app))?;
            execute!(terminal.backend_mut(), SetTitle(draw::window_title(&snap)))?;
        }
        drop(snap);

        if event::poll(EVENT_POLL)? {
            let ev = event::read()?;
            let snap = snapshot.read();
            let size = terminal.size()?;
            let area = Rect::new(0, 0, size.width, size.height);
            let quit = match ev {
                Event::Key(key) => handle_key(key.code, &snap, &mut app, force),
                Event::Mouse(m) => handle_mouse(m, &snap, &mut app, area),
                _ => KeyResult::Continue,
            };
            if matches!(quit, KeyResult::Quit) {
                return Ok(());
            }
            drop(snap);
            let snap = snapshot.read();
            terminal.draw(|f| ui(f, &snap, &mut app))?;
            execute!(terminal.backend_mut(), SetTitle(draw::window_title(&snap)))?;
        }
    }
}

fn handle_key(
    code: KeyCode,
    snap: &RuntimeSnapshot,
    app: &mut App,
    force: &mpsc::Sender<()>,
) -> KeyResult {
    match app.mode {
        Mode::List => {
            if app.filtering {
                return handle_filter_key(code, app);
            }
            let keys = &config::Config::global().keys;
            match code {
                KeyCode::Esc => clear_or_quit(app),
                KeyCode::Left => {
                    app.focus = Focus::Overview;
                    KeyResult::Continue
                }
                KeyCode::Right => {
                    apply_overview(snap, app);
                    KeyResult::Continue
                }
                KeyCode::Char('/') => {
                    app.filtering = true;
                    KeyResult::Continue
                }
                KeyCode::Char('p') => {
                    jump_projects(snap, app);
                    KeyResult::Continue
                }
                KeyCode::Char(c) if config::KeysConfig::hit(&keys.quit, c) => KeyResult::Quit,
                KeyCode::Char(c) if config::KeysConfig::hit(&keys.refresh, c) => {
                    let _ = force.send(());
                    KeyResult::Continue
                }
                KeyCode::Char(c) if config::KeysConfig::hit(&keys.help, c) => {
                    app.mode = Mode::Help;
                    KeyResult::Continue
                }
                KeyCode::Char(' ') if app.focus == Focus::Runtime => {
                    if !app.rows(snap).is_empty() && !app.marked.remove(&app.selected) {
                        app.marked.insert(app.selected);
                    }
                    KeyResult::Continue
                }
                KeyCode::Up => {
                    move_sel(app, snap, -1);
                    KeyResult::Continue
                }
                KeyCode::Down => {
                    move_sel(app, snap, 1);
                    KeyResult::Continue
                }
                KeyCode::Enter => on_enter(snap, app),
                KeyCode::Char(c) if config::KeysConfig::hit(&keys.kill, c) => {
                    open_kill_confirm(snap, app, false)
                }
                KeyCode::Char(c) if config::KeysConfig::hit(&keys.force_kill, c) => {
                    open_kill_confirm(snap, app, true)
                }
                KeyCode::Char(c) if config::KeysConfig::hit(&keys.stop, c) => {
                    stop_docker(snap, app, force)
                }
                KeyCode::Char(c) if config::KeysConfig::hit(&keys.prune, c) => {
                    open_prune_confirm(snap, app)
                }
                KeyCode::Char(c) if config::KeysConfig::hit(&keys.clean, c) => {
                    open_docker_confirm(snap, app)
                }
                KeyCode::Tab => {
                    app.focus = if app.focus == Focus::Overview {
                        Focus::Runtime
                    } else {
                        Focus::Overview
                    };
                    KeyResult::Continue
                }
                KeyCode::Backspace => back(app),
                KeyCode::Char('j') => {
                    move_sel(app, snap, 1);
                    KeyResult::Continue
                }
                KeyCode::Char('k') => {
                    move_sel(app, snap, -1);
                    KeyResult::Continue
                }
                KeyCode::Char('h') => {
                    app.focus = Focus::Overview;
                    KeyResult::Continue
                }
                KeyCode::Char('l') => {
                    apply_overview(snap, app);
                    KeyResult::Continue
                }
                _ => KeyResult::Continue,
            }
        }
        Mode::Help => match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Backspace => {
                app.mode = Mode::List;
                KeyResult::Continue
            }
            _ => KeyResult::Continue,
        },
        Mode::Details => {
            let keys = &config::Config::global().keys;
            match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Backspace => {
                    app.mode = Mode::List;
                    KeyResult::Continue
                }
                KeyCode::Char(c) if config::KeysConfig::hit(&keys.kill, c) => {
                    open_kill_confirm(snap, app, false)
                }
                KeyCode::Char(c) if config::KeysConfig::hit(&keys.force_kill, c) => {
                    open_kill_confirm(snap, app, true)
                }
                KeyCode::Char(c) if config::KeysConfig::hit(&keys.stop, c) => {
                    stop_docker(snap, app, force)
                }
                KeyCode::Char(c) if config::KeysConfig::hit(&keys.clean, c) => {
                    open_docker_confirm(snap, app)
                }
                KeyCode::Char('o') => {
                    open_selected_url(snap, app, 0);
                    KeyResult::Continue
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.detail_scroll = app.detail_scroll.saturating_sub(1);
                    KeyResult::Continue
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.detail_scroll = app.detail_scroll.saturating_add(1);
                    KeyResult::Continue
                }
                KeyCode::PageUp => {
                    app.detail_scroll = app.detail_scroll.saturating_sub(8);
                    KeyResult::Continue
                }
                KeyCode::PageDown => {
                    app.detail_scroll = app.detail_scroll.saturating_add(8);
                    KeyResult::Continue
                }
                _ => KeyResult::Continue,
            }
        }
        Mode::ConfirmKill { force: kill_force } => match code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Backspace => {
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
                app.marked.clear();
                let _ = force.send(());
                KeyResult::Continue
            }
            _ => KeyResult::Continue,
        },
        Mode::ConfirmDocker => match code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Backspace => {
                app.mode = Mode::List;
                app.frozen_docker.clear();
                KeyResult::Continue
            }
            KeyCode::Char('y') => {
                let targets: Vec<_> = app
                    .frozen_docker
                    .iter()
                    .filter(|r| !r.persistent)
                    .cloned()
                    .collect();
                if !targets.is_empty() {
                    for res in &targets {
                        let _ = crate::actions::docker::remove_blocking(res);
                    }
                    app.mode = Mode::List;
                    app.frozen_docker.clear();
                    app.marked.clear();
                    let _ = force.send(());
                }
                KeyResult::Continue
            }
            KeyCode::Char('D') => {
                let targets: Vec<_> = app
                    .frozen_docker
                    .iter()
                    .filter(|r| r.persistent)
                    .cloned()
                    .collect();
                if !targets.is_empty() {
                    for res in &targets {
                        let _ = crate::actions::docker::remove_blocking(res);
                    }
                    app.mode = Mode::List;
                    app.frozen_docker.clear();
                    app.marked.clear();
                    let _ = force.send(());
                }
                KeyResult::Continue
            }
            _ => KeyResult::Continue,
        },
        Mode::ConfirmPrune => match code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Backspace => {
                app.mode = Mode::List;
                KeyResult::Continue
            }
            KeyCode::Char('y') => {
                let ids = snap.docker.prunable_ids();
                let _ = crate::actions::docker::prune_anonymous_volumes_blocking(&ids);
                app.mode = Mode::List;
                let _ = force.send(());
                KeyResult::Continue
            }
            _ => KeyResult::Continue,
        },
    }
}

fn handle_mouse(
    m: event::MouseEvent,
    snap: &RuntimeSnapshot,
    app: &mut App,
    area: Rect,
) -> KeyResult {
    let h = hits(area);
    let pos = Position {
        x: m.column,
        y: m.row,
    };
    if !matches!(app.mode, Mode::List) {
        if matches!(m.kind, MouseEventKind::Down(_)) {
            // Only the popup area dismisses; listener rows are opened via the
            // explicit `o` key (honest HTTP assumption), not a brittle
            // popup-line offset.
            if !h.popup.contains(pos) {
                app.mode = Mode::List;
                app.frozen_docker.clear();
            }
        }
        return KeyResult::Continue;
    }
    match m.kind {
        MouseEventKind::Down(_) | MouseEventKind::Drag(_) => {
            if h.overview.contains(pos) {
                let i = m.row.saturating_sub(h.overview.y) as usize;
                if i < overview(snap).len() {
                    app.ov_sel = i;
                    app.focus = Focus::Overview;
                    apply_overview(snap, app);
                }
            } else if h.list.contains(pos) {
                let i = m.row.saturating_sub(h.list.y) as usize + app.scroll as usize;
                let n = app.rows(snap).len();
                if i < n {
                    let again = app.focus == Focus::Runtime && app.selected == i;
                    app.selected = i;
                    app.focus = Focus::Runtime;
                    if again && matches!(m.kind, MouseEventKind::Down(_)) {
                        app.detail_scroll = 0;
                        app.mode = Mode::Details;
                    }
                }
            }
        }
        MouseEventKind::ScrollUp => move_sel(app, snap, -1),
        MouseEventKind::ScrollDown => move_sel(app, snap, 1),
        _ => {}
    }
    KeyResult::Continue
}

fn handle_filter_key(code: KeyCode, app: &mut App) -> KeyResult {
    match code {
        KeyCode::Esc => {
            app.filtering = false;
            app.query.clear();
            app.reset_runtime();
            KeyResult::Continue
        }
        KeyCode::Enter => {
            app.filtering = false;
            KeyResult::Continue
        }
        KeyCode::Backspace => {
            app.query.pop();
            app.reset_runtime();
            KeyResult::Continue
        }
        KeyCode::Char(c) if !c.is_control() => {
            app.query.push(c);
            app.reset_runtime();
            KeyResult::Continue
        }
        _ => KeyResult::Continue,
    }
}

fn clear_or_quit(app: &mut App) -> KeyResult {
    if !app.query.is_empty() {
        app.query.clear();
        app.reset_runtime();
        return KeyResult::Continue;
    }
    if app.project.take().is_some() {
        app.reset_runtime();
        return KeyResult::Continue;
    }
    if app.section != Section::All {
        app.section = Section::All;
        app.ov_sel = 0;
        app.reset_runtime();
        return KeyResult::Continue;
    }
    KeyResult::Quit
}

/// Backspace: same unwind as esc (filter → project → section) but never quits.
fn back(app: &mut App) -> KeyResult {
    if !app.query.is_empty() {
        app.query.clear();
        app.reset_runtime();
    } else if app.project.take().is_some() {
        app.reset_runtime();
    } else if app.section != Section::All {
        app.section = Section::All;
        app.ov_sel = 0;
        app.reset_runtime();
    }
    KeyResult::Continue
}

fn move_sel(app: &mut App, snap: &RuntimeSnapshot, delta: i32) {
    if app.focus == Focus::Overview {
        let n = overview(snap).len();
        if n == 0 {
            return;
        }
        let next = app.ov_sel as i32 + delta;
        app.ov_sel = next.clamp(0, n as i32 - 1) as usize;
    } else {
        let n = app.rows(snap).len();
        if n == 0 {
            return;
        }
        let next = app.selected as i32 + delta;
        app.selected = next.clamp(0, n as i32 - 1) as usize;
    }
}

fn apply_overview(snap: &RuntimeSnapshot, app: &mut App) {
    if let Some(line) = overview(snap).get(app.ov_sel) {
        app.section = line.section;
        app.focus = Focus::Runtime;
        app.reset_runtime();
    }
}

fn jump_projects(snap: &RuntimeSnapshot, app: &mut App) {
    let ov = overview(snap);
    if let Some(i) = ov.iter().position(|l| l.section == Section::Projects) {
        app.ov_sel = i;
        app.section = Section::Projects;
        app.focus = Focus::Runtime;
        app.reset_runtime();
    }
}

fn on_enter(snap: &RuntimeSnapshot, app: &mut App) -> KeyResult {
    if app.focus == Focus::Overview {
        apply_overview(snap, app);
        return KeyResult::Continue;
    }
    let rs = app.rows(snap);
    match rs.get(app.selected) {
        Some(Row::Project { name, .. }) => {
            app.project = Some(name.clone());
            app.section = Section::All;
            app.ov_sel = 0;
            app.reset_runtime();
        }
        Some(_) => {
            app.detail_scroll = 0;
            app.mode = Mode::Details;
        }
        None => {}
    }
    KeyResult::Continue
}

fn open_kill_confirm(snap: &RuntimeSnapshot, app: &mut App, kill_force: bool) -> KeyResult {
    let rs = app.rows(snap);
    let mut idxs: Vec<usize> = app.marked.iter().copied().collect();
    if idxs.is_empty() {
        idxs.push(app.selected);
    }
    idxs.sort_unstable();
    app.frozen.clear();
    let mut titles = Vec::new();
    for i in idxs {
        if let Some(Row::Item { item, .. }) = rs.get(i) {
            app.frozen
                .extend(process::identities_for(item, &snap.processes));
            titles.push(item.title());
        }
    }
    if app.frozen.is_empty() {
        return KeyResult::Continue;
    }
    app.frozen_title = titles.join(", ");
    app.mode = Mode::ConfirmKill { force: kill_force };
    KeyResult::Continue
}

/// Stop marked (or selected) running containers, then rescan. Stopping is
/// reversible (`docker start`), so no confirm — unlike remove.
fn stop_docker(snap: &RuntimeSnapshot, app: &mut App, force: &mpsc::Sender<()>) -> KeyResult {
    let rs = app.rows(snap);
    let mut idxs: Vec<usize> = app.marked.iter().copied().collect();
    if idxs.is_empty() {
        idxs.push(app.selected);
    }
    idxs.sort_unstable();
    let mut stopped = false;
    for i in idxs {
        if let Some(Row::Docker(res)) = rs.get(i)
            && res.running()
        {
            stopped |= crate::actions::docker::stop_blocking(res).is_ok();
        }
    }
    if stopped {
        app.marked.clear();
        let _ = force.send(());
    }
    KeyResult::Continue
}

/// Open the i-th URL of the selected item. Mouse capture keeps the terminal
/// from opening links itself, so wyd does it.
fn open_selected_url(snap: &RuntimeSnapshot, app: &App, index: usize) {
    if let Some(Row::Item { item, .. }) = app.rows(snap).get(app.selected)
        && let Some(p) = item.ports.get(index)
    {
        open_url(&p.url());
    }
}

fn open_url(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(cmd).arg(url).spawn();
}

fn open_docker_confirm(snap: &RuntimeSnapshot, app: &mut App) -> KeyResult {
    let rs = app.rows(snap);
    let mut idxs: Vec<usize> = app.marked.iter().copied().collect();
    if idxs.is_empty() {
        idxs.push(app.selected);
    }
    idxs.sort_unstable();
    for i in idxs {
        if let Some(Row::Docker(res)) = rs.get(i) {
            app.frozen_docker.push((*res).clone());
        }
    }
    if app.frozen_docker.is_empty() {
        return KeyResult::Continue;
    }
    app.mode = Mode::ConfirmDocker;
    KeyResult::Continue
}

/// `P`: offer to delete all unused anonymous volumes. Named volumes and
/// anything attached survive — the engine filters, wyd only counts.
fn open_prune_confirm(snap: &RuntimeSnapshot, app: &mut App) -> KeyResult {
    if snap.docker.ok && snap.docker.prunable_stats().0 > 0 {
        app.mode = Mode::ConfirmPrune;
    }
    KeyResult::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    use ratatui::{Terminal, backend::TestBackend};

    use crate::classify::group;
    use crate::model::{self, ProcessInfo, Project, RuntimeSnapshot};

    use super::draw::{
        confirm_lines, details_lines, docker_confirm_lines, help_lines, hint, overview_lines,
        runtime_summary, window_title,
    };
    use super::rows::{fmt_age, fmt_bytes, truncate};

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
            docker: Arc::new(model::DockerSnapshot::default()),
            total_memory_bytes: 32 << 30,
            used_memory_bytes: 7 << 30,
            cpu_percent: 12.0,
            sessions: vec![],
            version: 1,
        }
    }
    #[test]
    fn window_title_lists_running_counts() {
        let snap = fixture_snapshot();
        let title = window_title(&snap);
        assert!(title.starts_with("wyd ·"), "{title}");
        assert!(title.contains("1 agents"), "{title}");
        assert!(title.contains("1 mcp"), "{title}");
        assert!(!title.contains("left"), "{title}");
    }

    #[test]
    fn renders_two_panel_tree() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let snap = fixture_snapshot();
        let mut app = App::new();
        terminal.draw(|f| ui(f, &snap, &mut app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        for expected in [
            "wyd",
            "RAM 7.0G/32.0G",
            "CPU 12%",
            "Overview",
            "Agents",
            "MCP",
            "● omp",
            "mcp",
            "2 procs",
            "space mark",
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

    /// The status column must explicitly separate the three runtime classes:
    /// leftover / persistent / owned by a live session.
    #[test]
    fn status_column_separates_leftover_persistent_owned() {
        let mut snap = fixture_snapshot();
        // MCP under the live agent: owned, then flipped to persistent.
        snap.logical_items[0].children[0].state = model::RuntimeState::Persistent;
        // A detached MCP with no owner: leftover.
        snap.logical_items.push(model::RuntimeItem {
            category: model::Category::Mcp,
            display_name: "playwright-mcp".into(),
            root_pid: Some(900),
            process_ids: vec![900],
            memory_bytes: 1 << 20,
            cpu_percent: 0.0,
            state: model::RuntimeState::Suspicious,
            suspicion: Some(model::Suspicion {
                score: 75,
                reasons: vec![model::SuspicionReason::OwningAgentMissing],
            }),
            ports: vec![],
            project: None,
            children: vec![],
        });
        let backend = TestBackend::new(180, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        terminal.draw(|f| ui(f, &snap, &mut app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        for status in ["owned", "persistent", "leftover"] {
            assert!(rendered.contains(status), "missing {status:?}:\n{rendered}");
        }
        // Leftover keeps its warn marker; the owned Chromium is still there.
        assert!(rendered.contains("⚠ playwright-mcp"), "{rendered}");
        assert!(rendered.contains("Chromium"), "{rendered}");
    }

    #[test]
    fn details_show_url_for_real_socket() {
        let mut snap = fixture_snapshot();
        snap.logical_items[0].ports = vec![model::ListeningPort {
            protocol: model::Protocol::Tcp,
            address: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 5555,
            pid: 100,
        }];
        let text: String = details_lines(&snap, &App::new(), 100)
            .into_iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // A listener is an observed TCP socket, not a fabricated URL.
        assert!(text.contains(":5555"), "{text}");
        assert!(text.contains("127.0.0.1"), "{text}");
        assert!(text.contains("pid 100"), "{text}");
        assert!(
            !text.contains("url"),
            "listener must not be labeled url:\n{text}"
        );
        assert!(!text.contains("http://"), "no fabricated URL:\n{text}");
        // The explicit HTTP open convenience still constructs a URL.
        assert_eq!(
            snap.logical_items[0].ports[0].url(),
            "http://127.0.0.1:5555"
        );
    }

    #[test]
    fn renders_empty_snapshot_as_scanning() {
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        terminal
            .draw(|f| ui(f, &RuntimeSnapshot::default(), &mut app))
            .unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("scanning…"), "{rendered}");
    }

    #[test]
    fn sessions_section_renders_agent_and_state() {
        use crate::model::session::{RuntimeSessionId, SessionInfo};
        let mut snap = fixture_snapshot();
        snap.sessions = vec![SessionInfo {
            id: RuntimeSessionId::from_u64(1),
            agent: "omp".into(),
            project: Some("/src/queryknight".into()),
            active: true,
            started_at: 1000,
        }];
        let mut app = App::new();
        app.section = rows::Section::Sessions;
        let backend = TestBackend::new(120, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, &snap, &mut app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("omp"), "{rendered}");
        assert!(rendered.contains("Sessions"), "{rendered}");
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
    fn scroll_keeps_selection_in_view() {
        let mut app = App::new();
        app.selected = 20;
        app.scroll = 0;
        super::draw::follow_selected(&mut app, 10);
        assert_eq!(app.scroll, 11);
        app.selected = 2;
        super::draw::follow_selected(&mut app, 10);
        assert_eq!(app.scroll, 2);
    }

    #[test]
    fn details_show_pid_and_command() {
        let snap = fixture_snapshot();
        let text: String = details_lines(&snap, &App::new(), 80)
            .into_iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("pid"), "{text}");
        assert!(text.contains("100"), "{text}");
        assert!(text.contains("omp"), "{text}");
        assert!(text.contains("children"), "{text}");
    }

    #[test]
    fn confirm_lists_frozen_pids() {
        let snap = fixture_snapshot();
        let mut app = App::new();
        open_kill_confirm(&snap, &mut app, false);
        assert!(!app.frozen.is_empty());
        let text: String = confirm_lines(&app, false)
            .into_iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("omp"), "{text}");
        assert!(text.contains("100"), "{text}");
        assert!(text.contains("term"), "{text}");
    }

    #[test]
    fn enter_does_not_confirm_kill() {
        let snap = fixture_snapshot();
        let mut app = App::new();
        open_kill_confirm(&snap, &mut app, false);
        let (tx, _rx) = mpsc::channel();
        handle_key(KeyCode::Enter, &snap, &mut app, &tx);
        assert!(matches!(app.mode, Mode::ConfirmKill { force: false }));
    }

    #[test]
    fn docker_section_volume_needs_d() {
        let mut snap = fixture_snapshot();
        snap.docker = Arc::new(model::DockerSnapshot {
            ok: true,
            note: String::new(),
            disk_bytes: 1 << 30,
            reclaimable_bytes: 100,
            resources: vec![model::DockerResource {
                kind: model::DockerKind::Volume,
                id: "old_pg".into(),
                name: "old_pg".into(),
                detail: "unused".into(),
                size_bytes: 6 << 30,
                compose: Some("oldproject".into()),
                persistent: true,
                anonymous: false,
                created: 0,
            }],
        });
        let mut app = App::new();
        app.section = Section::Docker;
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, &snap, &mut app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("old_pg"), "{rendered}");

        open_docker_confirm(&snap, &mut app);
        let text: String = docker_confirm_lines(&app)
            .into_iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("PERSISTENT DATA"), "{text}");
        let (tx, _rx) = mpsc::channel();
        handle_key(KeyCode::Char('y'), &snap, &mut app, &tx);
        assert!(
            matches!(app.mode, Mode::ConfirmDocker),
            "y must not delete a volume"
        );
    }

    #[test]
    fn help_lists_configured_keys() {
        let text: String = help_lines()
            .into_iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("terminate"), "{text}");
        assert!(text.contains("space"), "{text}");
        assert!(text.contains("projects"), "{text}");
        assert!(text.contains("[keys]"), "{text}");
    }

    #[test]
    fn space_marks_and_kill_confirm_uses_marks() {
        let snap = fixture_snapshot();
        let mut app = App::new();
        let (tx, _rx) = mpsc::channel();
        handle_key(KeyCode::Char(' '), &snap, &mut app, &tx);
        app.selected = 1;
        handle_key(KeyCode::Char(' '), &snap, &mut app, &tx);
        assert_eq!(app.marked.len(), 2);
        open_kill_confirm(&snap, &mut app, false);
        assert!(app.frozen_title.contains("omp"), "{}", app.frozen_title);
        assert!(
            app.frozen_title.contains("chrome-devtools-mcp"),
            "{}",
            app.frozen_title
        );
    }

    #[test]
    fn slash_filter_hides_non_matching_leaves() {
        let snap = fixture_snapshot();
        assert_eq!(visible_rows(&snap, Section::All, None, "").len(), 3);
        assert_eq!(visible_rows(&snap, Section::All, None, "devtools").len(), 2);
        let mut app = App::new();
        let (tx, _rx) = mpsc::channel();
        handle_key(KeyCode::Char('/'), &snap, &mut app, &tx);
        for c in "devtools".chars() {
            handle_key(KeyCode::Char(c), &snap, &mut app, &tx);
        }
        assert_eq!(app.query, "devtools");
        let names: Vec<_> = app
            .rows(&snap)
            .into_iter()
            .filter_map(|r| match r {
                Row::Item { item, .. } => Some(item.display_name.clone()),
                _ => None,
            })
            .collect();
        assert!(
            names.iter().any(|n| n.contains("chrome-devtools")),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("Chromium")),
            "non-matching leaf leaked: {names:?}"
        );
    }

    #[test]
    fn p_opens_projects_enter_pins_filter() {
        let mut snap = fixture_snapshot();
        snap.logical_items[0].project = Some(Project {
            name: "queryknight".into(),
            root: "/Users/max/Work/queryknight".into(),
        });
        let mut app = App::new();
        let (tx, _rx) = mpsc::channel();
        handle_key(KeyCode::Char('p'), &snap, &mut app, &tx);
        assert_eq!(app.section, Section::Projects);
        assert_eq!(app.focus, Focus::Runtime);
        assert!(
            app.rows(&snap)
                .iter()
                .any(|r| matches!(r, Row::Project { name, .. } if name == "queryknight"))
        );
        handle_key(KeyCode::Enter, &snap, &mut app, &tx);
        assert_eq!(app.project.as_deref(), Some("queryknight"));
        assert_eq!(app.section, Section::All);
    }

    #[test]
    fn leftovers_section_hides_clean_tree() {
        let snap = fixture_snapshot();
        assert!(visible_rows(&snap, Section::Leftovers, None, "").is_empty());
    }

    #[test]
    fn marked_docker_rows_batch_into_confirm() {
        let mut snap = fixture_snapshot();
        snap.docker = Arc::new(model::DockerSnapshot {
            ok: true,
            note: String::new(),
            disk_bytes: 1 << 30,
            reclaimable_bytes: 100,
            resources: vec![
                model::DockerResource {
                    kind: model::DockerKind::Container,
                    id: "abc".into(),
                    name: "old_web".into(),
                    detail: "exited".into(),
                    size_bytes: 40 << 20,
                    compose: None,
                    persistent: false,
                    anonymous: false,
                    created: 0,
                },
                model::DockerResource {
                    kind: model::DockerKind::Volume,
                    id: "old_pg".into(),
                    name: "old_pg".into(),
                    detail: "unused".into(),
                    size_bytes: 6 << 30,
                    compose: Some("oldproject".into()),
                    persistent: true,
                    anonymous: false,
                    created: 0,
                },
            ],
        });
        let mut app = App::new();
        app.section = Section::Docker;
        let (tx, _rx) = mpsc::channel();
        handle_key(KeyCode::Char(' '), &snap, &mut app, &tx);
        app.selected = 1;
        handle_key(KeyCode::Char(' '), &snap, &mut app, &tx);
        open_docker_confirm(&snap, &mut app);
        assert_eq!(app.frozen_docker.len(), 2);
        let text: String = docker_confirm_lines(&app)
            .into_iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("old_web"), "{text}");
        assert!(text.contains("old_pg"), "{text}");
        assert!(text.contains("PERSISTENT DATA"), "{text}");
    }

    // ── Node PID 90167 regression fixture ──────────────────────────────
    // A real dogfooding case: a node process with PPID 1, four localhost TCP
    // listeners, ParentExited suspicion score 40, and a long command.
    fn node_fixture() -> RuntimeSnapshot {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let process = ProcessInfo {
            pid: 90167,
            parent_pid: Some(1),
            name: "node".into(),
            command: vec![
                "/Library/Application Support/OpenCode/open-code".into(),
                "server".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--experimental-strip-types".into(),
                "--source-map".into(),
                "index.mjs".into(),
            ],
            executable: None,
            cwd: Some("/".into()),
            cpu_percent: 0.0,
            memory_bytes: 16 << 20,
            start_time: now - (2 * 3600 + 47 * 60),
            tty: None,
        };
        let port = |p: u16| model::ListeningPort {
            protocol: model::Protocol::Tcp,
            address: "127.0.0.1".parse().unwrap(),
            port: p,
            pid: 90167,
        };
        let item = model::RuntimeItem {
            category: model::Category::DevServer,
            display_name: "node".into(),
            root_pid: Some(90167),
            process_ids: vec![90167],
            memory_bytes: 16 << 20,
            cpu_percent: 0.0,
            state: model::RuntimeState::Suspicious,
            suspicion: Some(model::Suspicion {
                score: 40,
                reasons: vec![model::SuspicionReason::ParentExited],
            }),
            ports: vec![port(45623), port(49206), port(53674), port(53675)],
            project: None,
            children: vec![],
        };
        RuntimeSnapshot {
            processes: vec![process],
            logical_items: vec![item],
            docker: Arc::new(model::DockerSnapshot::default()),
            total_memory_bytes: 32 << 30,
            used_memory_bytes: 7 << 30,
            cpu_percent: 1.0,
            sessions: vec![],
            version: 1,
        }
    }

    fn join_lines(lines: Vec<ratatui::text::Line<'static>>) -> String {
        lines
            .into_iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_runtime_at(snap: &RuntimeSnapshot, w: u16, h: u16) -> String {
        let mut app = App::new();
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, snap, &mut app)).unwrap();
        format!("{:?}", terminal.backend().buffer())
    }

    fn render_details_at(snap: &RuntimeSnapshot, w: u16, h: u16) -> String {
        let mut app = App::new();
        app.mode = Mode::Details;
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, snap, &mut app)).unwrap();
        format!("{:?}", terminal.backend().buffer())
    }

    /// Listeners are observed TCP sockets, never presented as URLs; all four
    /// survive with address + owning PID; not claiming four HTTP servers.
    #[test]
    fn node_details_listeners_are_not_urls() {
        let snap = node_fixture();
        let text = join_lines(details_lines(&snap, &App::new(), 80));
        assert!(
            !text.contains("url"),
            "listeners must not be labeled url:\n{text}"
        );
        for p in [45623u16, 49206, 53674, 53675] {
            assert!(
                text.contains(&format!(":{p}")),
                "missing listener :{p}:\n{text}"
            );
        }
        assert!(text.contains("127.0.0.1"), "missing address:\n{text}");
        assert!(text.contains("pid 90167"), "missing pid ownership:\n{text}");
        assert!(
            !text.contains("http://"),
            "must not fabricate URLs:\n{text}"
        );
    }

    #[test]
    fn node_details_long_values_wrap_not_truncate() {
        let snap = node_fixture();
        let text = join_lines(details_lines(&snap, &App::new(), 80));
        assert!(
            text.contains("--experimental-strip-types"),
            "long command truncated:\n{text}"
        );
        assert!(
            text.contains("/Library/Application Support/OpenCode"),
            "long path truncated:\n{text}"
        );
        assert!(text.contains("parent exited / re-parented"), "{text}");
        assert!(
            text.contains("The original parent is gone"),
            "shared explanation missing:\n{text}"
        );
        assert!(text.contains("leftover candidate"), "{text}");
        assert!(text.contains("40 / 100"), "{text}");
    }

    #[test]
    fn node_what_is_multilistener() {
        let snap = node_fixture();
        let rendered = render_runtime_at(&snap, 100, 24);
        assert!(
            rendered.contains("srv ×4"),
            "multi-listener WHAT:\n{rendered}"
        );
    }

    #[test]
    fn what_single_listener_shows_port() {
        let mut snap = node_fixture();
        snap.logical_items[0].ports.truncate(1);
        snap.logical_items[0].ports[0].port = 5173;
        let rendered = render_runtime_at(&snap, 100, 24);
        assert!(rendered.contains("srv :5173"), "{rendered}");
    }

    #[test]
    fn details_popup_renders_at_small_terminal() {
        let snap = node_fixture();
        let mut app = App::new();
        app.mode = Mode::Details;
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, &snap, &mut app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        // Top of the details hierarchy is visible at once.
        assert!(rendered.contains("leftover candidate"), "{rendered}");
        assert!(rendered.contains("40 / 100"), "{rendered}");
        assert!(rendered.contains("node"), "{rendered}");
        // Content taller than the popup → scrollable: the listeners are not
        // visible yet, but scrolling brings them in.
        app.detail_scroll = 20;
        terminal.draw(|f| ui(f, &snap, &mut app)).unwrap();
        let scrolled = format!("{:?}", terminal.backend().buffer());
        assert!(scrolled.contains("listening sockets"), "{scrolled}");
        assert!(scrolled.contains("45623"), "{scrolled}");
    }

    #[test]
    fn details_popup_renders_at_medium_terminal() {
        let snap = node_fixture();
        let rendered = render_details_at(&snap, 140, 35);
        assert!(rendered.contains("listening sockets"), "{rendered}");
        assert!(
            rendered.contains("53675"),
            "last listener reachable:\n{rendered}"
        );
        assert!(rendered.contains("leftover candidate"), "{rendered}");
    }

    #[test]
    fn details_scroll_increments_and_clamps() {
        let snap = node_fixture();
        let mut app = App::new();
        app.mode = Mode::Details;
        let (tx, _rx) = mpsc::channel();
        for _ in 0..10 {
            handle_key(KeyCode::Down, &snap, &mut app, &tx);
        }
        assert_eq!(app.detail_scroll, 10);
        handle_key(KeyCode::Up, &snap, &mut app, &tx);
        assert_eq!(app.detail_scroll, 9);
        for _ in 0..20 {
            handle_key(KeyCode::Up, &snap, &mut app, &tx);
        }
        assert_eq!(app.detail_scroll, 0, "must not go below zero");
        // A large scroll clamps to content height when rendered (no panic).
        app.detail_scroll = 999;
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, &snap, &mut app)).unwrap();
        assert!(
            (app.detail_scroll as usize) < 999,
            "scroll was clamped to content height, not left at 999: {}",
            app.detail_scroll
        );
    }

    #[test]
    fn details_footer_describes_http_honestly() {
        let snap = node_fixture();
        let mut app = App::new();
        app.mode = Mode::Details;
        let h = hint(&app, &snap);
        assert!(h.contains("j/k scroll"), "{h}");
        assert!(h.contains("try HTTP"), "footer must say 'try HTTP': {h}");
        assert!(h.contains("kill"), "{h}");
        assert_eq!(
            snap.logical_items[0].ports[0].url(),
            "http://127.0.0.1:45623"
        );
    }

    #[test]
    fn help_describes_http_assumption() {
        let text = join_lines(help_lines());
        assert!(text.contains("as HTTP"), "{text}");
    }

    // ── Overview / sidebar semantics ───────────────────────────────────
    fn overview_fixture() -> RuntimeSnapshot {
        let mut snap = node_fixture();
        let res = |name: &str| model::DockerResource {
            kind: model::DockerKind::Container,
            id: name.into(),
            name: name.into(),
            detail: "exited".into(),
            size_bytes: 1 << 30,
            compose: None,
            persistent: false,
            anonymous: false,
            created: 0,
        };
        snap.docker = Arc::new(model::DockerSnapshot {
            ok: true,
            note: String::new(),
            disk_bytes: 9 << 30,
            reclaimable_bytes: 103 << 20,
            resources: vec![res("a"), res("b")],
        });
        snap
    }

    #[test]
    fn overview_shows_name_and_count_only() {
        let snap = overview_fixture();
        let text = join_lines(overview_lines(&snap, &App::new(), 44));
        // Names and counts stay.
        assert!(text.contains("Leftovers"), "{text}");
        assert!(text.contains("Docker"), "{text}");
        assert!(text.contains("Dev servers"), "{text}");
        assert!(text.contains(" 1"), "leftover count:\n{text}");
        assert!(text.contains(" 2"), "docker count:\n{text}");
        // Metrics were removed from the sidebar — no RAM / disk / reclaim.
        assert!(!text.contains("RAM"), "no memory in sidebar:\n{text}");
        assert!(!text.contains("16M"), "no byte metric in sidebar:\n{text}");
        assert!(!text.contains("9.0G"), "no docker disk in sidebar:\n{text}");
        assert!(!text.contains("reclaim"), "no reclaim in sidebar:\n{text}");
        assert!(!text.contains("dis…"), "no chopped summary:\n{text}");
    }

    #[test]
    fn overview_narrow_keeps_name_and_count() {
        let snap = overview_fixture();
        let text = join_lines(overview_lines(&snap, &App::new(), 26));
        assert!(text.contains("Leftovers"), "{text}");
        assert!(text.contains("Docker"), "{text}");
        assert!(text.contains("Dev servers"), "{text}");
        assert!(text.contains(" 1"), "counts stay visible:\n{text}");
        assert!(!text.contains("RAM"), "no memory in sidebar:\n{text}");
    }

    #[test]
    fn overview_rows_never_exceed_pane_width() {
        let snap = overview_fixture();
        for w in [44usize, 34, 26, 20] {
            let mut app = App::new();
            app.focus = Focus::Overview;
            app.ov_sel = 2;
            for line in overview_lines(&snap, &app, w) {
                let s = line.to_string();
                assert!(
                    s.chars().count() <= w,
                    "overview row {s:?} exceeds width {w}"
                );
            }
        }
    }

    // ── Bottom-of-pane RAM/CPU summary ─────────────────────────────────
    #[test]
    fn runtime_summary_shows_item_totals() {
        let snap = node_fixture();
        let rs = visible_rows(&snap, Section::All, None, "");
        let line = runtime_summary(&rs, 60).to_string();
        assert!(line.contains("1 item"), "{line}");
        assert!(line.contains("16M RAM"), "{line}");
        assert!(line.contains("CPU"), "{line}");
        // Right-aligned: the value fills the pane width, hugging the right edge.
        assert!(line.ends_with("0.0% CPU"), "right-aligned tail:\n{line}");
        assert_eq!(line.chars().count(), 60, "padded to pane width:\n{line}");
    }

    #[test]
    fn runtime_summary_aggregates_multiple_items() {
        let mut snap = node_fixture();
        snap.logical_items.push(model::RuntimeItem {
            category: model::Category::Database,
            display_name: "postgres".into(),
            root_pid: Some(9999),
            process_ids: vec![9999],
            memory_bytes: 48 << 20,
            cpu_percent: 3.0,
            state: model::RuntimeState::Persistent,
            suspicion: None,
            ports: vec![],
            project: None,
            children: vec![],
        });
        let rs = visible_rows(&snap, Section::All, None, "");
        let line = runtime_summary(&rs, 60).to_string();
        assert!(line.contains("2 items"), "{line}");
        assert!(line.contains("64M RAM"), "16M + 48M:\n{line}");
        assert!(line.contains("3.0% CPU"), "{line}");
    }

    #[test]
    fn runtime_summary_renders_in_pane() {
        let snap = node_fixture();
        let rendered = render_runtime_at(&snap, 100, 24);
        assert!(
            rendered.contains("1 item · 16M RAM"),
            "summary strip visible in pane:\n{rendered}"
        );
    }
}
