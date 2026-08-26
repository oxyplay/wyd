use std::collections::{HashMap, HashSet};

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
};

use crate::classify::short_path;
use crate::config;
use crate::model::{DockerResource, ProcessInfo, RuntimeItem, RuntimeSnapshot, RuntimeState};

use super::rows::{self, Row, Section, fmt_age, fmt_bytes, truncate};
use super::{App, Focus, Mode};

const CYAN: Color = Color::Cyan;
const YELLOW: Color = Color::Yellow;
const RED: Color = Color::LightRed;
const DIM: Color = Color::DarkGray;

fn chrome() -> Style {
    Style::new().fg(CYAN)
}
fn warn() -> Style {
    Style::new().fg(YELLOW)
}
fn hot() -> Style {
    Style::new().fg(RED)
}
fn dim() -> Style {
    Style::new().fg(DIM)
}

fn pane(title: &str, focused: bool) -> Block<'static> {
    let style = if focused {
        chrome().add_modifier(Modifier::BOLD)
    } else {
        dim()
    };
    Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(style)
        .title_style(style)
}

fn popup_rect(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let [_, mid, _] = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .areas(area);
    let [_, pop, _] = Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .areas(mid);
    pop
}

pub(super) struct Hits {
    pub overview: Rect,
    pub list: Rect,
    pub popup: Rect,
}

pub(super) fn hits(area: Rect) -> Hits {
    let [main, _] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
    let inner = Block::default().borders(Borders::ALL).inner(main);
    let [left, right] =
        Layout::horizontal([Constraint::Length(46), Constraint::Min(20)]).areas(inner);
    let overview = Block::default().borders(Borders::ALL).inner(left);
    let body = Block::default().borders(Borders::ALL).inner(right);
    let [_, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(body);
    Hits {
        overview,
        list,
        popup: popup_rect(inner, 58, 50),
    }
}

fn draw_popup(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    accent: Style,
    lines: Vec<Line<'static>>,
) {
    let pop = popup_rect(area, 58, 50);
    frame.render_widget(Clear, pop);
    let fill = Style::new().bg(Color::Black);
    let block = Block::default()
        .title(format!(" {title} "))
        .title_style(accent.add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(accent)
        .padding(Padding::new(2, 2, 1, 1))
        .style(fill);
    let inner = block.inner(pop);
    frame.render_widget(block, pop);
    frame.render_widget(
        Paragraph::new(lines).style(fill).wrap(Wrap { trim: false }),
        inner,
    );
}

/// Ghostty/iTerm tab title: how much is running.
pub(super) fn window_title(snap: &RuntimeSnapshot) -> String {
    let mut parts = vec!["wyd".into()];
    for line in rows::overview(snap) {
        if line.count == 0 {
            continue;
        }
        let Some(tag) = title_tag(line.section) else {
            continue;
        };
        parts.push(format!("{} {tag}", line.count));
    }
    parts.join(" · ")
}

fn title_tag(section: Section) -> Option<&'static str> {
    match section {
        Section::Category(crate::model::Category::Agent) => Some("agents"),
        Section::Category(crate::model::Category::Mcp) => Some("mcp"),
        Section::Category(crate::model::Category::Browser) => Some("browsers"),
        Section::Category(crate::model::Category::DevServer) => Some("srv"),
        Section::Category(crate::model::Category::Database) => Some("db"),
        Section::Category(crate::model::Category::LanguageServer) => Some("lsp"),
        Section::Category(crate::model::Category::Worker) => Some("wk"),
        Section::Docker => Some("docker"),
        Section::Leftovers => Some("left"),
        _ => None,
    }
}

pub fn ui(frame: &mut Frame, snap: &RuntimeSnapshot, app: &mut App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
    let rs = app.rows(snap);
    let title = Line::from(vec![
        Span::styled(" wyd ", chrome().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(
                "RAM {}/{}",
                fmt_bytes(snap.used_memory_bytes),
                fmt_bytes(snap.total_memory_bytes)
            ),
            chrome(),
        ),
        Span::styled(
            format!(" │ CPU {:.0}% │ {} items ", snap.cpu_percent, rs.len()),
            dim(),
        ),
    ]);
    let (border, title_style) = match app.mode {
        Mode::ConfirmKill { force: true } => (hot(), hot().add_modifier(Modifier::BOLD)),
        Mode::ConfirmDocker if app.frozen_docker.iter().any(|r| r.persistent) => (warn(), warn()),
        Mode::ConfirmPrune => (warn(), warn().add_modifier(Modifier::BOLD)),
        _ => (chrome(), chrome()),
    };
    let outer = Block::default()
        .title(title)
        .title_style(title_style)
        .borders(Borders::ALL)
        .border_style(border);
    let inner = outer.inner(main);
    frame.render_widget(outer, main);

    match app.mode {
        Mode::List
        | Mode::Details
        | Mode::ConfirmKill { .. }
        | Mode::ConfirmDocker
        | Mode::ConfirmPrune => {
            let [left, right] =
                Layout::horizontal([Constraint::Length(46), Constraint::Min(20)]).areas(inner);
            frame.render_widget(
                Paragraph::new(overview_lines(snap, app))
                    .block(pane("Overview", app.focus == Focus::Overview)),
                left,
            );
            let block = pane(app.section.title(), app.focus == Focus::Runtime);
            let body = block.inner(right);
            frame.render_widget(block, right);
            let [head, list] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(body);
            let width = list.width as usize;
            follow_selected(app, list.height);
            frame.render_widget(Paragraph::new(col_header(width)), head);
            frame.render_widget(
                Paragraph::new(runtime_lines(snap, app, &rs, width)).scroll((app.scroll, 0)),
                list,
            );
            let pw = popup_rect(inner, 58, 50).width.saturating_sub(2 + 4) as usize;
            match app.mode {
                Mode::Details => {
                    draw_popup(
                        frame,
                        inner,
                        "details",
                        chrome(),
                        details_lines(snap, app, pw),
                    );
                }
                Mode::ConfirmKill { force } => {
                    let (title, accent) = if force {
                        ("force kill", hot())
                    } else {
                        ("terminate", warn())
                    };
                    draw_popup(frame, inner, title, accent, confirm_lines(app, force));
                }
                Mode::ConfirmDocker => {
                    draw_popup(
                        frame,
                        inner,
                        "docker",
                        if app.frozen_docker.iter().any(|r| r.persistent) {
                            warn()
                        } else {
                            chrome()
                        },
                        docker_confirm_lines(app),
                    );
                }
                Mode::ConfirmPrune => {
                    draw_popup(frame, inner, "volume prune", warn(), prune_lines(snap));
                }
                _ => {}
            }
        }
        Mode::Help => {
            frame.render_widget(Paragraph::new(help_lines()), inner);
        }
    }

    frame.render_widget(Paragraph::new(hint(app, snap)).style(chrome()), footer);
}

pub(super) fn follow_selected(app: &mut App, view_h: u16) {
    let h = view_h as usize;
    if h == 0 {
        return;
    }
    let sel = app.selected;
    let scroll = app.scroll as usize;
    if sel < scroll {
        app.scroll = sel as u16;
    } else if sel >= scroll + h {
        app.scroll = (sel + 1 - h) as u16;
    }
}

const RAM_W: usize = 6;
const CPU_W: usize = 6;
const AGE_W: usize = 7;
const STATUS_W: usize = 11;
const WHAT_W: usize = 10;
const FROM_W: usize = 22;
const NAME_MAX: usize = 28;
const GAP: usize = 2;

fn name_width(total: usize) -> usize {
    let meta = WHAT_W + FROM_W + RAM_W + CPU_W + STATUS_W + AGE_W + GAP * 6;
    NAME_MAX.min(total.saturating_sub(meta)).max(10)
}

fn col_header(width: usize) -> Line<'static> {
    let nw = name_width(width);
    let g = " ".repeat(GAP);
    Line::from(format!(
        "{:<nw$}{g}{:<WHAT_W$}{g}{:<FROM_W$}{g}{:>RAM_W$}{g}{:>CPU_W$}{g}{:<STATUS_W$}{g}{:>AGE_W$}",
        "NAME", "WHAT", "FROM", "RAM", "CPU", "STATUS", "AGE"
    ))
    .style(dim())
}

fn select_bar(line: Line<'static>, on: bool) -> Line<'static> {
    if !on {
        return line;
    }
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    Line::from(text).style(Style::new().fg(Color::Black).bg(CYAN))
}

fn hint(app: &App, snap: &RuntimeSnapshot) -> String {
    let keys = &config::Config::global().keys;
    match app.mode {
        Mode::List if app.filtering || !app.query.is_empty() => {
            let caret = if app.filtering { "█" } else { "" };
            format!(" /{}{caret}   enter apply  esc clear", app.query)
        }
        Mode::List => {
            let mut parts = vec![
                "←→ pane".into(),
                "↑↓ move".into(),
                "space mark".into(),
                "enter".into(),
                "/ filter".into(),
                "p projects".into(),
                format!("? help"),
                format!("q quit"),
            ];
            match app.rows(snap).get(app.selected) {
                Some(Row::Item { .. }) => {
                    parts.insert(0, format!("{} kill", keys.kill));
                    parts.insert(1, format!("{} force", keys.force_kill));
                }
                Some(Row::Docker(res)) => {
                    let clean = if res.kind == crate::model::DockerKind::Volume {
                        format!("{} clean(D)", keys.clean)
                    } else {
                        format!("{} clean", keys.clean)
                    };
                    if res.running() {
                        parts.insert(0, format!("{} stop", keys.stop));
                        parts.insert(1, clean);
                    } else {
                        parts.insert(0, clean);
                    }
                }
                Some(Row::DockerAgg { .. }) => {
                    parts.insert(0, format!("{} prune", keys.prune));
                }
                Some(Row::Port { .. }) | Some(Row::Project { .. }) => {}
                None => {}
            }
            if app.section == Section::Docker && snap.docker.prunable_stats().0 > 0 {
                parts.push(format!("{} prune", keys.prune));
            }
            format!(" {}", parts.join("  "))
        }
        Mode::Details => {
            let keys = &config::Config::global().keys;
            format!(
                " {} kill  {} stop  o open  {} clean  esc back",
                keys.kill, keys.stop, keys.clean
            )
        }
        Mode::Help => " esc back".into(),
        Mode::ConfirmKill { force: true } => " y force kill  n/esc cancel".into(),
        Mode::ConfirmKill { force: false } => " y terminate  n/esc cancel".into(),
        Mode::ConfirmPrune => " y prune  n/esc cancel".into(),
        Mode::ConfirmDocker => {
            let any_p = app.frozen_docker.iter().any(|r| r.persistent);
            let any_s = app.frozen_docker.iter().any(|r| !r.persistent);
            match (any_p, any_s) {
                (true, true) => " y remove  D delete volume  n/esc cancel".into(),
                (true, false) => " D delete volume  n/esc cancel".into(),
                (false, true) => " y remove  n/esc cancel".into(),
                (false, false) => " n/esc cancel".into(),
            }
        }
    }
}

fn overview_lines(snap: &RuntimeSnapshot, app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, row) in rows::overview(snap).into_iter().enumerate() {
        let mark = if row.section == app.section {
            "▸"
        } else {
            " "
        };
        let extra = if row.extra.is_empty() {
            String::new()
        } else {
            truncate(&row.extra, 20)
        };
        let text = format!(
            "{mark}{:<18} {:>4} {:>20}",
            truncate(row.label, 18),
            row.count,
            extra
        );
        let style = if row.section == Section::Leftovers && row.count > 0 {
            warn()
        } else if row.count == 0 && row.section != Section::All {
            dim()
        } else if row.section == app.section {
            chrome()
        } else {
            Style::default()
        };
        let on = app.focus == Focus::Overview && i == app.ov_sel;
        lines.push(select_bar(Line::from(text).style(style), on));
    }
    if let Some(p) = &app.project {
        lines.push(Line::from(""));
        lines.push(Line::from(format!("filter {p}")).style(chrome()));
    }
    lines
}

fn runtime_lines(
    snap: &RuntimeSnapshot,
    app: &App,
    rs: &[Row<'_>],
    width: usize,
) -> Vec<Line<'static>> {
    if snap.processes.is_empty() {
        return vec![Line::from(" scanning…").style(dim())];
    }
    if rs.is_empty() {
        return vec![Line::from(" no matching items").style(dim())];
    }
    let by_pid: HashMap<u32, &ProcessInfo> = snap.processes.iter().map(|p| (p.pid, p)).collect();
    let nw = name_width(width);
    rs.iter()
        .enumerate()
        .map(|(idx, row)| {
            let line = match row {
                Row::Item { item, depth, last } => {
                    item_line(item, *depth, *last, &by_pid, &app.marked, idx, nw)
                }
                Row::Docker(res) => docker_line(res, &app.marked, idx, nw),
                Row::DockerAgg {
                    count,
                    bytes,
                    oldest,
                } => {
                    let star = if app.marked.contains(&idx) { "*" } else { " " };
                    let left = pad_name(&format!(" {star}○ anonymous volumes ×{count}"), "", nw);
                    cols(
                        &left,
                        "volume",
                        "—",
                        &fmt_bytes(*bytes),
                        "",
                        "unused",
                        &fmt_age((*oldest).max(0) as u64),
                    )
                }
                Row::Port { port, owner } => {
                    let from = owner
                        .project
                        .as_ref()
                        .map(|p| short_path(&p.root))
                        .unwrap_or_else(|| "—".into());
                    let left = pad_name(&format!(" :{}", port.port), "", nw);
                    cols(&left, &owner.title(), &from, "", "", "", "")
                }
                Row::Project { name, ram, kids } => {
                    let left = pad_name(name, "", nw);
                    cols(
                        &left,
                        "project",
                        &format!("{kids} items"),
                        &fmt_bytes(*ram),
                        "",
                        "",
                        "",
                    )
                }
            };
            select_bar(line, app.focus == Focus::Runtime && idx == app.selected)
        })
        .collect()
}

fn pad_name(title: &str, extra: &str, nw: usize) -> String {
    let extra_n = extra.chars().count();
    let budget = if extra_n > 0 && extra_n < nw {
        nw - extra_n
    } else {
        nw
    };
    let t = truncate(title, budget.max(1));
    let mut s = if extra_n > 0 && t.chars().count() + extra_n <= nw {
        format!("{t}{extra}")
    } else {
        t
    };
    let n = s.chars().count();
    if n < nw {
        s.push_str(&" ".repeat(nw - n));
    }
    s
}

fn cols(
    name: &str,
    what: &str,
    from: &str,
    ram: &str,
    cpu: &str,
    status: &str,
    age: &str,
) -> Line<'static> {
    let g = " ".repeat(GAP);
    Line::from(vec![
        Span::raw(name.to_string()),
        Span::styled(format!("{g}{:<WHAT_W$}", truncate(what, WHAT_W)), dim()),
        Span::styled(format!("{g}{:<FROM_W$}", truncate(from, FROM_W)), chrome()),
        Span::styled(format!("{g}{ram:>RAM_W$}"), chrome()),
        Span::styled(format!("{g}{cpu:>CPU_W$}"), dim()),
        Span::styled(
            format!("{g}{:<STATUS_W$}", truncate(status, STATUS_W)),
            dim(),
        ),
        Span::styled(format!("{g}{age:>AGE_W$}"), dim()),
    ])
}

fn role(item: &RuntimeItem) -> String {
    if item.category == crate::model::Category::DevServer
        && let Some(p) = item.ports.iter().map(|x| x.port).min()
    {
        return format!("srv :{p}");
    }
    let n = item.process_ids.len();
    if n > 1 {
        return format!("{n} procs");
    }
    let port = item.ports.iter().map(|p| p.port).min();
    match item.category {
        crate::model::Category::Agent => "agent".into(),
        crate::model::Category::Mcp => "mcp".into(),
        crate::model::Category::Browser => "browser".into(),
        crate::model::Category::DevServer => port
            .map(|p| format!("srv :{p}"))
            .unwrap_or_else(|| "server".into()),
        crate::model::Category::Database => port
            .map(|p| format!("db :{p}"))
            .unwrap_or_else(|| "db".into()),
        crate::model::Category::LanguageServer => "lsp".into(),
        crate::model::Category::Worker => "worker".into(),
        crate::model::Category::DevService => "svc".into(),
        crate::model::Category::UnknownDev => "dev".into(),
    }
}

fn origin(item: &RuntimeItem, by_pid: &HashMap<u32, &ProcessInfo>) -> String {
    if let Some(p) = &item.project {
        return short_path(&p.root);
    }
    if let Some(cwd) = item
        .process_ids
        .iter()
        .find_map(|pid| by_pid.get(pid).and_then(|p| p.cwd.as_ref()))
    {
        let s = short_path(cwd);
        if s != "~" {
            return s;
        }
    }
    if let Some(pwd) = pwd_along(item, by_pid) {
        return short_path(&pwd);
    }
    if let Some(tty) = item
        .process_ids
        .iter()
        .find_map(|pid| by_pid.get(pid).and_then(|p| p.tty.as_deref()))
    {
        return tty.to_string();
    }
    if item.state == RuntimeState::Suspicious {
        return "orphan".into();
    }
    if item.state == RuntimeState::Persistent {
        "svc".into()
    } else {
        "—".into()
    }
}

fn pwd_along(
    item: &RuntimeItem,
    by_pid: &HashMap<u32, &ProcessInfo>,
) -> Option<std::path::PathBuf> {
    for &start in &item.process_ids {
        let mut pid = start;
        for _ in 0..64 {
            let p = by_pid.get(&pid)?;
            if let Some(pwd) = crate::classify::pwd_from_cmd(&p.command) {
                return Some(pwd);
            }
            pid = p.parent_pid?;
        }
    }
    None
}

fn item_line(
    item: &RuntimeItem,
    depth: usize,
    last: bool,
    by_pid: &HashMap<u32, &ProcessInfo>,
    marked: &HashSet<usize>,
    idx: usize,
    nw: usize,
) -> Line<'static> {
    let indent = "  ".repeat(depth);
    let leftover = item.state == RuntimeState::Suspicious;
    let marker = if leftover && depth == 0 {
        "⚠ "
    } else if depth == 0 {
        "● "
    } else if last {
        "└ "
    } else {
        "├ "
    };
    let proc = item.root_pid.and_then(|pid| by_pid.get(&pid).copied());
    let age = proc
        .map(|p| fmt_age(p.start_time))
        .unwrap_or_else(|| "—".into());
    let star = if marked.contains(&idx) { "*" } else { " " };
    let prefix = format!("{indent}{star}{marker}");
    let left = pad_name(&format!("{prefix}{}", item.display_name), "", nw);
    let cpu = format!("{:.1}%", item.cpu_percent);
    let mut line = cols(
        &left,
        &role(item),
        &origin(item, by_pid),
        &fmt_bytes(item.memory_bytes),
        &cpu,
        "",
        &age,
    );
    if leftover && let Some(span) = line.spans.get_mut(0) {
        span.style = warn();
    }
    line
}

fn docker_line(
    res: &DockerResource,
    marked: &HashSet<usize>,
    idx: usize,
    nw: usize,
) -> Line<'static> {
    let running = res.running() || res.detail == "attached";
    let marker = if running { "● " } else { "○ " };
    let star = if marked.contains(&idx) { "*" } else { " " };
    let from = res.compose.clone().unwrap_or_else(|| "—".into());
    let left = pad_name(&format!(" {star}{marker}{}", res.name), "", nw);
    cols(
        &left,
        res.kind.label(),
        &from,
        &fmt_bytes(res.size_bytes),
        "",
        &res.detail,
        &fmt_age(res.created.max(0) as u64),
    )
}

pub fn details_lines(snap: &RuntimeSnapshot, app: &App, width: usize) -> Vec<Line<'static>> {
    let rs = app.rows(snap);
    match rs.get(app.selected) {
        Some(Row::Item { item, .. }) => item_details(item, snap, width),
        Some(Row::Docker(res)) => docker_details(res, width),
        Some(Row::DockerAgg {
            count,
            bytes,
            oldest,
        }) => {
            let nw = name_width(width);
            let mut lines = vec![
                col_header(width),
                cols(
                    &pad_name(&format!("anonymous volumes ×{count}"), "", nw),
                    "volume",
                    "—",
                    &fmt_bytes(*bytes),
                    "",
                    "unused",
                    &fmt_age((*oldest).max(0) as u64),
                ),
                Line::from(""),
                fact_row("safe", "prune with P — named volumes kept", nw),
            ];
            if *oldest > 0 {
                lines.push(fact_row("oldest", &fmt_age(*oldest as u64), nw));
            }
            lines
        }
        Some(Row::Port { port, owner }) => {
            let nw = name_width(width);
            let mut lines = vec![col_header(width)];
            let from = owner
                .project
                .as_ref()
                .map(|p| short_path(&p.root))
                .unwrap_or_else(|| "—".into());
            lines.push(cols(
                &pad_name(&format!(":{}", port.port), "", nw),
                &owner.display_name,
                &from,
                "",
                "",
                "",
                "",
            ));
            lines.push(Line::from(""));
            lines.push(fact_row("pid", &port.pid.to_string(), nw));
            lines.push(fact_row(
                "addr",
                &format!("{}:{}", port.address, port.port),
                nw,
            ));
            lines
        }
        Some(Row::Project { name, ram, kids }) => {
            let nw = name_width(width);
            vec![
                col_header(width),
                cols(
                    &pad_name(name, "", nw),
                    "project",
                    "",
                    &fmt_bytes(*ram),
                    "",
                    "",
                    "",
                ),
                Line::from(""),
                fact_row("items", &kids.to_string(), nw),
            ]
        }
        None => vec![Line::from(" no item")],
    }
}

fn item_details(item: &RuntimeItem, snap: &RuntimeSnapshot, width: usize) -> Vec<Line<'static>> {
    let by_pid: HashMap<u32, &ProcessInfo> = snap.processes.iter().map(|p| (p.pid, p)).collect();
    let proc = item.root_pid.and_then(|pid| by_pid.get(&pid).copied());
    let nw = name_width(width);
    let mut lines = vec![
        col_header(width),
        item_line(item, 0, true, &by_pid, &HashSet::new(), 0, nw),
        Line::from(""),
    ];
    for p in &item.ports {
        lines.push(fact_row("url", &p.url(), nw));
    }
    if let Some(p) = proc {
        lines.push(fact_row("pid", &p.pid.to_string(), nw));
        lines.push(fact_row(
            "ppid",
            &p.parent_pid
                .map(|n| n.to_string())
                .unwrap_or_else(|| "—".into()),
            nw,
        ));
        if !p.command.is_empty() {
            lines.push(fact_row("cmd", &p.command.join(" "), nw));
        }
        if let Some(cwd) = &p.cwd {
            lines.push(fact_row("cwd", &short_path(cwd), nw));
        }
        if let Some(tty) = &p.tty {
            lines.push(fact_row("tty", tty, nw));
        }
    }
    if let Some(project) = &item.project {
        lines.push(fact_row("project", &short_path(&project.root), nw));
    }
    if item.process_ids.len() > 1 {
        lines.push(fact_row(
            "procs",
            &item
                .process_ids
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(" "),
            nw,
        ));
    }
    if !item.children.is_empty() {
        lines.push(fact_row(
            "children",
            &item
                .children
                .iter()
                .map(|c| c.display_name.clone())
                .collect::<Vec<_>>()
                .join("  "),
            nw,
        ));
    }
    if let Some(s) = &item.suspicion {
        lines.push(Line::from(""));
        lines.push(fact_row("leftover", &format!("score {}", s.score), nw).style(warn()));
        for r in &s.reasons {
            lines.push(fact_row("", r.as_str(), nw).style(warn()));
        }
    }
    lines
}

fn fact_row(label: &str, value: &str, nw: usize) -> Line<'static> {
    cols(&pad_name(label, "", nw), "", value, "", "", "", "")
}

pub fn confirm_lines(app: &App, force: bool) -> Vec<Line<'static>> {
    let action = if force { "force kill" } else { "terminate" };
    let accent = if force { hot() } else { warn() };
    let n = app.frozen.len();
    let pids: Vec<String> = app.frozen.iter().map(|id| id.pid.to_string()).collect();
    vec![
        Line::from(""),
        Line::from(format!(" {} ", app.frozen_title)).style(chrome().add_modifier(Modifier::BOLD)),
        Line::from(format!(
            " {n} process{} · PID + start time rechecked",
            if n == 1 { "" } else { "es" }
        ))
        .style(dim()),
        Line::from(format!(" pids {}", pids.join(" "))).style(dim()),
        Line::from(""),
        // Top-anchored: the popup clips bottom-up on short terminals.
        Line::from(format!(" y {action}  ·  n/esc cancel"))
            .style(accent.add_modifier(Modifier::BOLD)),
        Line::from(""),
    ]
}

pub fn docker_details(res: &DockerResource, width: usize) -> Vec<Line<'static>> {
    let nw = name_width(width);
    let mut lines = vec![
        col_header(width),
        cols(
            &pad_name(&res.name, "", nw),
            res.kind.label(),
            res.compose.as_deref().unwrap_or("—"),
            &fmt_bytes(res.size_bytes),
            "",
            &res.detail,
            &fmt_age(res.created.max(0) as u64),
        ),
        Line::from(""),
        fact_row("id", &res.id, nw),
    ];
    if res.persistent {
        lines.push(Line::from(""));
        lines.push(fact_row("warn", "PERSISTENT DATA", nw).style(warn()));
    }
    lines
}

pub fn prune_lines(snap: &RuntimeSnapshot) -> Vec<Line<'static>> {
    let (count, bytes) = snap.docker.prunable_stats();
    vec![
        Line::from(""),
        Line::from(" Prune anonymous volumes").style(chrome().add_modifier(Modifier::BOLD)),
        Line::from(format!(" {count} volumes · {}", fmt_bytes(bytes))).style(dim()),
        Line::from(" named volumes & data are kept").style(dim()),
        Line::from(""),
        Line::from(" y prune  ·  n/esc cancel").style(warn().add_modifier(Modifier::BOLD)),
        Line::from(""),
    ]
}

pub fn docker_confirm_lines(app: &App) -> Vec<Line<'static>> {
    if app.frozen_docker.is_empty() {
        return vec![Line::from(" no docker target")];
    }
    let any_p = app.frozen_docker.iter().any(|r| r.persistent);
    let any_s = app.frozen_docker.iter().any(|r| !r.persistent);
    let n = app.frozen_docker.len();
    let names: Vec<String> = app
        .frozen_docker
        .iter()
        .map(|r| truncate(&r.name, 24))
        .collect();
    let mut keys = Vec::new();
    if any_s {
        keys.push("y remove");
    }
    if any_p {
        keys.push("D delete volume");
    }
    keys.push("n/esc cancel");
    let mut lines = vec![
        Line::from(""),
        Line::from(format!(
            " Remove {} resource{}?",
            n,
            if n == 1 { "" } else { "s" }
        ))
        .style(chrome().add_modifier(Modifier::BOLD)),
        Line::from(format!(" {}", names.join(" · "))).style(dim()),
    ];
    if any_p {
        lines.push(Line::from(" PERSISTENT DATA — D required").style(warn()));
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from(format!(" {} ", keys.join("  ·  "))).style(warn().add_modifier(Modifier::BOLD)),
    );
    lines.push(Line::from(""));
    lines
}

pub fn help_lines() -> Vec<Line<'static>> {
    let k = &config::Config::global().keys;
    std::iter::once(Line::from("wyd").style(chrome().add_modifier(Modifier::BOLD)))
        .chain(
            [
                "",
                "← → / h l    overview / runtime",
                "↑↓ / j k     move selection",
                "Tab          focus overview / list",
                "backspace    go back (never quits)",
                "space        mark / unmark",
                "/            filter list",
                "p            projects; enter pins a project filter",
                "enter        details (or open overview section)",
                "",
            ]
            .into_iter()
            .map(Line::from),
        )
        .chain([
            Line::from(format!("{}            prune anonymous volumes", k.prune)),
            Line::from(format!("{}            open server URL from details", "o")),
            Line::from(format!("{}            refresh", k.refresh)),
            Line::from(format!("{}            this help", k.help)),
            Line::from(format!("{}            quit", k.quit)),
            Line::from("y            confirm terminate / docker remove / prune"),
            Line::from("D            confirm volume delete"),
            Line::from(""),
            Line::from("Keys: ~/.config/wyd/config.toml  [keys]"),
        ])
        .collect()
}
