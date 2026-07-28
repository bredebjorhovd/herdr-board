//! `herdr-board demo` — the TUI on fixtures, for design review.
//!
//! No database, no network, no herdr. Every state in the design is reachable
//! here: `n` cycles scenarios, the normal keys move and switch views, and the
//! mouse does what the mouse does on the real board.
//!
//! The demo exists to be *looked at*, so anything it cannot reach is a design
//! state nobody reviews. Two rules keep it honest:
//!
//!   1. [`apply`] is exhaustive over [`Action`]. A new action will not compile
//!      until the demo says what it shows — which is how `m` came to print
//!      `Merge (demo: no side effects)` instead of the merge confirmation.
//!   2. Its guards mirror [`super::app::Board::apply`]. A demo that offers to
//!      cancel a `ready` row shows a screen the board never produces.

use super::app::{Action, MERGE_PREFIX, key_action, mouse_action};
use super::{fixtures, render};
use crate::model::BoardState;
use crate::ui::state::{App, Screen};
use anyhow::{Result, bail};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::time::{Duration, Instant};

pub fn run(scenario: &str) -> Result<()> {
    if !fixtures::ALL.contains(&scenario) {
        bail!(
            "unknown scenario `{scenario}`; try one of: {}",
            fixtures::ALL.join(", ")
        );
    }
    let mut ix = fixtures::ALL.iter().position(|s| *s == scenario).unwrap();
    let mut app = fixtures::app(fixtures::ALL[ix]);
    let mut last_click: Option<(u16, Instant)> = None;

    enable_raw_mode()?;
    let mut out = std::io::stdout();
    // Mouse parity is part of the design, so it is part of the demo: without
    // capture a click never reaches us and the parity cannot be driven at all.
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
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
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    // `n` is demo-only: it cycles the scenarios.
                    if k.code == KeyCode::Char('n') && app.confirm.is_none() {
                        ix = (ix + 1) % fixtures::ALL.len();
                        let screen = app.screen;
                        app = fixtures::app(fixtures::ALL[ix]);
                        app.screen = screen;
                        app.flash(format!("scenario: {}", fixtures::ALL[ix]));
                        continue;
                    }
                    let action = key_action(&app, k);
                    if apply(&mut app, action) {
                        return Ok(());
                    }
                }
                Event::Mouse(m) => {
                    let action = mouse_action(&mut app, m, &mut last_click);
                    if apply(&mut app, action) {
                        return Ok(());
                    }
                }
                _ => {}
            }
            app.expire_message();
        }
    })();

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    res
}

/// Show what the board would show. Returns `true` to quit.
///
/// Side-effectful actions stop at the screen the operator would see — the
/// confirmation, then the outcome line the board flashes on success. The `✓`
/// and `✕` on those lines are the whole point of them, so the demo prints the
/// real strings rather than a placeholder.
fn apply(app: &mut App, action: Action) -> bool {
    match action {
        Action::None => {}
        Action::Quit => return true,
        Action::Move(d) => match &mut app.adopt {
            Some(view) if app.screen == Screen::Adopt => view.move_cursor(d),
            _ => app.select_delta(d),
        },
        Action::Toggle => {
            if let Some(view) = &mut app.adopt {
                view.toggle();
            }
        }
        Action::Help => app.screen = Screen::Help,
        // Fixtures have no attempt history, so the screen shows its empty state.
        Action::Stats => app.screen = Screen::Stats,
        Action::Back => {
            app.adopt = None;
            app.screen = Screen::List;
        }
        Action::Detail => {
            if app.selected().is_some() {
                app.screen = Screen::Detail;
            }
        }
        Action::Prompt => {
            app.screen = if app.screen == Screen::Prompt {
                Screen::Detail
            } else {
                Screen::Prompt
            }
        }
        // The adoption screen writes routing.toml, which the demo has none of,
        // so it stops at the line the board flashes — which is the part worth
        // reviewing: it has to name the filter that was just chosen.
        Action::Enter if app.screen == Screen::Adopt => {
            let Some(view) = app.adopt.take() else {
                return false;
            };
            app.screen = Screen::List;
            let filter = match view.written_labels().as_deref() {
                Some([]) => ", every open issue".to_string(),
                Some(l) => format!(", only {}", l.join(" + ")),
                None => String::new(),
            };
            let (slug, name) = (view.repo.slug.clone(), view.repo.name().to_string());
            app.flash(format!(
                "✓ adopted {slug} — route + polling{filter}. Linear label `{name}` is only a suggestion, commented out in routing.toml"
            ));
        }
        Action::Enter if app.on_unadopted_header() => {
            app.collapsed_unadopted = !app.collapsed_unadopted
        }
        Action::Enter => match app.on_section_header() {
            Some(state) => app.toggle_collapsed(state),
            None => match app.selected().map(|v| (v.state(), v.has_route)) {
                // The picker is a separate process; the demo says so rather
                // than pretending to open one.
                Some((BoardState::Ready | BoardState::Failed, true)) => {
                    app.flash("the dispatch picker opens here")
                }
                Some((BoardState::Ready | BoardState::Failed, false)) => {
                    let id = app.selected().map(|v| v.task.identifier.clone()).unwrap_or_default();
                    app.flash(format!("no route for {id} · add one in routing.toml"))
                }
                Some(_) => app.screen = Screen::Detail,
                None => {}
            },
        },
        Action::Cancel => {
            // Only working and blocked rows hold a pane to kill.
            if let Some(v) = app.selected()
                && v.state().holds_pane()
            {
                app.confirm = Some(v.id().to_string());
            }
        }
        Action::Merge => {
            if let Some(v) = app.selected()
                && v.task.pr_number.is_some()
            {
                app.confirm = Some(format!("{MERGE_PREFIX}{}", v.id()));
            } else {
                app.flash("no pull request on this row");
            }
        }
        Action::ConfirmNo => app.confirm = None,
        Action::ConfirmYes => {
            let Some(id) = app.confirm.take() else {
                return false;
            };
            let bare = id.strip_prefix(MERGE_PREFIX);
            let merging = bare.is_some();
            let key = bare.unwrap_or(&id).to_string();
            let v = app.views.iter().find(|v| v.id() == key);
            let ident = v.map(|v| v.task.identifier.clone()).unwrap_or_default();
            let pr = v.and_then(|v| v.task.pr_number);
            app.flash(if merging {
                match pr {
                    Some(n) => format!("✓ merged PR #{n} for {ident}"),
                    None => format!("✓ merged {ident}"),
                }
            } else {
                format!("✓ cancelled {ident} — the issue is still open")
            });
        }
        Action::Retry => {
            if app.selected().map(|v| v.state()) == Some(BoardState::Failed) {
                app.flash("the dispatch picker opens here");
            }
        }
        Action::MarkDone => {
            if let Some(v) = app.selected()
                && v.state() == BoardState::Failed
            {
                let ident = v.task.identifier.clone();
                app.flash(format!("✓ {ident} marked done on the board"));
            }
        }
        Action::GoToPane => match app.selected().map(|v| (v.task.identifier.clone(), v.pane_id.clone())) {
            Some((_, Some(pane))) => app.flash(format!("herdr focuses {pane}")),
            Some((ident, None)) => app.flash(format!("{ident} has no pane")),
            None => {}
        },
        Action::Open => {
            if let Some(v) = app.selected() {
                let ident = v.task.identifier.clone();
                app.flash(format!("opened {ident} in your browser"));
            }
        }
        // `a` opens the ask, exactly as it does on the board. The two shapes
        // that screen has both need looking at, so the demo reaches both: a
        // repo GitHub answered for, and one it could not be asked about.
        Action::Adopt => {
            if let Some(u) = app.unadopted_selected().cloned() {
                // Already polled — the filter is decided and there is nothing
                // to ask, same guard as the board's.
                if u.missing == crate::adopt::Missing::Route {
                    let (slug, name) = (u.slug.clone(), u.name().to_string());
                    app.flash(format!(
                        "✓ adopted {slug} — route. Linear label `{name}` is only a suggestion, commented out in routing.toml"
                    ));
                    return false;
                }
                // Answered outright: the demo has no network, so the wait the
                // real board draws once has nothing to wait for here.
                let preview = if u.missing == crate::adopt::Missing::Polling {
                    Err("github HTTP 404 for /repos/…".to_string())
                } else {
                    Ok(fixtures::repo_preview())
                };
                app.adopt =
                    Some(crate::ui::state::AdoptView::new(u, Some(preview), Vec::new()));
                app.screen = Screen::Adopt;
            }
        }
        Action::Ignore => {
            if let Some(u) = app.unadopted_selected() {
                let slug = u.slug.clone();
                app.flash(format!(
                    "✓ ignoring {slug} — remove it from `[adopt] ignore` to be offered it again"
                ));
            }
        }
        Action::Sync => app.flash("synced"),
    }
    false
}
