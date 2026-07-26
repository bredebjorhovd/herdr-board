//! The board pane: event loop, key and mouse handling.
//!
//! herdr is prefix-key native, so the board binds **plain single keys only** and
//! never touches `ctrl+b` — see [`key_action`], which refuses every control
//! chord so the prefix always reaches herdr.

use super::render;
use super::state::*;
use crate::config::{Paths, RoutingConfig};
use crate::db::Db;
use crate::dispatch;
use crate::herdr::Herdr;
use crate::log::Logger;
use crate::model::BoardState;
use crate::sync::{SyncEngine, meta};
use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::Stdout;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What a keypress or click means. Pure, so the whole key map is testable
/// without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Move(isize),
    /// Context-dependent: dispatch, open detail, or expand the done section.
    Enter,
    Detail,
    Prompt,
    Back,
    GoToPane,
    Open,
    Retry,
    MarkDone,
    Cancel,
    Sync,
    Help,
    ConfirmYes,
    ConfirmNo,
}

/// The key map (design spec: Interactions).
///
/// Returns [`Action::None`] for anything carrying a modifier, which is what
/// keeps `ctrl+b` — and every other herdr chord — flowing through untouched.
pub fn key_action(app: &App, key: KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    // The board must never swallow the prefix.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        || key.modifiers.contains(KeyModifiers::ALT)
        || key.modifiers.contains(KeyModifiers::SUPER)
    {
        return Action::None;
    }

    // An inline confirmation takes only y/n/esc.
    if app.confirm.is_some() {
        return match key.code {
            KeyCode::Char('y') => Action::ConfirmYes,
            KeyCode::Char('n') | KeyCode::Esc => Action::ConfirmNo,
            _ => Action::None,
        };
    }

    match key.code {
        KeyCode::Char('j') | KeyCode::Down => Action::Move(1),
        KeyCode::Char('k') | KeyCode::Up => Action::Move(-1),
        KeyCode::Enter => Action::Enter,
        KeyCode::Char('l') => Action::Detail,
        KeyCode::Char('p') => Action::Prompt,
        KeyCode::Char('h') | KeyCode::Esc => Action::Back,
        KeyCode::Char('g') => Action::GoToPane,
        KeyCode::Char('o') => Action::Open,
        KeyCode::Char('r') => Action::Retry,
        KeyCode::Char('d') => Action::MarkDone,
        KeyCode::Char('x') => Action::Cancel,
        KeyCode::Char('s') => Action::Sync,
        KeyCode::Char('?') => Action::Help,
        KeyCode::Char('q') => Action::Quit,
        _ => Action::None,
    }
}

/// Mouse parity is required — herdr is mouse-first. Click selects, double-click
/// is `enter`, and a click on a footer hint fires that hint's action. There are
/// **no hover states**; terminals do not hover reliably.
pub fn mouse_action(app: &mut App, m: MouseEvent, last_click: &mut Option<(u16, Instant)>) -> Action {
    if m.kind != MouseEventKind::Down(MouseButton::Left) {
        return Action::None;
    }
    let y = m.row;

    // Footer hints first — they sit on the last row.
    for (x1, x2, ch) in app.footer_hits.clone() {
        if y + 1 == app.last_height && m.column >= x1 && m.column < x2 {
            return match ch {
                '\r' => Action::Enter,
                '\u{1b}' => Action::Back,
                'g' => Action::GoToPane,
                'o' => Action::Open,
                'x' => Action::Cancel,
                'r' => Action::Retry,
                'd' => Action::MarkDone,
                's' => Action::Sync,
                'h' => Action::Back,
                'p' => Action::Prompt,
                '?' => Action::Help,
                _ => Action::None,
            };
        }
    }

    let Some((_, row)) = app.rows.iter().find(|(ry, _)| *ry == y).cloned() else {
        return Action::None;
    };
    match row {
        Row::DoneCollapsed => Action::Enter,
        Row::Section(_) => Action::None,
        Row::Task(id) => {
            let double = last_click
                .map(|(ly, at)| ly == y && at.elapsed() < Duration::from_millis(400))
                .unwrap_or(false);
            app.selected_id = Some(id);
            *last_click = Some((y, Instant::now()));
            if double { Action::Enter } else { Action::None }
        }
    }
}

pub struct Board {
    pub app: App,
    pub engine: SyncEngine,
    pub herdr: Herdr,
    pub log: Arc<Logger>,
    pub paths: Paths,
    /// Last seen mtime of routing.toml.
    routing_mtime: Option<std::time::SystemTime>,
}

impl Board {
    /// Re-read `routing.toml` when it changes on disk.
    fn reload_routing(&mut self) {
        let path = self.paths.routing();
        let Some(mtime) = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok()) else {
            return;
        };
        if self.routing_mtime == Some(mtime) {
            return;
        }
        self.routing_mtime = Some(mtime);
        let cfg = RoutingConfig::load_or_default(&path);
        self.log
            .info(format!("reloaded routing.toml ({} routes)", cfg.routes.len()));
        self.engine.cfg = cfg;
    }

    /// Select a task by id, revealing it if it sits in the collapsed section.
    ///
    /// Arriving via the link handler lands on the **list** with the row
    /// selected, not in its detail view — detail hides the queue the operator
    /// came to see.
    pub fn select_task(&mut self, task_id: &str) {
        if !self.app.views.iter().any(|v| v.id() == task_id) {
            self.app
                .flash(format!("{task_id} is not on the board"));
            return;
        }
        if !self.app.visible_task_ids().iter().any(|i| i == task_id) {
            // It is in the collapsed `done` section; expand rather than
            // silently doing nothing.
            self.app.done_expanded = true;
        }
        self.app.selected_id = Some(task_id.to_string());
        self.app.screen = Screen::List;
    }

    /// Rebuild the view model from the database.
    pub fn reload(&mut self) -> Result<()> {
        // Routing is edited while the board is open — that is the whole setup
        // loop — so a cached config silently mis-routes every row until the
        // pane is reopened.
        self.reload_routing();
        let tasks = self.engine.db.load_tasks()?;
        let views = build_views(tasks, &self.engine.cfg, &self.paths);
        let sync = read_sync_status(&self.engine)?;
        self.app.refresh(views, sync);
        self.app.setup_hints = setup_hints(&self.engine.cfg, &self.paths);
        Ok(())
    }

    fn apply(&mut self, action: Action) -> Result<()> {
        match action {
            Action::None => {}
            Action::Quit => self.app.should_quit = true,
            Action::Move(d) => self.app.select_delta(d),
            Action::Help => self.app.screen = Screen::Help,
            Action::Back => {
                self.app.screen = Screen::List;
            }
            Action::Detail => {
                if self.app.selected().is_some() {
                    self.app.screen = Screen::Detail;
                }
            }
            Action::Prompt => {
                // `p` toggles: again returns to the summary.
                self.app.screen = if self.app.screen == Screen::Prompt {
                    Screen::Detail
                } else {
                    Screen::Prompt
                };
            }
            Action::Enter => self.on_enter()?,
            Action::Open => self.open_in_browser(),
            Action::GoToPane => self.go_to_pane(),
            Action::Sync => self.sync_now(),
            Action::Cancel => {
                if let Some(v) = self.app.selected()
                    && v.state().holds_pane()
                {
                    self.app.confirm = Some(v.id().to_string());
                }
            }
            Action::ConfirmNo => self.app.confirm = None,
            Action::ConfirmYes => self.do_cancel()?,
            Action::Retry => self.retry()?,
            Action::MarkDone => self.mark_done()?,
        }
        Ok(())
    }

    fn on_enter(&mut self) -> Result<()> {
        // On the collapsed done header, enter expands it in place.
        if self.app.screen == Screen::List
            && self.app.selected().is_none_or(|v| v.state() != BoardState::Done)
            && self.app.rows.iter().any(|(_, r)| *r == Row::DoneCollapsed)
            && self.app.selected().is_none()
        {
            self.app.done_expanded = true;
            return Ok(());
        }
        let Some(v) = self.app.selected().cloned() else {
            return Ok(());
        };
        match v.state() {
            BoardState::Ready | BoardState::Failed => {
                if !v.has_route {
                    self.app.flash(format!(
                        "no route for {} · add one in routing.toml",
                        v.task.identifier
                    ));
                    return Ok(());
                }
                self.open_picker(v.id())?;
            }
            BoardState::Working | BoardState::Blocked if self.app.screen == Screen::Prompt => {
                // Pressing enter here states the fact rather than doing nothing.
                self.app
                    .flash(format!("{} is already working", v.task.identifier));
            }
            _ => self.app.screen = Screen::Detail,
        }
        Ok(())
    }

    /// Open the dispatch popup.
    ///
    /// The popup is a separate process with no pane id and no access to our
    /// memory, so the selected task is handed over through the database — which
    /// is already the bus between our processes.
    fn open_picker(&mut self, task_id: &str) -> Result<()> {
        self.engine.db.meta_set(PICKER_TASK, task_id)?;
        self.engine.db.meta_set(PICKER_RESULT, "")?;
        match self.herdr.plugin_pane_open("picker") {
            Ok(_) => {}
            Err(e) => self.app.flash(format!("could not open the picker: {e}")),
        }
        Ok(())
    }

    fn open_in_browser(&mut self) {
        let Some(v) = self.app.selected() else { return };
        // On a review row `o` opens the PR, which is what the footer promises.
        let url = if v.state() == BoardState::Review {
            v.task.pr_url.clone().unwrap_or_else(|| v.task.url.clone())
        } else {
            v.task.url.clone()
        };
        let ident = v.task.identifier.clone();
        match open_url(&url) {
            Ok(()) => self.app.flash(format!("opened {ident} in your browser")),
            Err(e) => self.app.flash(format!("could not open {url}: {e}")),
        }
    }

    fn go_to_pane(&mut self) {
        let Some(v) = self.app.selected() else { return };
        let Some(pane) = v.pane_id.clone() else {
            self.app
                .flash(format!("{} has no pane", v.task.identifier));
            return;
        };
        match self.herdr.focus_pane(&pane) {
            Ok(()) => self.app.should_quit = false,
            Err(e) => self.app.flash(format!("could not focus {pane}: {e}")),
        }
    }

    fn sync_now(&mut self) {
        self.app.flash("syncing…");
        if let Err(e) = self.engine.sync_once(Some(&self.herdr)) {
            self.app.flash(format!("sync failed: {e}"));
        } else {
            let _ = self.reload();
            self.app.flash("synced");
        }
    }

    fn do_cancel(&mut self) -> Result<()> {
        let Some(id) = self.app.confirm.take() else {
            return Ok(());
        };
        let Some(task) = self.engine.db.get_task(&id)? else {
            return Ok(());
        };
        match dispatch::cancel(&self.engine, &self.herdr, &self.log, &task) {
            Ok(()) => {
                self.app
                    .flash(format!("cancelled {} — the issue is still open", task.identifier));
            }
            Err(e) => self.app.flash(format!("cancel failed: {e}")),
        }
        self.engine.rederive_all()?;
        self.reload()
    }

    fn retry(&mut self) -> Result<()> {
        let Some(v) = self.app.selected().cloned() else {
            return Ok(());
        };
        if v.state() != BoardState::Failed {
            return Ok(());
        }
        // A retry clears a previous `mark done` so the row can move again.
        self.engine.db.set_local_done(v.id(), false)?;
        self.open_picker(v.id())
    }

    fn mark_done(&mut self) -> Result<()> {
        let Some(v) = self.app.selected().cloned() else {
            return Ok(());
        };
        if v.state() != BoardState::Failed {
            return Ok(());
        }
        // A local terminal override. Without it the next poll would recompute
        // `open` upstream and the row would come straight back — a key that
        // undoes itself.
        self.engine.db.set_local_done(v.id(), true)?;
        self.engine.rederive_all()?;
        self.reload()?;
        self.app
            .flash(format!("{} marked done on the board", v.task.identifier));
        Ok(())
    }
}

pub const PICKER_TASK: &str = "picker_task";
pub const PICKER_RESULT: &str = "picker_result";
/// Pane id of the running board, so the link handler can focus it instead of
/// opening a second copy.
pub const BOARD_PANE: &str = "board_pane";
/// A task the board should select on its next tick — how arriving via a link
/// lands on the right row.
pub const SELECT_TASK: &str = "select_task";

/// Read header status out of the database.
pub fn read_sync_status(engine: &SyncEngine) -> Result<SyncStatus> {
    let age = |key: &str| -> Option<i64> {
        let v = engine.db.meta_get(key).ok().flatten()?;
        let t = chrono::DateTime::parse_from_rfc3339(&v).ok()?;
        Some((chrono::Utc::now() - t.with_timezone(&chrono::Utc))
            .num_seconds()
            .max(0))
    };
    // The freshest successful poll across the configured sources.
    let ok = [meta::LINEAR_LAST_OK, meta::GITHUB_LAST_OK]
        .iter()
        .filter_map(|k| age(k))
        .min();
    Ok(SyncStatus {
        linear: engine.health(crate::model::Source::Linear),
        github: engine.health(crate::model::Source::Github),
        last_cycle_secs: age(meta::LAST_SYNC),
        last_source_ok_secs: ok,
        interval_secs: engine.cfg.sync.interval_secs(),
    })
}

fn open_url(url: &str) -> Result<()> {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(cmd)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

type Term = Terminal<CrosstermBackend<Stdout>>;

pub fn run(mut board: Board) -> Result<()> {
    let mut term = setup()?;
    let res = event_loop(&mut term, &mut board);
    // Deregister before leaving, so the toggle does not try to close a pane
    // that has already gone.
    let _ = board.engine.db.meta_set(BOARD_PANE, "");
    teardown(&mut term)?;
    res
}

fn setup() -> Result<Term> {
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn teardown(term: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    term.show_cursor()?;
    Ok(())
}

fn event_loop(term: &mut Term, board: &mut Board) -> Result<()> {
    let db_path = board.paths.db();
    let mut last_mtime = mtime(&db_path);
    let mut last_tick = Instant::now();
    let mut last_click: Option<(u16, Instant)> = None;
    board.reload()?;

    // Record which pane we are, so the link handler focuses this board rather
    // than opening a second one.
    if let Ok(pane) = std::env::var("HERDR_PANE_ID")
        && !pane.is_empty()
    {
        let _ = board.engine.db.meta_set(BOARD_PANE, &pane);
    }

    loop {
        term.draw(|f| {
            let area = f.area();
            board.app.last_height = area.height;
            render::render(f.buffer_mut(), area, &mut board.app);
        })?;

        // Elapsed counters tick once a second and are the only motion here.
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(k) => {
                    let action = key_action(&board.app, k);
                    board.apply(action)?;
                }
                Event::Mouse(m) => {
                    let action = mouse_action(&mut board.app, m, &mut last_click);
                    board.apply(action)?;
                }
                Event::Resize(..) => {}
                _ => {}
            }
        }

        board.app.expire_message();

        // Re-read on the tick, and immediately when the daemon writes.
        let now_mtime = mtime(&db_path);
        if last_tick.elapsed() >= Duration::from_secs(1) || now_mtime != last_mtime {
            last_tick = Instant::now();
            last_mtime = now_mtime;
            board.reload()?;
            // A dispatch that happened in the popup reports back through meta.
            if let Ok(Some(msg)) = board.engine.db.meta_get(PICKER_RESULT)
                && !msg.is_empty()
            {
                board.app.flash(msg);
                let _ = board.engine.db.meta_set(PICKER_RESULT, "");
            }
            // A link click asked for a row.
            if let Ok(Some(id)) = board.engine.db.meta_get(SELECT_TASK)
                && !id.is_empty()
            {
                board.select_task(&id);
                let _ = board.engine.db.meta_set(SELECT_TASK, "");
            }
        }

        if board.app.should_quit {
            return Ok(());
        }
    }
}

fn mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
}

/// Build a board from the plugin's own config and state.
pub fn open_board(paths: Paths, log: Arc<Logger>) -> Result<Board> {
    let cfg = RoutingConfig::load_or_default(&paths.routing());
    let db = Db::open(&paths.db())?;
    let herdr = Herdr::discover(log.clone());
    let engine = crate::cli::build_engine(db, cfg, paths.clone(), log.clone())?;
    let tasks = engine.db.load_tasks()?;
    let views = build_views(tasks, &engine.cfg, &paths);
    let sync = read_sync_status(&engine)?;
    let mut app = App::new(
        views,
        sync,
        paths.routing().to_string_lossy().into_owned(),
    );
    app.setup_hints = setup_hints(&engine.cfg, &paths);
    Ok(Board {
        app,
        engine,
        herdr,
        log,
        paths,
        routing_mtime: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::fixtures;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn the_board_never_swallows_the_prefix() {
        // ctrl+b must reach herdr untouched — the single hardest interaction
        // rule in the design.
        let app = fixtures::app(fixtures::POPULATED);
        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(key_action(&app, ctrl_b), Action::None);
        // And no other control chord is claimed either.
        for c in ['a', 'c', 'd', 'x', 'q', 's'] {
            let k = KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
            assert_eq!(key_action(&app, k), Action::None, "ctrl+{c} was claimed");
        }
    }

    #[test]
    fn the_documented_keys_map_to_their_actions() {
        let app = fixtures::app(fixtures::POPULATED);
        for (c, want) in [
            ('j', Action::Move(1)),
            ('k', Action::Move(-1)),
            ('l', Action::Detail),
            ('p', Action::Prompt),
            ('h', Action::Back),
            ('g', Action::GoToPane),
            ('o', Action::Open),
            ('r', Action::Retry),
            ('d', Action::MarkDone),
            ('x', Action::Cancel),
            ('s', Action::Sync),
            ('?', Action::Help),
        ] {
            assert_eq!(key_action(&app, key(c)), want, "key {c}");
        }
        assert_eq!(
            key_action(&app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Enter
        );
        assert_eq!(
            key_action(&app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Back
        );
    }

    #[test]
    fn arrows_and_vim_keys_agree() {
        let app = fixtures::app(fixtures::POPULATED);
        assert_eq!(
            key_action(&app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            key_action(&app, key('j'))
        );
        assert_eq!(
            key_action(&app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            key_action(&app, key('k'))
        );
    }

    #[test]
    fn a_pending_confirmation_takes_only_yes_no_and_esc() {
        let mut app = fixtures::app(fixtures::POPULATED);
        app.confirm = Some("linear:LIN-138".into());
        assert_eq!(key_action(&app, key('y')), Action::ConfirmYes);
        assert_eq!(key_action(&app, key('n')), Action::ConfirmNo);
        assert_eq!(
            key_action(&app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Action::ConfirmNo
        );
        // Everything else is inert while the question is on screen.
        assert_eq!(key_action(&app, key('j')), Action::None);
        assert_eq!(key_action(&app, key('x')), Action::None);
    }

    #[test]
    fn key_releases_are_ignored() {
        let app = fixtures::app(fixtures::POPULATED);
        let mut k = key('j');
        k.kind = KeyEventKind::Release;
        assert_eq!(key_action(&app, k), Action::None);
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn laid_out() -> App {
        let mut app = fixtures::app(fixtures::POPULATED);
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        app.last_height = 24;
        render::render(&mut buf, area, &mut app);
        app
    }

    #[test]
    fn clicking_a_row_selects_it_and_double_clicking_is_enter() {
        let mut app = laid_out();
        let (y, id) = app
            .rows
            .iter()
            .rev()
            .find_map(|(y, r)| match r {
                Row::Task(id) => Some((*y, id.clone())),
                _ => None,
            })
            .unwrap();
        let mut last = None;
        assert_eq!(mouse_action(&mut app, click(20, y), &mut last), Action::None);
        assert_eq!(app.selected_id.as_deref(), Some(id.as_str()));
        // A second click on the same row, quickly, is enter.
        assert_eq!(mouse_action(&mut app, click(20, y), &mut last), Action::Enter);
    }

    #[test]
    fn clicking_a_section_header_does_nothing() {
        let mut app = laid_out();
        let y = app
            .rows
            .iter()
            .find_map(|(y, r)| matches!(r, Row::Section(_)).then_some(*y))
            .unwrap();
        let mut last = None;
        assert_eq!(mouse_action(&mut app, click(5, y), &mut last), Action::None);
    }

    #[test]
    fn clicking_a_footer_hint_fires_that_hint() {
        // Keyboard and mouse must produce identical behavior for every action.
        let mut app = laid_out();
        let (x1, _, ch) = app.footer_hits[0];
        let mut last = None;
        let action = mouse_action(&mut app, click(x1, 23), &mut last);
        assert_ne!(action, Action::None, "footer hint {ch:?} was not clickable");
    }

    #[test]
    fn clicking_the_collapsed_done_header_expands_it() {
        let mut app = laid_out();
        let y = app
            .rows
            .iter()
            .find_map(|(y, r)| (*r == Row::DoneCollapsed).then_some(*y))
            .expect("the fixture has a collapsed done section");
        let mut last = None;
        assert_eq!(mouse_action(&mut app, click(5, y), &mut last), Action::Enter);
    }
}
