use std::io;

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    widgets::{Block, Borders, Paragraph},
};

/// See what your dev sessions left running.
#[derive(Parser)]
#[command(name = "wyd", version, about)]
struct Cli {}

// M0: hardcoded data matching the SPEC §5.1 mockup; real scanners land in M1+.
const OVERVIEW: &str = "\
Overview

Agents        3
MCP           7
Browsers     14
Dev servers   4
Databases     3
Docker        8
Ports        11
Projects      6
Leftovers     5

⚠ RAM waste
  ~1.6 GB";

const RUNTIME: &str = "\
Runtime

● omp                              312 MB   00:42
  ├ chrome-devtools-mcp             48 MB
  │ └ Chromium ×6                  780 MB
  └ queryknight mcp                 37 MB

● opencode                         420 MB   01:17
  ├ playwright-mcp
  └ Chromium ×8                    1.1 GB

⚠ vite                             181 MB   17h
  :3001 ~/Work/old-project
  likely leftover";

fn main() -> io::Result<()> {
    let _cli = Cli::parse();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = run(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    loop {
        terminal.draw(ui)?;
        if let Event::Key(key) = event::read()?
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            return Ok(());
        }
    }
}

fn ui(frame: &mut Frame) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

    let outer = Block::default()
        .title(" wyd ── RAM 7.2/32G │ CPU 12% ")
        .borders(Borders::ALL);
    let inner = outer.inner(main);
    frame.render_widget(outer, main);

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).areas(inner);
    frame.render_widget(Paragraph::new(OVERVIEW), left);
    frame.render_widget(Paragraph::new(RUNTIME), right);

    frame.render_widget(
        Paragraph::new(" ↑↓ select  enter details  k kill  x clean  r refresh  / filter  q quit")
            .style(Style::default().add_modifier(Modifier::DIM)),
        footer,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn renders_overview_and_runtime_panels() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(super::ui).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        for expected in [
            "wyd",
            "Overview",
            "Agents",
            "Runtime",
            "chrome-devtools-mcp",
            "likely leftover",
            "k kill",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}:\n{rendered}"
            );
        }
    }
}
