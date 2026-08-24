use std::collections::{HashMap, HashSet};

use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

use crate::classify::short_path;
use crate::config;
use crate::model::{DockerResource, ProcessInfo, RuntimeItem, RuntimeSnapshot, RuntimeState};

use super::rows::{self, Row, fmt_age, fmt_bytes, truncate};
use super::{App, Focus, Mode};

pub fn ui(frame: &mut Frame, snap: &RuntimeSnapshot, app: &App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

    let rs = app.rows(snap);
    let outer = Block::default()
        .title(format!(
            " wyd ── RAM {}/{} │ CPU {:.0}% │ {} items ",
            fmt_bytes(snap.used_memory_bytes),
            fmt_bytes(snap.total_memory_bytes),
            snap.cpu_percent,
            rs.len(),
        ))
        .borders(Borders::ALL);
    let inner = outer.inner(main);
    frame.render_widget(outer, main);

    match app.mode {
        Mode::List => {
            let [left, right] =
                Layout::horizontal([Constraint::Length(26), Constraint::Min(20)]).areas(inner);
            frame.render_widget(Paragraph::new(overview_lines(snap, app)), left);
            frame.render_widget(
                Paragraph::new(runtime_lines(snap, app, &rs)).scroll((app.scroll, 0)),
                right,
            );
        }
        Mode::Details => {
            frame.render_widget(Paragraph::new(details_lines(snap, app)), inner);
        }
        Mode::Help => {
            frame.render_widget(Paragraph::new(help_lines()), inner);
        }
        Mode::ConfirmKill { force } => {
            frame.render_widget(Paragraph::new(confirm_lines(app, force)), inner);
        }
        Mode::ConfirmDocker => {
            frame.render_widget(Paragraph::new(docker_confirm_lines(app)), inner);
        }
    }

    frame.render_widget(
        Paragraph::new(hint(app)).style(Style::default().add_modifier(Modifier::DIM)),
        footer,
    );
}

fn hint(app: &App) -> String {
    let keys = &config::Config::global().keys;
    match app.mode {
        Mode::List if app.filtering || !app.query.is_empty() => {
            let caret = if app.filtering { "█" } else { "" };
            format!(" /{}{caret}   enter apply  esc clear", app.query)
        }
        Mode::List => format!(
            " ←→ pane  ↑↓  space mark  enter  {} kill  {} clean  p projects  / filter  {} help  {} quit",
            keys.kill, keys.clean, keys.help, keys.quit
        ),
        Mode::Details => " k kill  x clean  esc back".into(),
        Mode::Help => " esc back".into(),
        Mode::ConfirmKill { force: true } => " y force kill  n/esc cancel".into(),
        Mode::ConfirmKill { force: false } => " y terminate  n/esc cancel".into(),
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
    let mut lines = vec![Line::from("Overview")];
    for (i, row) in rows::overview(snap).into_iter().enumerate() {
        let mark = if row.section == app.section {
            "▸"
        } else {
            " "
        };
        let extra = if row.extra.is_empty() {
            String::new()
        } else {
            format!("  {}", row.extra)
        };
        let text = format!("{mark}{:<12} {:>3}{extra}", row.label, row.count);
        let mut style = Style::default();
        if app.focus == Focus::Overview && i == app.ov_sel {
            style = style.add_modifier(Modifier::REVERSED);
        }
        lines.push(Line::from(text).style(style));
    }
    if let Some(p) = &app.project {
        lines.push(Line::from(""));
        lines.push(Line::from(format!("filter {p}")));
    }
    lines
}

fn runtime_lines(snap: &RuntimeSnapshot, app: &App, rs: &[Row<'_>]) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(app.section.title())];
    if snap.processes.is_empty() {
        lines.push(Line::from(" scanning…"));
        return lines;
    }
    if rs.is_empty() {
        lines.push(Line::from(" no matching items"));
        return lines;
    }
    let by_pid: HashMap<u32, &ProcessInfo> = snap.processes.iter().map(|p| (p.pid, p)).collect();
    for (idx, row) in rs.iter().enumerate() {
        let text = match row {
            Row::Item { item, depth, last } => {
                item_line(item, *depth, *last, &by_pid, &app.marked, idx)
            }
            Row::Docker(res) => docker_line(res, &app.marked, idx),
            Row::Port { port, owner } => {
                let proj = owner
                    .project
                    .as_ref()
                    .map(|p| format!("  {}", short_path(&p.root)))
                    .unwrap_or_default();
                format!(
                    " :{:<6} {:<20}{proj}",
                    port.port,
                    truncate(&owner.title(), 20)
                )
            }
            Row::Project { name, ram, kids } => {
                format!(
                    " {:<22} {:>8}  {kids} items",
                    truncate(name, 22),
                    fmt_bytes(*ram)
                )
            }
        };
        let mut style = Style::default();
        if app.focus == Focus::Runtime && idx == app.selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        lines.push(Line::from(text).style(style));
    }
    lines
}

fn item_line(
    item: &RuntimeItem,
    depth: usize,
    last: bool,
    by_pid: &HashMap<u32, &ProcessInfo>,
    marked: &HashSet<usize>,
    idx: usize,
) -> String {
    let indent = "  ".repeat(depth);
    let marker = if item.state == RuntimeState::Suspicious && depth == 0 {
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
    let mut text = format!(
        "{indent}{star}{marker}{:<28} {:>8} {:>5.1}%  {age}",
        truncate(&item.title(), 28),
        fmt_bytes(item.memory_bytes),
        item.cpu_percent,
    );
    if item.state == RuntimeState::Persistent {
        text.push_str("  persistent");
    }
    if !item.ports.is_empty() {
        let shown: Vec<String> = item.ports.iter().take(3).map(|p| p.label()).collect();
        text.push(' ');
        text.push_str(&shown.join(","));
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
    text
}

fn docker_line(res: &DockerResource, marked: &HashSet<usize>, idx: usize) -> String {
    let marker = if res.detail == "running" || res.detail == "attached" {
        "● "
    } else {
        "○ "
    };
    let warn = if res.persistent { "⚠ " } else { "" };
    let compose = res
        .compose
        .as_ref()
        .map(|c| format!("  {c}"))
        .unwrap_or_default();
    let star = if marked.contains(&idx) { "*" } else { " " };
    format!(
        " {star}{marker}{warn}{:<20} {:<10} {:>8}{compose}",
        truncate(&res.name, 20),
        res.detail,
        fmt_bytes(res.size_bytes),
    )
}

pub fn details_lines(snap: &RuntimeSnapshot, app: &App) -> Vec<Line<'static>> {
    let rs = app.rows(snap);
    match rs.get(app.selected) {
        Some(Row::Item { item, .. }) => item_details(item, snap),
        Some(Row::Docker(res)) => docker_details(res),
        Some(Row::Port { port, owner }) => {
            let mut lines = vec![
                Line::from(format!("{}  {}", port.label(), owner.title())),
                Line::from(""),
                Line::from(format!("pid   {}", port.pid)),
                Line::from(format!("{}:{}", port.address, port.port)),
            ];
            if let Some(p) = &owner.project {
                lines.push(Line::from(format!(
                    "project  {}  {}",
                    p.name,
                    short_path(&p.root)
                )));
            }
            lines
        }
        Some(Row::Project { name, ram, kids }) => vec![
            Line::from(name.clone()),
            Line::from(""),
            Line::from(format!("RAM   {}", fmt_bytes(*ram))),
            Line::from(format!("items {kids}")),
            Line::from("enter  filter other views to this project"),
        ],
        None => vec![Line::from(" no item")],
    }
}

fn item_details(item: &RuntimeItem, snap: &RuntimeSnapshot) -> Vec<Line<'static>> {
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
    if let Some(s) = &item.suspicion {
        lines.push(Line::from(format!("⚠ leftover score {}", s.score)));
        for r in &s.reasons {
            lines.push(Line::from(format!("  · {}", r.as_str())));
        }
    }
    lines
}

pub fn confirm_lines(app: &App, force: bool) -> Vec<Line<'static>> {
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

pub fn docker_details(res: &DockerResource) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(format!("{}  {}", res.name, res.kind.label())),
        Line::from(""),
        Line::from(format!("id      {}", res.id)),
        Line::from(format!("state   {}", res.detail)),
        Line::from(format!("size    {}", fmt_bytes(res.size_bytes))),
    ];
    if let Some(c) = &res.compose {
        lines.push(Line::from(format!("compose {c}")));
    }
    if res.persistent {
        lines.push(Line::from("⚠ PERSISTENT DATA — may contain a database"));
    }
    lines
}

pub fn docker_confirm_lines(app: &App) -> Vec<Line<'static>> {
    if app.frozen_docker.is_empty() {
        return vec![Line::from(" no docker target")];
    }
    let mut lines = Vec::new();
    if app.frozen_docker.iter().any(|r| r.persistent) {
        lines.push(Line::from("⚠ PERSISTENT DATA"));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(format!(
        "Remove {} Docker resource(s)?",
        app.frozen_docker.len()
    )));
    for res in &app.frozen_docker {
        let warn = if res.persistent { "  ⚠ volume" } else { "" };
        lines.push(Line::from(format!(
            "  {} {}  {}{warn}",
            res.kind.label(),
            res.name,
            fmt_bytes(res.size_bytes)
        )));
    }
    if app.frozen_docker.iter().any(|r| r.persistent) {
        lines.push(Line::from(""));
        lines.push(Line::from("Volumes may contain database data."));
        lines.push(Line::from("[D] Delete volumes    [Esc] Cancel"));
    }
    lines
}

pub fn help_lines() -> Vec<Line<'static>> {
    let k = &config::Config::global().keys;
    [
        "wyd",
        "",
        "← →          overview / runtime",
        "↑↓           move selection",
        "space        mark / unmark",
        "/            filter list",
        "p            projects; enter pins a project filter",
        "enter        details (or open overview section)",
        "",
    ]
    .into_iter()
    .map(Line::from)
    .chain([
        Line::from(format!("{}            terminate (confirm with y)", k.kill)),
        Line::from(format!(
            "{}            force kill (confirm with y)",
            k.force_kill
        )),
        Line::from(format!(
            "{}            docker clean (y, or D for volumes)",
            k.clean
        )),
        Line::from(format!("{}            refresh", k.refresh)),
        Line::from(format!("{}            this help", k.help)),
        Line::from(format!("{}            quit", k.quit)),
        Line::from(""),
        Line::from("esc          clear filter / project / back / quit"),
        Line::from("y            confirm terminate / docker remove"),
        Line::from("D            confirm volume delete"),
        Line::from(""),
        Line::from("Keys: ~/.config/wyd/config.toml  [keys]"),
    ])
    .collect()
}
