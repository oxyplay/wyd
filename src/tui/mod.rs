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
            if h.popup.contains(pos) {
                open_popup_url(snap, app, m.row.saturating_sub(h.popup.y));
            } else {
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
        Some(_) => app.mode = Mode::Details,
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

/// URL fact rows sit right under the popup header block: 1 row of padding,
/// then col_header + item_line + blank.
fn open_popup_url(snap: &RuntimeSnapshot, app: &App, popup_line: u16) {
    open_selected_url(snap, app, popup_line.saturating_sub(4) as usize);
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
        confirm_lines, details_lines, docker_confirm_lines, help_lines, window_title,
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
        assert!(text.contains("http://127.0.0.1:5555"), "{text}");
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
}
