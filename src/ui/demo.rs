//! `herdr-board demo` — the TUI on fixtures, for design review.
//!
//! No database, no network, no herdr. Every state in the design is reachable
//! here: `n` cycles scenarios, the normal keys move and switch views.

use super::app::{Action, key_action};
use super::{fixtures, render};
use crate::ui::state::Screen;
use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::time::Duration;

pub fn run(scenario: &str) -> Result<()> {
    if !fixtures::ALL.contains(&scenario) {
        bail!(
            "unknown scenario `{scenario}`; try one of: {}",
            fixtures::ALL.join(", ")
        );
    }
    let mut ix = fixtures::ALL.iter().position(|s| *s == scenario).unwrap();
    let mut app = fixtures::app(fixtures::ALL[ix]);

    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    let res = (|| -> Result<()> {
        loop {
            term.draw(|f| {
                let area = f.area();
                app.last_height = area.height;
                render::render(f.buffer_mut(), area, &mut app);
            })?;

            if !event::poll(Duration::from_millis(250))? {
                app.expire_message();
                continue;
            }
            if let Event::Key(k) = event::read()?
                && k.kind != KeyEventKind::Release
            {
                // `n` is demo-only: it cycles the scenarios.
                if k.code == KeyCode::Char('n') {
                    ix = (ix + 1) % fixtures::ALL.len();
                    let screen = app.screen;
                    app = fixtures::app(fixtures::ALL[ix]);
                    app.screen = screen;
                    app.flash(format!("scenario: {}", fixtures::ALL[ix]));
                    continue;
                }
                match key_action(&app, k) {
                    Action::Quit => return Ok(()),
                    Action::Move(d) => app.select_delta(d),
                    Action::Help => app.screen = Screen::Help,
                    Action::Back => app.screen = Screen::List,
                    Action::Detail => app.screen = Screen::Detail,
                    Action::Prompt => {
                        app.screen = if app.screen == Screen::Prompt {
                            Screen::Detail
                        } else {
                            Screen::Prompt
                        }
                    }
                    Action::Enter => {
                        if app.rows.iter().any(|(_, r)| *r == super::state::Row::DoneCollapsed)
                            && app.selected().is_none()
                        {
                            app.done_expanded = true;
                        } else {
                            app.screen = Screen::Detail;
                        }
                    }
                    Action::Cancel => {
                        app.confirm = app.selected().map(|v| v.id().to_string());
                    }
                    Action::ConfirmNo | Action::ConfirmYes => app.confirm = None,
                    other => app.flash(format!("{other:?} (demo: no side effects)")),
                }
            }
            app.expire_message();
        }
    })();

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    res
}
