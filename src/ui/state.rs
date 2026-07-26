//! View model for the board: what is on screen, and what the keys act on.

use crate::config::RoutingConfig;
use crate::model::*;
use crate::sync::{SourceHealth, route_context};
use std::time::Instant;

/// Which full-pane view is showing. Help and detail are **full-pane views, not
/// floating panels** — a bordered panel inside an already-bordered herdr pane is
/// exactly the nested-box clutter the design forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    List,
    Detail,
    Prompt,
    Help,
}

/// One task, plus everything the renderer needs that is not on the row.
#[derive(Debug, Clone)]
pub struct TaskView {
    pub task: Task,
    /// A route matched. Without one the row is shown but not dispatchable.
    pub has_route: bool,
    pub route_name: Option<String>,
    pub runtime: Option<String>,
    pub workspace: Option<String>,
    /// Seconds since the live attempt started.
    pub elapsed_secs: Option<i64>,
    /// The agent has settled but produced no PR — it may be between turns. This
    /// is a *marker on a working row*, never a state of its own.
    pub idle: bool,
    pub pane_id: Option<String>,
    /// Branch this task's next (or current) attempt uses.
    pub branch: Option<String>,
    /// The **resolved** prompt — interpolated with this task's title and body,
    /// which is what the prompt view must show, not the template.
    pub resolved_prompt: Option<String>,
    /// Identifier of the parent task that released this one, when an agent did.
    /// `None` means the operator did.
    ///
    /// Provenance names the parent **task**, not the pane: the pane is
    /// transient, the task is what you can navigate to.
    pub dispatched_by: Option<String>,
}

impl TaskView {
    pub fn id(&self) -> &str {
        &self.task.id
    }
    pub fn state(&self) -> BoardState {
        self.task.state
    }
}

/// Build view rows from stored tasks, resolving routes for display.
pub fn build_views(
    tasks: Vec<Task>,
    cfg: &RoutingConfig,
    paths: &crate::config::Paths,
) -> Vec<TaskView> {
    let now = chrono::Utc::now();
    // Provenance is stored as a task id; the board shows the identifier.
    let identifiers: std::collections::HashMap<String, String> = tasks
        .iter()
        .map(|t| (t.id.clone(), t.identifier.clone()))
        .collect();
    tasks
        .clone()
        .into_iter()
        .map(|task| {
            let route = cfg.resolve(&route_context(&task));
            let live = task.live_attempt();
            let last = task.attempts.last();
            let elapsed_secs = live.and_then(|a| {
                chrono::DateTime::parse_from_rfc3339(&a.started_at)
                    .ok()
                    // Never negative: a clock skew must not render as a
                    // count-up from the future.
                    .map(|s| (now - s.with_timezone(&chrono::Utc)).num_seconds().max(0))
            });
            let idle = live.is_some_and(|a| {
                matches!(
                    a.agent_status,
                    Some(AgentStatus::Idle) | Some(AgentStatus::Done)
                )
            }) && task.state == BoardState::Working;
            // The branch and prompt shown are the ones this task would use:
            // for a dispatched task, the attempt's own; otherwise the plan's.
            let branch = live
                .or(last)
                .and_then(|a| a.branch.clone())
                .or_else(|| route.map(|r| crate::dispatch::resolve_branch(cfg, r, &task)));
            let resolved_prompt = route.map(|r| {
                let wt = live
                    .or(last)
                    .and_then(|a| a.worktree.clone())
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| paths.worktree_root());
                crate::dispatch::resolve_prompt(
                    cfg,
                    r,
                    &task,
                    branch.as_deref().unwrap_or_default(),
                    &wt,
                )
            });
            TaskView {
                has_route: route.is_some(),
                route_name: route.map(|r| r.display_name().to_string()),
                runtime: live
                    .or(last)
                    .map(|a| a.runtime.clone())
                    .or_else(|| route.map(|r| r.runtime.clone())),
                workspace: live
                    .or(last)
                    .map(|a| a.workspace.clone())
                    .or_else(|| route.map(|r| r.workspace.clone())),
                elapsed_secs,
                idle,
                pane_id: live.and_then(|a| a.pane_id.clone()),
                dispatched_by: live.or(last).and_then(|a| {
                    a.dispatched_by.as_ref().map(|id| {
                        identifiers.get(id).cloned().unwrap_or_else(|| id.clone())
                    })
                }),
                branch,
                resolved_prompt,
                task,
            }
        })
        .collect()
}

/// Header sync status. Three distinct states, not two.
///
/// Two different clocks, which the design's own copy distinguishes and which it
/// is easy to conflate: `last_cycle_secs` is when syncd last *ran* (daemon
/// liveness), `last_source_ok_secs` is when a source last *answered*. During an
/// outage the daemon keeps cycling on time while the sources go stale, so the
/// header shows a healthy daemon and `last synced 4m`.
#[derive(Debug, Clone)]
pub struct SyncStatus {
    pub linear: SourceHealth,
    pub github: SourceHealth,
    /// Age of the last completed sync cycle.
    pub last_cycle_secs: Option<i64>,
    /// Age of the most recent successful source poll.
    pub last_source_ok_secs: Option<i64>,
    pub interval_secs: u64,
}

impl SyncStatus {
    /// A dead daemon is **not** a down source: the sources may be perfectly
    /// healthy and nobody is asking them. Naming syncd points at the thing to
    /// restart.
    pub fn syncd_dead(&self) -> bool {
        match self.last_cycle_secs {
            None => true,
            Some(age) => age > (self.interval_secs as i64) * 3,
        }
    }
}

/// A row on screen, for rendering and for mouse hit-testing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Section(BoardState),
    Task(String),
    /// The collapsed one-line `done` section.
    DoneCollapsed,
}

pub struct App {
    pub views: Vec<TaskView>,
    pub selected_id: Option<String>,
    pub screen: Screen,
    pub done_expanded: bool,
    pub sync: SyncStatus,
    /// Footer flash for `o` / `g` / `s` and post-dispatch acknowledgements.
    pub message: Option<(String, Instant)>,
    /// Inline cancel confirmation — replaces the footer, never a second modal.
    pub confirm: Option<String>,
    pub should_quit: bool,
    /// Rows as laid out by the last render, for click hit-testing.
    pub rows: Vec<(u16, Row)>,
    /// Footer hint click targets from the last render: (x range, key).
    pub footer_hits: Vec<(u16, u16, char)>,
    pub config_path: String,
    /// What is stopping the board from having anything on it. Empty once the
    /// board is set up, at which point an empty board just means an empty queue.
    pub setup_hints: Vec<String>,
    /// Height of the last render, so a click can tell the footer row apart.
    pub last_height: u16,
    /// First body line on screen. Rows below the fold are otherwise
    /// unreachable, which on a short pane is most of the board.
    pub scroll: usize,
}

pub const MESSAGE_TTL: std::time::Duration = std::time::Duration::from_millis(2600);

impl App {
    pub fn new(views: Vec<TaskView>, sync: SyncStatus, config_path: String) -> App {
        let mut app = App {
            views,
            selected_id: None,
            screen: Screen::List,
            done_expanded: false,
            sync,
            message: None,
            confirm: None,
            should_quit: false,
            rows: Vec::new(),
            footer_hits: Vec::new(),
            config_path,
            setup_hints: Vec::new(),
            last_height: 0,
            scroll: 0,
        };
        app.selected_id = app.visible_task_ids().first().cloned();
        app
    }

    /// Sections in fixed order, empty ones omitted entirely.
    pub fn sections(&self) -> Vec<(BoardState, Vec<&TaskView>)> {
        BoardState::SECTION_ORDER
            .iter()
            .filter_map(|&state| {
                let rows: Vec<&TaskView> =
                    self.views.iter().filter(|v| v.state() == state).collect();
                if rows.is_empty() {
                    None
                } else {
                    Some((state, rows))
                }
            })
            .collect()
    }

    /// The rows a cursor can land on, in display order.
    pub fn visible_task_ids(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (state, rows) in self.sections() {
            if state == BoardState::Done && !self.done_expanded {
                continue;
            }
            out.extend(rows.iter().map(|v| v.id().to_string()));
        }
        out
    }

    pub fn selected(&self) -> Option<&TaskView> {
        let id = self.selected_id.as_deref()?;
        self.views.iter().find(|v| v.id() == id)
    }

    pub fn select_delta(&mut self, delta: isize) {
        let ids = self.visible_task_ids();
        if ids.is_empty() {
            self.selected_id = None;
            return;
        }
        let cur = self
            .selected_id
            .as_deref()
            .and_then(|id| ids.iter().position(|i| i == id))
            .unwrap_or(0);
        let next = (cur as isize + delta).clamp(0, ids.len() as isize - 1) as usize;
        self.selected_id = Some(ids[next].clone());
    }

    /// Replace the task list from a refresh.
    ///
    /// Selection persists **by task id, not row index** — a poll landing while
    /// the operator is on a row must not move them.
    pub fn refresh(&mut self, views: Vec<TaskView>, sync: SyncStatus) {
        self.views = views;
        self.sync = sync;
        let ids = self.visible_task_ids();
        let keep = self
            .selected_id
            .as_deref()
            .is_some_and(|id| ids.iter().any(|i| i == id));
        if !keep {
            // The selected task left the board; fall back to the first row
            // rather than to nothing.
            self.selected_id = ids.first().cloned();
        }
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    pub fn expire_message(&mut self) {
        if let Some((_, at)) = &self.message
            && at.elapsed() > MESSAGE_TTL
        {
            self.message = None;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }
}

/// Why the board is empty, when the reason is "not set up yet" rather than
/// "nothing queued".
///
/// The design's empty state assumes a configured board with an empty queue.
/// A board that was never configured is a different state and needs to say so:
/// naming a file the operator has not created yet is not an instruction.
pub fn setup_hints(cfg: &RoutingConfig, paths: &crate::config::Paths) -> Vec<String> {
    let mut out = Vec::new();
    let env = crate::config::shorten_home(&paths.env_file());

    if !paths.routing().exists() {
        out.push("No routing.toml yet — run  herdr-board init".to_string());
    }
    if crate::config::linear_api_key().is_none() {
        out.push(format!("No LINEAR_API_KEY, so Linear is never polled — add it to {env}"));
    }
    if !cfg.github.repos.is_empty() && crate::config::github_token().is_none() {
        out.push(format!(
            "No GITHUB_TOKEN, and {} repo(s) are configured — private repos answer 404 without it",
            cfg.github.repos.len()
        ));
    }
    if cfg.routes.is_empty() && paths.routing().exists() {
        out.push("No routes, so nothing can be dispatched — see routing.toml".to_string());
    }
    if !out.is_empty() {
        out.push("herdr-board doctor  checks all of this".to_string());
    }
    out
}

/// `12s` / `9m04s` / `1h20m`. Minute resolution was rejected: a counter that
/// never visibly moves is not worth the redraw.
pub fn format_elapsed(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// Coarser form for the header (`synced 12s`, `last synced 4m`).
pub fn format_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(id: &str, state: BoardState) -> TaskView {
        TaskView {
            task: Task {
                id: id.into(),
                source: Source::Linear,
                source_id: "u".into(),
                identifier: id.into(),
                title: "t".into(),
                body: None,
                url: "u".into(),
                labels: vec![],
                state,
                source_state: None,
                linear_team: None,
                linear_project: None,
                upstream: UpstreamState::Unstarted,
                local_done: false,
                pr_url: None,
                pr_number: None,
                pr_open: false,
                updated_at: String::new(),
                synced_at: String::new(),
                attempts: vec![],
            },
            has_route: true,
            route_name: Some("offhand".into()),
            runtime: Some("claude-code".into()),
            workspace: Some("offhand".into()),
            elapsed_secs: None,
            idle: false,
            pane_id: None,
            branch: Some("board/lin-142".into()),
            resolved_prompt: Some("do the thing".into()),
            dispatched_by: None,
        }
    }

    fn sync() -> SyncStatus {
        SyncStatus {
            linear: SourceHealth::Ok,
            github: SourceHealth::Absent,
            last_cycle_secs: Some(12),
            last_source_ok_secs: Some(12),
            interval_secs: 30,
        }
    }

    fn app() -> App {
        App::new(
            vec![
                view("a", BoardState::Ready),
                view("b", BoardState::Working),
                view("c", BoardState::Done),
                view("d", BoardState::Blocked),
            ],
            sync(),
            "/cfg".into(),
        )
    }

    #[test]
    fn sections_are_in_fixed_order_and_omit_empties() {
        let a = app();
        let order: Vec<_> = a.sections().iter().map(|(s, _)| *s).collect();
        assert_eq!(
            order,
            vec![
                BoardState::Blocked,
                BoardState::Working,
                BoardState::Ready,
                BoardState::Done
            ]
        );
        // review and failed had no rows and are absent entirely.
        assert!(!order.contains(&BoardState::Review));
    }

    #[test]
    fn a_collapsed_done_section_is_not_navigable() {
        let mut a = app();
        assert_eq!(a.visible_task_ids(), vec!["d", "b", "a"]);
        a.done_expanded = true;
        assert_eq!(a.visible_task_ids(), vec!["d", "b", "a", "c"]);
    }

    #[test]
    fn selection_starts_on_the_first_visible_row() {
        assert_eq!(app().selected_id.as_deref(), Some("d"));
    }

    #[test]
    fn movement_clamps_at_both_ends() {
        let mut a = app();
        a.select_delta(-1);
        assert_eq!(a.selected_id.as_deref(), Some("d"));
        a.select_delta(99);
        assert_eq!(a.selected_id.as_deref(), Some("a"));
    }

    #[test]
    fn selection_survives_a_refresh_that_reorders_the_list() {
        // The design's explicit requirement: selection persists by id, not
        // index, so a poll landing on a row does not move the operator.
        let mut a = app();
        a.selected_id = Some("a".into());
        a.refresh(
            vec![
                view("z", BoardState::Blocked),
                view("b", BoardState::Working),
                view("a", BoardState::Ready),
            ],
            sync(),
        );
        assert_eq!(a.selected_id.as_deref(), Some("a"));
    }

    #[test]
    fn selection_falls_back_when_its_task_leaves_the_board() {
        let mut a = app();
        a.selected_id = Some("a".into());
        a.refresh(vec![view("b", BoardState::Working)], sync());
        assert_eq!(a.selected_id.as_deref(), Some("b"));
    }

    #[test]
    fn a_dead_daemon_is_detected_at_three_intervals() {
        let mut s = sync();
        s.last_cycle_secs = Some(89);
        assert!(!s.syncd_dead());
        s.last_cycle_secs = Some(91);
        assert!(s.syncd_dead());
        // Never synced at all also counts.
        s.last_cycle_secs = None;
        assert!(s.syncd_dead());
    }

    #[test]
    fn a_source_outage_does_not_read_as_a_dead_daemon() {
        // The daemon keeps cycling on time; only the sources have gone stale.
        let s = SyncStatus {
            linear: SourceHealth::Down {
                error: "refused".into(),
                retry_in: 30,
            },
            last_cycle_secs: Some(12),
            last_source_ok_secs: Some(240),
            ..sync()
        };
        assert!(!s.syncd_dead());
    }

    #[test]
    fn elapsed_formats_at_three_scales() {
        assert_eq!(format_elapsed(12), "12s");
        assert_eq!(format_elapsed(544), "9m04s");
        assert_eq!(format_elapsed(4800), "1h20m");
    }

    #[test]
    fn elapsed_is_never_negative() {
        // Clock skew must not render as a count-up from the future.
        assert_eq!(format_elapsed(-5), "0s");
    }

    #[test]
    fn messages_expire() {
        let mut a = app();
        a.flash("opened in browser");
        assert!(a.message.is_some());
        a.message = Some(("x".into(), Instant::now() - MESSAGE_TTL * 2));
        a.expire_message();
        assert!(a.message.is_none());
    }
}
