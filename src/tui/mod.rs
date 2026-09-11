mod draw;
mod rows;

use std::collections::HashSet;
use std::io;
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseEventKind},
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
    },
};
use parking_lot::{Condvar, Mutex, RwLock};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Position, Rect},
};

use crate::actions::process::{self, Identity, Signal};
use crate::config;
use crate::model::DockerResource;
use crate::model::RuntimeSnapshot;
use crate::model::run::Capacity;
use crate::model::run::{RunId, RunState};
use crate::runner::RunView;
use crate::store::{RunFilter, RuntimeStore};

use draw::{hits, ui};
use rows::{Focus, Row, RunDetailData, Section, overview, rows as visible_rows};

const EVENT_POLL: Duration = Duration::from_millis(100);
/// How often the run feed re-reads the durable store.
const RUNS_REFRESH: Duration = Duration::from_millis(1000);
/// Newest runs kept in memory. The store is the bound: nothing older is read.
const RUNS_LIMIT: usize = 200;
/// Event rows kept for the focused run's detail popup.
const RUN_EVENTS_SHOWN: usize = 40;

/// Run list plus the focused run's detail, published by the feed thread. The
/// UI only ever sees a bounded snapshot: the newest `RUNS_LIMIT` runs and at
/// most `RUN_EVENTS_SHOWN` events — never log bytes.
#[derive(Default)]
struct RunFeed {
    version: u64,
    rows: Vec<RunView>,
    /// Live admission capacity, when a supervisor is running.
    capacity: Option<Capacity>,
    detail: Option<RunDetailData>,
    error: Option<String>,
    focus: Option<RunId>,
    /// Set when the UI wants a reload before the next periodic tick; checked
    /// under the same lock that the wait releases, so a wake-up cannot be
    /// lost between a publish and the wait.
    dirty: bool,
    stop: bool,
}

type Feed = Arc<(Mutex<RunFeed>, Condvar)>;

fn new_feed() -> Feed {
    Arc::new((Mutex::new(RunFeed::default()), Condvar::new()))
}

/// Reads the run store off the UI thread: opened once, then every tick
/// reloads the bounded newest-first list and the focused run's events. A
/// missing or unreadable store leaves the rest of the TUI working.
fn run_feed(feed: Feed) {
    let (lock, cv) = &*feed;
    let store = match RuntimeStore::open(&RuntimeStore::default_path()) {
        Ok(s) => s,
        Err(e) => {
            let mut f = lock.lock();
            f.error = Some(format!("run store: {e}"));
            f.version += 1;
            return;
        }
    };
    loop {
        let focus = lock.lock().focus;
        let mut error = None;
        // Prefer the supervisor when it is answering: it knows the observed
        // usage, queue position and limit events the store does not keep.
        let live = crate::server::serve_alive()
            .then(|| {
                crate::runner::client::Client::new()
                    .list(serde_json::json!({ "limit": RUNS_LIMIT }))
                    .ok()
            })
            .flatten();
        let mut rows = match live {
            Some(mut runs) => {
                for v in &mut runs {
                    if v.duration_ms.is_none() && !v.state.is_terminal() {
                        v.duration_ms = Some(now_secs().saturating_sub(v.created_at) * 1000);
                    }
                }
                runs
            }
            None => match store.run_list(&RunFilter {
                limit: Some(RUNS_LIMIT),
                ..RunFilter::default()
            }) {
                Ok(records) => records
                    .iter()
                    .map(|r| {
                        let mut v = RunView::from_record(r);
                        // A running run has no result yet; show elapsed wall time.
                        if v.duration_ms.is_none() && !v.state.is_terminal() {
                            v.duration_ms = Some(now_secs().saturating_sub(v.created_at) * 1000);
                        }
                        v
                    })
                    .collect(),
                Err(e) => {
                    error = Some(format!("run list: {e}"));
                    Vec::new()
                }
            },
        };
        fill_queue(&mut rows, &store);
        // Live counts live in the supervisor, not the store. Read them over
        // the local socket only when a supervisor is actually answering, so
        // the TUI keeps working standalone.
        let capacity = crate::server::serve_alive()
            .then(|| crate::runner::client::Client::new().capacity().ok())
            .flatten();
        let detail = focus.and_then(|id| match load_detail(&store, id) {
            Ok(d) => d,
            Err(e) => {
                error = Some(format!("run detail: {e}"));
                None
            }
        });
        {
            let mut f = lock.lock();
            f.version += 1;
            f.rows = rows;
            f.capacity = capacity;
            f.detail = detail;
            f.error = error;
        }
        let mut guard = lock.lock();
        if guard.stop {
            return;
        }
        if !guard.dirty {
            cv.wait_for(&mut guard, RUNS_REFRESH);
        }
        guard.dirty = false;
        if guard.stop {
            return;
        }
    }
}

fn load_detail(store: &RuntimeStore, id: RunId) -> io::Result<Option<RunDetailData>> {
    let Some(record) = store.run_get(id)? else {
        return Ok(None);
    };
    let mut events = store.run_events(id, 0)?;
    if events.len() > RUN_EVENTS_SHOWN {
        events = events.split_off(events.len() - RUN_EVENTS_SHOWN);
    }
    Ok(Some(RunDetailData { record, events }))
}

/// The store keeps only the final queue wait, and neither the position nor the
/// reason (both live in the supervisor). Reconstruct the FIFO position and the
/// elapsed wait from the bounded list, and read the reason the supervisor
/// recorded once in the `queued` event, so a waiting run explains itself
/// without opening details or talking to the supervisor.
/// ponytail: exact while every queued run fits the bounded newest-200 list.
fn fill_queue(rows: &mut [RunView], store: &RuntimeStore) {
    let now = now_secs();
    // The scheduler deque is FIFO by enqueue; position is its 1-based index.
    // Key on creation time, run id breaking same-second ties.
    // A live view from the supervisor already carries the authoritative
    // position and wait; only reconstruct what the store cannot provide.
    let mut queued: Vec<(u64, i64, usize)> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.state == RunState::Queued && r.queue.position.is_none())
        .map(|(i, r)| (r.created_at, r.run_id.parse::<i64>().unwrap_or(0), i))
        .collect();
    queued.sort_unstable();
    for (pos, &(_, _, i)) in queued.iter().enumerate() {
        let run = &mut rows[i];
        run.queue.position = Some(pos + 1);
        run.queue.waiting_ms = now.saturating_sub(run.created_at).saturating_mul(1000);
        if run.queue.reason.is_none() {
            let reason = queued_reason(store, &run.run_id);
            run.queue.reason = reason;
        }
    }
}

/// The supervisor writes `queued at position N: <reason>` once. The position in
/// that event is the position at enqueue and may be stale, so reuse only the
/// reason text.
fn queued_reason(store: &RuntimeStore, run_id: &str) -> Option<String> {
    let id = RunId(run_id.parse().ok()?);
    let events = store.run_events(id, 0).ok()?;
    let detail = events
        .iter()
        .rev()
        .find(|e| e.kind == "queued")?
        .detail
        .as_deref()?;
    Some(match detail.split_once(": ") {
        Some((_, reason)) => reason.to_string(),
        None => detail.to_string(),
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

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
    /// Mirror of the feed's newest-first run list.
    runs: Vec<RunView>,
    run_detail: Option<RunDetailData>,
    runs_version: u64,
    runs_error: Option<String>,
    capacity: Option<Capacity>,
    feed: Feed,
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
            runs: Vec::new(),
            run_detail: None,
            runs_version: 0,
            runs_error: None,
            capacity: None,
            feed: new_feed(),
        }
    }

    fn rows<'a>(&self, snap: &'a RuntimeSnapshot) -> Vec<Row<'a>> {
        visible_rows(
            snap,
            &self.runs,
            self.section,
            self.project.as_deref(),
            &self.query,
        )
    }

    fn clamp(&mut self, snap: &RuntimeSnapshot) {
        let n = self.rows(snap).len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
        let ov = overview(snap, &self.runs).len();
        if ov == 0 {
            self.ov_sel = 0;
        } else if self.ov_sel >= ov {
            self.ov_sel = ov - 1;
        }
    }

    /// Copy the feed's latest bounded snapshot. Cheap: only when it changed.
    fn sync_runs(&mut self) {
        let feed = Arc::clone(&self.feed);
        let guard = feed.0.lock();
        if guard.version == self.runs_version {
            return;
        }
        self.runs_version = guard.version;
        self.runs = guard.rows.clone();
        self.run_detail = guard.detail.clone();
        self.runs_error = guard.error.clone();
        self.capacity = guard.capacity.clone();
    }

    /// Point the feed at a run so its events load off the UI thread.
    fn focus_run(&mut self, id: RunId) {
        let feed = Arc::clone(&self.feed);
        feed.0.lock().focus = Some(id);
        self.wake_feed();
        self.run_detail = None;
    }

    /// Ask the feed for an immediate reload instead of waiting for its tick.
    fn wake_feed(&self) {
        let feed = Arc::clone(&self.feed);
        {
            let mut f = feed.0.lock();
            f.dirty = true;
        }
        feed.1.notify_all();
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
    let feed = new_feed();
    let worker = {
        let feed = Arc::clone(&feed);
        std::thread::spawn(move || run_feed(feed))
    };
    let result = run(&mut terminal, &snapshot, &force, &feed);
    {
        let (lock, cv) = &*feed;
        lock.lock().stop = true;
        cv.notify_all();
    }
    let _ = worker.join();
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
    feed: &Feed,
) -> io::Result<()> {
    let mut drawn_version = u64::MAX;
    let mut drawn_runs = u64::MAX;
    let mut app = App::new();
    app.feed = Arc::clone(feed);
    loop {
        let snap = snapshot.read();
        app.sync_runs();
        app.clamp(&snap);
        if snap.version != drawn_version || app.runs_version != drawn_runs {
            drawn_version = snap.version;
            drawn_runs = app.runs_version;
            terminal.draw(|f| ui(f, &snap, &mut app))?;
            execute!(
                terminal.backend_mut(),
                SetTitle(draw::window_title(&snap, &app.runs))
            )?;
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
            execute!(
                terminal.backend_mut(),
                SetTitle(draw::window_title(&snap, &app.runs))
            )?;
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
                    app.wake_feed();
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
    let h = hits(area, app.section);
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
                if i < overview(snap, &app.runs).len() {
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
                        let run = match app.rows(snap).get(i) {
                            Some(Row::Run(ri)) => Some(*ri),
                            _ => None,
                        };
                        if let Some(ri) = run {
                            open_run(app, ri);
                        }
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
        let n = overview(snap, &app.runs).len();
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
    if let Some(line) = overview(snap, &app.runs).get(app.ov_sel) {
        app.section = line.section;
        app.focus = Focus::Runtime;
        app.reset_runtime();
    }
}

fn jump_projects(snap: &RuntimeSnapshot, app: &mut App) {
    let ov = overview(snap, &app.runs);
    if let Some(i) = ov.iter().position(|l| l.section == Section::Projects) {
        app.ov_sel = i;
        app.section = Section::Projects;
        app.focus = Focus::Runtime;
        app.reset_runtime();
    }
}

/// Point the feed at the run behind a row so its events load off-thread.
fn open_run(app: &mut App, i: usize) {
    if let Some(id) = app.runs.get(i).and_then(|r| r.run_id.parse::<i64>().ok()) {
        app.focus_run(RunId(id));
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
        Some(Row::Run(i)) => {
            open_run(app, *i);
            app.detail_scroll = 0;
            app.mode = Mode::Details;
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
    use crate::model::run::{
        Capability, CleanupState, EffectiveLimits, Enforcement, LogState, ResourceRequest,
        RunOutcome, RunRecord, RunResult, RunSpec, RunState,
    };
    use crate::model::{self, ProcessInfo, Project, RuntimeSnapshot};
    use crate::store::RunEvent;

    use super::draw::{
        capability_line, capacity_line, confirm_lines, details_lines, docker_confirm_lines,
        help_lines, hint, overview_lines, runtime_summary, window_title,
    };
    use super::rows::{fmt_age, fmt_bytes, fmt_dur, truncate};

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
        let title = window_title(&snap, &[]);
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
        assert_eq!(fmt_dur(0), "0ms");
        assert_eq!(fmt_dur(999), "999ms");
        assert_eq!(fmt_dur(62_000), "1m02s");
        assert_eq!(fmt_dur(3_700_000), "1h01m");
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
                ports: vec![],
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
        assert_eq!(visible_rows(&snap, &[], Section::All, None, "").len(), 3);
        assert_eq!(
            visible_rows(&snap, &[], Section::All, None, "devtools").len(),
            2
        );
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
        assert!(visible_rows(&snap, &[], Section::Leftovers, None, "").is_empty());
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
                    ports: vec![],
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
                    ports: vec![],
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
            ports: vec![],
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
        let rs = visible_rows(&snap, &[], Section::All, None, "");
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
        let rs = visible_rows(&snap, &[], Section::All, None, "");
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

    // ── Managed runs ───────────────────────────────────────────────────
    fn run_view(
        id: i64,
        state: RunState,
        outcome: Option<RunOutcome>,
        cleanup: CleanupState,
    ) -> RunView {
        let finished = state.is_terminal();
        RunView {
            requested: Default::default(),
            effective: Default::default(),
            queue: Default::default(),
            observed_memory_bytes: None,
            observed_processes: None,
            limit_event: None,
            run_id: id.to_string(),
            request_id: format!("req-{id}"),
            argv: vec!["cargo".into(), "test".into(), "--all".into()],
            cwd: "/src/queryknight".into(),
            project_root: Some("/src/queryknight".into()),
            session_id: Some("0123456789abcdef".into()),
            state,
            outcome,
            exit_code: if outcome == Some(RunOutcome::Exited) {
                Some(0)
            } else {
                None
            },
            signal: if outcome == Some(RunOutcome::Signaled) {
                Some(9)
            } else {
                None
            },
            cleanup,
            revision: 3,
            created_at: 1_700_000_000,
            started_at: if finished { Some(1_700_000_000) } else { None },
            finished_at: if finished { Some(1_700_000_062) } else { None },
            duration_ms: if finished { Some(62_000) } else { None },
            detail: None,
            logs: LogState {
                stdout_bytes: 2048,
                stderr_bytes: 0,
                stdout_truncated: false,
                stderr_truncated: false,
            },
            supervisor: Some("4242:boot".into()),
            capabilities: crate::model::run::backend_capabilities(),
            events: Vec::new(),
        }
    }

    fn run_record(id: i64, outcome: RunOutcome, cleanup: CleanupState) -> RunRecord {
        let mut spec = RunSpec::new(
            format!("req-{id}"),
            vec!["cargo".into(), "test".into()],
            "/src/queryknight".into(),
        );
        spec.project_root = Some("/src/queryknight".into());
        let (exit_code, signal) = match outcome {
            RunOutcome::Signaled => (None, Some(9)),
            RunOutcome::SpawnFailed => (None, None),
            _ => (Some(0), None),
        };
        RunRecord {
            id: RunId(id),
            spec,
            state: RunState::Finished,
            effective: Default::default(),
            queue_wait_ms: 0,
            limit_event: None,
            result: Some(RunResult {
                outcome,
                exit_code,
                signal,
                started_at: Some(1_700_000_000),
                finished_at: 1_700_000_062,
                duration_ms: 62_000,
                cleanup,
                detail: None,
            }),
            logs: LogState {
                stdout_bytes: 10 << 20,
                stderr_bytes: 0,
                stdout_truncated: true,
                stderr_truncated: false,
            },
            revision: 3,
            created_at: 1_700_000_000,
            leader: None,
            supervisor: Some("4242:boot".into()),
        }
    }

    /// A running run and a finished run in one list: distinct state, outcome,
    /// duration and command, and the outcome is never mixed with the
    /// leftover heuristic.
    #[test]
    fn runs_section_renders_running_and_finished() {
        let snap = fixture_snapshot();
        let mut app = App::new();
        app.runs = vec![
            run_view(
                2,
                RunState::Finished,
                Some(RunOutcome::Exited),
                CleanupState::Complete,
            ),
            run_view(1, RunState::Running, None, CleanupState::Pending),
        ];
        app.section = Section::Runs;
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, &snap, &mut app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("#2"), "finished run id:\n{rendered}");
        assert!(rendered.contains("#1"), "running run id:\n{rendered}");
        assert!(rendered.contains("finished"), "{rendered}");
        assert!(rendered.contains("running"), "{rendered}");
        assert!(rendered.contains("exited"), "outcome:\n{rendered}");
        assert!(rendered.contains("1m02s"), "duration:\n{rendered}");
        assert!(rendered.contains("queryknight"), "project:\n{rendered}");
        assert!(
            rendered.contains("cargo test --all"),
            "command:\n{rendered}"
        );
        assert!(
            !rendered.contains("leftover"),
            "heuristic leaked:\n{rendered}"
        );

        // Cleanup is a per-run fact, not a shared guess: the finished run is
        // complete, the running one still pending.
        app.run_detail = Some(RunDetailData {
            record: run_record(2, RunOutcome::Exited, CleanupState::Complete),
            events: vec![],
        });
        app.mode = Mode::Details;
        terminal.draw(|f| ui(f, &snap, &mut app)).unwrap();
        let finished = format!("{:?}", terminal.backend().buffer());
        assert!(finished.contains("complete"), "cleanup:\n{finished}");
        app.selected = 1;
        app.run_detail = None;
        terminal.draw(|f| ui(f, &snap, &mut app)).unwrap();
        let running = format!("{:?}", terminal.backend().buffer());
        assert!(running.contains("pending"), "cleanup:\n{running}");
    }

    #[test]
    fn run_details_separate_exit_code_signal_and_cleanup() {
        let snap = fixture_snapshot();
        let mut app = App::new();
        let mut view = run_view(
            7,
            RunState::Finished,
            Some(RunOutcome::Signaled),
            CleanupState::Incomplete,
        );
        view.logs.stdout_bytes = 10 << 20;
        view.logs.stdout_truncated = true;
        app.runs = vec![view];
        app.run_detail = Some(RunDetailData {
            record: run_record(7, RunOutcome::Signaled, CleanupState::Incomplete),
            events: vec![
                RunEvent {
                    revision: 0,
                    at: 1,
                    kind: "created".into(),
                    detail: None,
                },
                RunEvent {
                    revision: 2,
                    at: 2,
                    kind: "finished".into(),
                    detail: Some("signaled".into()),
                },
            ],
        });
        app.section = Section::Runs;
        app.mode = Mode::Details;
        let text = join_lines(details_lines(&snap, &app, 100));
        assert!(text.contains("command"), "{text}");
        assert!(text.contains("cargo test --all"), "{text}");
        assert!(text.contains("exit code"), "{text}");
        assert!(text.contains("signal"), "{text}");
        assert!(text.contains("timeout"), "{text}");
        assert!(text.contains("10m00s"), "timeout value:\n{text}");
        assert!(text.contains("incomplete"), "cleanup:\n{text}");
        assert!(text.contains("supervisor"), "{text}");
        assert!(text.contains("4242:boot"), "{text}");
        assert!(text.contains("10M"), "log bytes:\n{text}");
        assert!(text.contains("truncated"), "truncation marker:\n{text}");
        assert!(text.contains("created"), "events:\n{text}");
        assert!(text.contains("finished"), "events:\n{text}");
        // Heuristic session scoring must never appear on a run.
        assert!(!text.contains("score"), "{text}");
        assert!(!text.contains("leftover"), "{text}");
    }

    #[test]
    fn run_detail_renders_control_chars_safely() {
        let snap = fixture_snapshot();
        let mut app = App::new();
        let mut view = run_view(
            9,
            RunState::Finished,
            Some(RunOutcome::SpawnFailed),
            CleanupState::Unknown,
        );
        view.argv = vec!["bad\u{7}\u{0}cmd".into()];
        view.detail = Some("spawn failed: \u{1b}[31mboom\u{0}".into());
        app.runs = vec![view];
        let mut record = run_record(9, RunOutcome::SpawnFailed, CleanupState::Unknown);
        record.result.as_mut().unwrap().detail = Some("spawn failed: \u{1b}[31mboom\u{0}".into());
        app.run_detail = Some(RunDetailData {
            record,
            events: vec![],
        });
        app.section = Section::Runs;
        app.mode = Mode::Details;
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, &snap, &mut app)).unwrap();
        let text = join_lines(details_lines(&snap, &app, 100));
        assert!(text.contains("spawn failed"), "{text}");
        assert!(text.contains("loading…"), "missing events state:\n{text}");
    }

    #[test]
    fn runs_section_empty_renders_without_panic() {
        let snap = fixture_snapshot();
        let mut app = App::new();
        app.section = Section::Runs;
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, &snap, &mut app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("no runs"), "{rendered}");
        assert!(rendered.contains("Runs"), "sidebar row:\n{rendered}");
    }

    /// A queued run shows its 1-based position, the wait and a short reason in
    /// the row itself, so waiting is visible without opening details.
    #[test]
    fn queued_run_row_shows_position_wait_and_reason() {
        let snap = fixture_snapshot();
        let mut app = App::new();
        let mut view = run_view(4, RunState::Queued, None, CleanupState::Pending);
        view.queue.position = Some(2);
        view.queue.waiting_ms = 12_000;
        view.queue.reason = Some("project at limit".into());
        app.runs = vec![view];
        app.section = Section::Runs;
        let backend = TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, &snap, &mut app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("queued"), "state:\n{rendered}");
        assert!(rendered.contains("q#2"), "queue position:\n{rendered}");
        assert!(rendered.contains("12.0s"), "queue wait:\n{rendered}");
        assert!(
            rendered.contains("project at limit"),
            "queue reason:\n{rendered}"
        );
    }

    /// Details report the request and what was applied side by side, name the
    /// difference, and label observed usage as a measurement with its source.
    #[test]
    fn run_details_show_requested_effective_and_observation() {
        let snap = fixture_snapshot();
        let mut app = App::new();
        let mut view = run_view(7, RunState::Running, None, CleanupState::Pending);
        view.requested = ResourceRequest {
            memory_bytes: Some(512 << 20),
            cpu_millicores: Some(2000),
            processes: Some(8),
            enforcement: Enforcement::Hard,
            queue_timeout: Some(Duration::from_secs(300)),
        };
        view.effective = EffectiveLimits {
            cpu_millicores: None,
            processes: None,
            memory_bytes: Some(512 << 20),
            enforcement: Enforcement::Monitored,
            metric: Some("sum of RSS over the process group, sampled every 200 ms".into()),
            backend: "process_group".into(),
        };
        view.observed_memory_bytes = Some(900 << 20);
        view.observed_processes = Some(5);
        app.runs = vec![view];
        app.section = Section::Runs;
        app.mode = Mode::Details;
        let text = join_lines(details_lines(&snap, &app, 120));
        assert!(text.contains("requested"), "{text}");
        assert!(text.contains("enforce hard"), "{text}");
        assert!(text.contains("effective"), "{text}");
        assert!(text.contains("enforce monitored"), "{text}");
        assert!(text.contains("differs"), "difference is named:\n{text}");
        assert!(text.contains("hard → monitored"), "{text}");
        assert!(text.contains("sum of RSS"), "metric source:\n{text}");
        assert!(text.contains("observation, not a limit"), "{text}");
        assert!(text.contains("5 procs"), "{text}");
    }

    /// A reservation is a budget, never RAM; and `Monitored` must not render
    /// like `Available`.
    #[test]
    fn reservation_is_not_ram_and_monitored_is_distinct() {
        let snap = fixture_snapshot();
        let mut app = App::new();
        let mut view = run_view(3, RunState::Running, None, CleanupState::Pending);
        view.requested.memory_bytes = Some(512 << 20);
        view.effective.memory_bytes = Some(512 << 20);
        view.effective.enforcement = Enforcement::Monitored;
        view.capabilities.aggregate_memory_limit = Capability::Monitored("rss".into());
        app.runs = vec![view];
        app.section = Section::Runs;
        app.mode = Mode::Details;
        let lines = details_lines(&snap, &app, 120);
        let reserved = lines
            .iter()
            .map(|l| l.to_string())
            .find(|l| l.starts_with("reserved"))
            .expect("reserved row");
        assert!(reserved.contains("budget"), "{reserved}");
        assert!(reserved.contains("not measured memory"), "{reserved}");
        assert!(
            !reserved.contains("RAM"),
            "reservation labelled as RAM: {reserved}"
        );

        let available = capability_line("memory", &Capability::Available);
        let monitored = capability_line("memory", &Capability::Monitored("rss".into()));
        assert!(available.to_string().contains("available"));
        assert!(monitored.to_string().contains("monitored"));
        assert_ne!(
            available.spans[1].style, monitored.spans[1].style,
            "monitored must not render like available"
        );
    }
    #[test]
    fn capacity_line_shows_live_counts_only_when_a_supervisor_answers() {
        let text = |line: &ratatui::text::Line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        let live = Capacity {
            limits: crate::model::run::LimitSummary {
                max_parallel: 3,
                max_parallel_per_project: 1,
                max_queued: 8,
                queue_timeout_secs: 60,
                memory_budget_bytes: 2 * 1024 * 1024 * 1024,
                default_run_memory_bytes: 256 * 1024 * 1024,
                starvation_after_secs: 60,
                cpu_budget_millicores: None,
                pids_budget: None,
            },
            running: 2,
            queued: 1,
            slots_free: 1,
            over_parallel_limit: true,
            aggregate_memory_max_bytes: Some(2 * 1024 * 1024 * 1024),
            aggregate_cpu_millicores: Some(2_000),
            aggregate_pids_max: Some(256),
            reserved_memory_bytes: 512 * 1024 * 1024,
            projects: Vec::new(),
            queue: Vec::new(),
            reservation_note: String::new(),
            capabilities: crate::model::run::backend_capabilities(),
        };
        let rendered = text(&capacity_line(200, Some(&live)));
        assert!(rendered.contains("2/3 used"), "{rendered}");
        assert!(rendered.contains("1 waiting"), "{rendered}");
        assert!(rendered.contains("budget, not RAM"), "{rendered}");
        assert!(rendered.contains("above the new limit"), "{rendered}");
        assert!(rendered.contains("kernel cap"), "{rendered}");
        assert!(rendered.contains("cpu 2000m"), "{rendered}");
        assert!(rendered.contains("pids 256"), "{rendered}");
        assert!(!rendered.contains("unavailable"), "{rendered}");

        // Without a supervisor the configured limits are shown and the live
        // counts are honestly marked unavailable.
        let standalone = text(&capacity_line(200, None));
        assert!(
            standalone.contains("live counts unavailable"),
            "{standalone}"
        );
    }
}
