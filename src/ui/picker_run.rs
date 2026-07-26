//! The picker process.
//!
//! Launched by herdr as a `placement = "popup"` pane. It receives all terminal
//! input including Escape, and closes when this command exits.

use super::picker::{Field, PickerState};
use crate::config::{HERDR_AGENT_KINDS, Paths};
use crate::dispatch::{self, Overrides};
use crate::herdr::Herdr;
use crate::log::Logger;
use crate::ui::app::{PICKER_RESULT, PICKER_TASK};
use anyhow::{Result, anyhow};
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::sync::Arc;
use std::time::Duration;

pub fn run(paths: Paths, log: Arc<Logger>) -> Result<()> {
    let engine = crate::cli::engine_from_paths(&paths, log.clone())?;
    let herdr = Herdr::discover(log.clone());

    // The board hands the selection over through the database — a popup has no
    // pane id and no shared memory with the board process.
    let task_id = engine
        .db
        .meta_get(PICKER_TASK)?
        .ok_or_else(|| anyhow!("no task selected"))?;
    let task = engine
        .db
        .get_task(&task_id)?
        .ok_or_else(|| anyhow!("no task {task_id}"))?;

    let plan = dispatch::plan(&engine.db, &engine.cfg, &engine.paths, &task, &Overrides::default())?;

    // Workspace choices: every workspace herdr knows about, with the routed one
    // first so `enter` without cycling does what the route says.
    let mut workspaces: Vec<String> = herdr
        .workspace_list()
        .unwrap_or_default()
        .into_iter()
        .map(|w| w.label)
        .filter(|l| !l.is_empty())
        .collect();
    if !workspaces.contains(&plan.workspace) {
        workspaces.insert(0, plan.workspace.clone());
    }
    workspaces.sort_by_key(|w| (*w != plan.workspace, w.clone()));
    if workspaces.is_empty() {
        workspaces.push(plan.workspace.clone());
    }

    let mut runtimes: Vec<String> = vec![plan.runtime.clone()];
    runtimes.extend(
        HERDR_AGENT_KINDS
            .iter()
            .map(|k| k.to_string())
            .filter(|k| *k != plan.runtime),
    );

    let mut st = PickerState {
        identifier: task.identifier.clone(),
        title: task.title.clone(),
        workspaces,
        runtimes,
        workspace_ix: 0,
        runtime_ix: 0,
        branch: plan.branch.clone(),
        focus: 0,
        plan,
        message: None,
    };

    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen, crossterm::event::EnableMouseCapture)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    let result = event_loop(&mut term, &mut st, &engine, &log, &task);

    disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    result
}

fn event_loop(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    st: &mut PickerState,
    engine: &crate::sync::SyncEngine,
    log: &Logger,
    task: &crate::model::Task,
) -> Result<()> {
    loop {
        term.draw(|f| {
            let area = f.area();
            super::picker::render(f.buffer_mut(), area, st);
        })?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        match event::read()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                // Never claim a herdr chord, even here.
                if k.modifiers.contains(KeyModifiers::CONTROL) {
                    continue;
                }
                match k.code {
                    KeyCode::Esc => return Ok(()),
                    KeyCode::Tab | KeyCode::Down => st.next_field(),
                    KeyCode::BackTab | KeyCode::Up => st.prev_field(),
                    KeyCode::Left if st.cycle(-1) => replan(st, engine, task),
                    KeyCode::Right if st.cycle(1) => replan(st, engine, task),
                    KeyCode::Backspace => st.backspace(),
                    KeyCode::Char(c) if st.focused() == Field::Branch => st.type_char(c),
                    KeyCode::Char('j') => st.next_field(),
                    KeyCode::Char('k') => st.prev_field(),
                    KeyCode::Enter => {
                        // At cap `enter` is not offered, and pressing it anyway
                        // repeats the refusal rather than doing nothing.
                        if st.plan.at_cap() {
                            st.message = Some(super::picker::capacity_line(&st.plan));
                            continue;
                        }
                        // Hand the slow part off and close.
                        //
                        // Creating a worktree, opening a pane and waiting for an
                        // agent to become interactive takes seconds. Doing that
                        // inline froze the picker while the operator watched a
                        // terminal start, which is time they were going to spend
                        // anyway *not* looking at it. The detached run reports
                        // back through the database, and the board picks the
                        // result up on its next tick.
                        match spawn_dispatch(task, st, log) {
                            Ok(()) => {
                                let _ = engine.db.meta_set(
                                    PICKER_RESULT,
                                    &format!(
                                        "dispatching {} → ws:{} · {}",
                                        task.identifier,
                                        st.workspace(),
                                        st.runtime()
                                    ),
                                );
                                // Land on this row when the board comes back.
                                let _ = engine
                                    .db
                                    .meta_set(crate::ui::app::SELECT_TASK, &task.id);
                                return Ok(());
                            }
                            Err(e) => st.message = Some(e.to_string()),
                        }
                    }
                    _ => {}
                }
            }
            Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                if let Some(ix) = PickerState::field_at_row(m.row) {
                    st.focus = ix;
                }
            }
            _ => {}
        }
    }
}

/// Run the dispatch in a detached child, so the picker can close at once.
///
/// The cap and duplicate-attempt checks run in the child; the picker has already
/// shown the cap, and a refusal comes back as a board message rather than being
/// worth blocking for.
fn spawn_dispatch(
    task: &crate::model::Task,
    st: &PickerState,
    log: &Logger,
) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("dispatch")
        .arg("--task")
        .arg(&task.id)
        .arg("--workspace")
        .arg(st.workspace())
        .arg("--runtime")
        .arg(st.runtime())
        .arg("--branch")
        .arg(&st.branch)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // A popup is a session-modal resource; when this process exits herdr tears
    // it down, so the child must outlive it.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    log.info(format!(
        "dispatch of {} handed to pid {}",
        task.identifier,
        child.id()
    ));
    Ok(())
}

/// The worktree path and the concurrency count both depend on the chosen
/// workspace, so re-plan whenever it changes.
fn replan(st: &mut PickerState, engine: &crate::sync::SyncEngine, task: &crate::model::Task) {
    if let Ok(p) = dispatch::plan(
        &engine.db,
        &engine.cfg,
        &engine.paths,
        task,
        &Overrides {
            workspace: Some(st.workspace().to_string()),
            runtime: Some(st.runtime().to_string()),
            branch: Some(st.branch.clone()),
            via: None,
        },
    ) {
        st.plan = p;
    }
}
