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
    /// Throughput: whether the delegating is actually working.
    Stats,
    /// What adopting a repo is about to put on the board, before it does.
    Adopt,
}

/// The adoption screen: how many issues a repo would contribute, and the labels
/// it could be narrowed to.
///
/// A board is a queue of work in play, and a repo's whole backlog is not that.
/// Adoption already knows the repo, so it asks rather than finding out
/// afterwards — 83 rows arriving in one poll is a surprise worth one keypress.
#[derive(Debug, Clone)]
pub struct AdoptView {
    pub repo: crate::adopt::Unadopted,
    /// What GitHub answered, or why it could not be asked. `None` while the
    /// question is still out — the call blocks, and a board that freezes with
    /// nothing on screen explaining why is the surprise this screen exists to
    /// remove. A repo that cannot be previewed is still adoptable: the ask is a
    /// courtesy, not a gate.
    pub preview: Option<Result<crate::adopt::RepoPreview, String>>,
    /// Label rows the operator has picked, by index into `preview.labels`.
    /// Empty means every open issue.
    pub chosen: Vec<usize>,
    pub cursor: usize,
    /// The `[github] labels` this repo would fall back to with no table of its
    /// own — so the screen can tell an override from agreeing with the default.
    pub fallback: Vec<String>,
}

impl AdoptView {
    pub fn new(
        repo: crate::adopt::Unadopted,
        preview: Option<Result<crate::adopt::RepoPreview, String>>,
        fallback: Vec<String>,
    ) -> AdoptView {
        AdoptView {
            repo,
            preview,
            chosen: Vec::new(),
            cursor: 0,
            fallback,
        }
    }

    /// Still waiting on GitHub.
    pub fn pending(&self) -> bool {
        self.preview.is_none()
    }

    /// The labels available to pick from.
    pub fn label_rows(&self) -> &[(String, usize)] {
        match &self.preview {
            Some(Ok(p)) => &p.labels,
            _ => &[],
        }
    }

    /// The labels currently picked, in the order they are shown.
    pub fn labels(&self) -> Vec<String> {
        let rows = self.label_rows();
        let mut ix = self.chosen.clone();
        ix.sort_unstable();
        ix.iter().filter_map(|i| rows.get(*i)).map(|(l, _)| l.clone()).collect()
    }

    pub fn is_chosen(&self, ix: usize) -> bool {
        self.chosen.contains(&ix)
    }

    pub fn toggle(&mut self) {
        let ix = self.cursor;
        if ix >= self.label_rows().len() {
            return;
        }
        match self.chosen.iter().position(|c| *c == ix) {
            Some(at) => {
                self.chosen.remove(at);
            }
            None => self.chosen.push(ix),
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.label_rows().len();
        if len == 0 {
            return;
        }
        let next = (self.cursor as isize + delta).clamp(0, len as isize - 1);
        self.cursor = next as usize;
    }

    /// The labels this repo would actually be polled for: what is picked, or
    /// the global list it falls back to when nothing is.
    pub fn effective_labels(&self) -> Vec<String> {
        let chosen = self.labels();
        if chosen.is_empty() {
            self.fallback.clone()
        } else {
            chosen
        }
    }

    /// How many issues the current choice would put on the board, if that can
    /// be known at all.
    pub fn would_pull(&self) -> Option<usize> {
        let p = self.preview.as_ref()?.as_ref().ok()?;
        Some(p.count_for(&self.effective_labels()))
    }

    /// What to write as `[[github.repo]] labels`, or `None` to leave the repo
    /// on the global list.
    ///
    /// Picking nothing is not "poll everything" — it is not answering, which
    /// leaves the repo where every repo was before this screen existed. And a
    /// table restating `[github] labels` is config that decides nothing, which
    /// the next reader has to compare two lists to discover.
    pub fn written_labels(&self) -> Option<Vec<String>> {
        let chosen = self.labels();
        (!chosen.is_empty() && chosen != self.fallback).then_some(chosen)
    }
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
    /// Short repo name for a GitHub task. `gh#507` says nothing about which of
    /// several configured repos it came from.
    pub repo: Option<String>,
    /// How to name the agent that released this row. `None` means the operator
    /// did — a keypress on the board, or a CLI run from a pane with no agent.
    ///
    /// The parent **task**'s identifier when the board dispatched that parent
    /// too, because a task is what you can navigate to. Most dispatching agents
    /// are orchestrators the board never dispatched, though, so this falls back
    /// to the **pane** — transient, but it is what there is, and it is where
    /// the agent actually is.
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
            // `gh:Florin-AS/Tally#507` → `Tally`. The owner is noise when you
            // only work with a handful of repos; the name is the part you read.
            let repo = task
                .id
                .strip_prefix("gh:")
                .and_then(|r| r.split(['#', '!']).next())
                .and_then(|r| r.rsplit('/').next())
                .map(str::to_string);

            TaskView {
                repo,
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
                // The parent's identifier when the board dispatched it too,
                // and the pane it is running in otherwise — which is what an
                // orchestrator, the usual dispatcher, has instead of a task.
                dispatched_by: live.or(last).and_then(|a| {
                    a.dispatched_by
                        .as_ref()
                        .map(|id| identifiers.get(id).cloned().unwrap_or_else(|| id.clone()))
                        .or_else(|| a.dispatched_by_pane.clone())
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

/// Selection id for a section header.
///
/// Headers are rows the cursor can land on, because `enter` on one folds it and
/// there is otherwise no way to reach them without a mouse. The NUL prefix
/// keeps them from ever colliding with a real task id.
pub fn section_row_id(state: BoardState) -> String {
    format!("\u{0}section:{}", state.as_str())
}

/// The `done` header, kept as a name because it is the one collapsed by
/// default.
pub const DONE_ROW: &str = "\u{0}section:done";

/// Selection id for the UNADOPTED header. Not a [`BoardState`]: a repo the
/// board is not watching is not a state a task can be in.
pub const UNADOPTED_ROW: &str = "\u{0}section:unadopted";

/// A row on screen, for rendering and for mouse hit-testing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A section header — selectable, and folds its section on `enter`.
    Section(BoardState),
    Task(String),
    /// The UNADOPTED header, which folds like any other.
    UnadoptedSection,
    /// One repo with a herdr workspace and no board config, by slug.
    Unadopted(String),
}

impl Row {
    /// The selection id this row answers to.
    pub fn id(&self) -> String {
        match self {
            Row::Section(s) => section_row_id(*s),
            Row::Task(id) => id.clone(),
            Row::UnadoptedSection => UNADOPTED_ROW.to_string(),
            Row::Unadopted(slug) => crate::adopt::row_id(slug),
        }
    }
}

pub struct App {
    pub views: Vec<TaskView>,
    /// Repos with a herdr workspace and no board config. Not tasks: they are
    /// the reason there are no tasks for those repos.
    pub unadopted: Vec<crate::adopt::Unadopted>,
    pub selected_id: Option<String>,
    pub screen: Screen,
    /// Sections the operator has folded away. `done` starts folded: it is
    /// history, and the rest is the queue.
    pub collapsed: std::collections::HashSet<BoardState>,
    /// UNADOPTED starts **open**. It is a notice, and the whole point of it is
    /// that a silent repo stops being silent.
    pub collapsed_unadopted: bool,
    pub sync: SyncStatus,
    /// Footer flash for `o` / `g` / `s` and post-dispatch acknowledgements.
    pub message: Option<(String, Instant)>,
    /// Inline cancel confirmation — replaces the footer, never a second modal.
    pub confirm: Option<String>,
    /// The repo [`Screen::Adopt`] is asking about. Set by `a`, cleared on the
    /// way out either way.
    pub adopt: Option<AdoptView>,
    pub should_quit: bool,
    /// Rows as laid out by the last render, for click hit-testing.
    pub rows: Vec<(u16, Row)>,
    /// Footer hint click targets from the last render: (x range, key).
    pub footer_hits: Vec<(u16, u16, char)>,
    /// Label rows on the adoption screen from the last render: (y, index).
    /// herdr is mouse-first, and a picker you cannot click is half a picker.
    pub adopt_hits: Vec<(u16, usize)>,
    pub config_path: String,
    /// What is stopping the board from having anything on it. Empty once the
    /// board is set up, at which point an empty board just means an empty queue.
    pub setup_hints: Vec<String>,
    /// Height of the last render, so a click can tell the footer row apart.
    pub last_height: u16,
    /// Recomputed when the stats screen is opened, not every tick — it reads
    /// every attempt and nothing on it changes second to second.
    pub stats: Option<crate::stats::Stats>,
    /// First body line on screen. Rows below the fold are otherwise
    /// unreachable, which on a short pane is most of the board.
    pub scroll: usize,
}

pub const MESSAGE_TTL: std::time::Duration = std::time::Duration::from_millis(2600);

impl App {
    pub fn new(views: Vec<TaskView>, sync: SyncStatus, config_path: String) -> App {
        let mut app = App {
            views,
            unadopted: Vec::new(),
            selected_id: None,
            screen: Screen::List,
            collapsed: std::collections::HashSet::from([BoardState::Done]),
            collapsed_unadopted: false,
            sync,
            message: None,
            confirm: None,
            adopt: None,
            should_quit: false,
            rows: Vec::new(),
            footer_hits: Vec::new(),
            adopt_hits: Vec::new(),
            config_path,
            setup_hints: Vec::new(),
            last_height: 0,
            stats: None,
            scroll: 0,
        };
        app.selected_id = app
            .first_actionable()
            .or_else(|| app.visible_task_ids().first().cloned());
        app
    }

    /// Sections in fixed order, empty ones omitted entirely.
    ///
    /// `done` is bounded to today. The header says "DONE today" and it means
    /// it: every issue ever closed in a tracked repo derives to `done`, and
    /// ninety-odd of them is a wall of history, not a board.
    pub fn sections(&self) -> Vec<(BoardState, Vec<&TaskView>)> {
        BoardState::SECTION_ORDER
            .iter()
            .filter_map(|&state| {
                let rows: Vec<&TaskView> = self
                    .views
                    .iter()
                    .filter(|v| v.state() == state)
                    .filter(|v| state != BoardState::Done || finished_today(v))
                    .collect();
                if rows.is_empty() {
                    None
                } else {
                    Some((state, rows))
                }
            })
            .collect()
    }

    /// Every row the body draws, in display order.
    ///
    /// UNADOPTED comes last, below `done`. It is setup, not queue: a blocked
    /// agent is burning wall-clock right now and a repo that is not being polled
    /// has been quietly not being polled for days, so pushing the queue down for
    /// it would be the wrong trade every time.
    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for (state, rows) in self.sections() {
            // The header is a row either way: folded it is the only way to open
            // the section, unfolded the only way to close it again.
            out.push(Row::Section(state));
            if self.is_collapsed(state) {
                continue;
            }
            out.extend(rows.iter().map(|v| Row::Task(v.id().to_string())));
        }
        if !self.unadopted.is_empty() {
            out.push(Row::UnadoptedSection);
            if !self.collapsed_unadopted {
                out.extend(self.unadopted.iter().map(|u| Row::Unadopted(u.slug.clone())));
            }
        }
        out
    }

    /// The rows a cursor can land on, in display order.
    pub fn visible_task_ids(&self) -> Vec<String> {
        self.rows().iter().map(Row::id).collect()
    }

    /// The unadopted repo the cursor is on, if any.
    pub fn unadopted_selected(&self) -> Option<&crate::adopt::Unadopted> {
        let id = self.selected_id.as_deref()?;
        self.unadopted.iter().find(|u| u.row_id() == id)
    }

    pub fn on_unadopted_header(&self) -> bool {
        self.selected_id.as_deref() == Some(UNADOPTED_ROW)
    }

    /// The first row that is an actual task, for landing the cursor somewhere
    /// useful: a header is selectable, but it is not what you came to act on.
    pub fn first_task(&self) -> Option<String> {
        self.visible_task_ids()
            .into_iter()
            .find(|id| self.views.iter().any(|v| v.id() == id))
    }

    /// The first row worth landing on. A board with no tasks and an unadopted
    /// repo on it is the exact case this feature exists for, and leaving the
    /// cursor on a header there would put `a` one keypress further away.
    pub fn first_actionable(&self) -> Option<String> {
        self.first_task().or_else(|| {
            self.rows().into_iter().find_map(|r| match r {
                Row::Unadopted(slug) => Some(crate::adopt::row_id(&slug)),
                _ => None,
            })
        })
    }

    /// The section header the cursor is on, if any.
    pub fn on_section_header(&self) -> Option<BoardState> {
        let id = self.selected_id.as_deref()?;
        BoardState::SECTION_ORDER
            .into_iter()
            .find(|s| section_row_id(*s) == id)
    }

    pub fn is_collapsed(&self, state: BoardState) -> bool {
        self.collapsed.contains(&state)
    }

    pub fn toggle_collapsed(&mut self, state: BoardState) {
        if !self.collapsed.remove(&state) {
            self.collapsed.insert(state);
        }
    }

    /// How many rows a section holds, for the count on a folded header.
    pub fn section_len(&self, state: BoardState) -> usize {
        self.sections()
            .into_iter()
            .find(|(s, _)| *s == state)
            .map(|(_, rows)| rows.len())
            .unwrap_or(0)
    }

    /// The selected task, or `None` — including when the cursor is on the
    /// collapsed `done` header, which is a row but not a task.
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
    pub fn refresh(
        &mut self,
        views: Vec<TaskView>,
        sync: SyncStatus,
        unadopted: Vec<crate::adopt::Unadopted>,
    ) {
        self.views = views;
        self.unadopted = unadopted;
        self.sync = sync;
        let ids = self.visible_task_ids();
        let keep = self
            .selected_id
            .as_deref()
            .is_some_and(|id| ids.iter().any(|i| i == id));
        if !keep {
            // The selected task left the board; fall back to the first task
            // rather than to nothing — or to a header, which is not actionable.
            self.selected_id = self.first_actionable().or_else(|| ids.first().cloned());
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
    if crate::config::linear_api_key(paths).is_none() {
        out.push(format!("No LINEAR_API_KEY, so Linear is never polled — add it to {env}"));
    }
    if !cfg.github.repos.is_empty() && crate::config::github_token(paths).is_none() {
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

/// Was this task closed today, in the operator's own timezone?
///
/// Local midnight, not a rolling 24 hours: "today" is a thing a person means,
/// and a row dropping off at 14:37 because that is when it closed yesterday
/// would be baffling.
pub fn finished_today(v: &TaskView) -> bool {
    let Ok(updated) = chrono::DateTime::parse_from_rfc3339(&v.task.updated_at) else {
        // An unparseable timestamp is not evidence of recency.
        return false;
    };
    let local = updated.with_timezone(&chrono::Local);
    local.date_naive() == chrono::Local::now().date_naive()
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
                pr_merged: false,
                pr_mergeable: None,
                // Today, so a `done` fixture row is inside the section's bound.
                updated_at: crate::db::rfc3339(chrono::Utc::now()),
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
            repo: None,
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
    fn done_is_bounded_to_today() {
        // Every issue ever closed in a tracked repo derives to `done`; the
        // section says "today" and has to mean it.
        let mut old = view("old", BoardState::Done);
        old.task.updated_at = "2020-01-01T00:00:00Z".into();
        let mut today = view("today", BoardState::Done);
        today.task.updated_at = crate::db::rfc3339(chrono::Utc::now());

        let a = App::new(vec![old, today], sync(), "/cfg".into());
        let done: Vec<&str> = a
            .sections()
            .into_iter()
            .find(|(s, _)| *s == BoardState::Done)
            .map(|(_, rows)| rows.into_iter().map(|v| v.id()).collect())
            .unwrap_or_default();
        assert_eq!(done, vec!["today"]);
    }

    #[test]
    fn a_done_section_with_nothing_from_today_is_omitted() {
        let mut old = view("old", BoardState::Done);
        old.task.updated_at = "2020-01-01T00:00:00Z".into();
        let a = App::new(vec![old, view("r", BoardState::Ready)], sync(), "/cfg".into());
        assert!(
            !a.sections().iter().any(|(s, _)| *s == BoardState::Done),
            "an all-history done section must not render at all"
        );
    }

    #[test]
    fn the_done_header_is_reachable_however_it_is_folded() {
        // `enter` on the header is the only way in and the only way out, so it
        // has to stay selectable in both states.
        let mut a = app();
        let hdr = |s: BoardState| section_row_id(s);
        assert_eq!(
            a.visible_task_ids(),
            vec![
                hdr(BoardState::Blocked), "d".into(),
                hdr(BoardState::Working), "b".into(),
                hdr(BoardState::Ready), "a".into(),
                hdr(BoardState::Done),
            ]
        );
        a.collapsed.remove(&BoardState::Done);
        assert!(a.visible_task_ids().contains(&"c".to_string()));
    }

    #[test]
    fn a_section_header_is_a_row_but_not_a_task() {
        let mut a = app();
        a.selected_id = Some(DONE_ROW.into());
        assert_eq!(a.on_section_header(), Some(BoardState::Done));
        assert!(a.selected().is_none());
    }

    #[test]
    fn every_section_folds_not_just_done() {
        let mut a = app();
        // `done` starts folded; the rest start open.
        assert!(a.is_collapsed(BoardState::Done));
        assert!(!a.is_collapsed(BoardState::Ready));

        a.toggle_collapsed(BoardState::Ready);
        assert!(a.is_collapsed(BoardState::Ready));
        assert!(!a.visible_task_ids().contains(&"a".to_string()));
        // ...but its header stays, or it could never be reopened.
        assert!(a.visible_task_ids().contains(&section_row_id(BoardState::Ready)));

        a.toggle_collapsed(BoardState::Ready);
        assert!(a.visible_task_ids().contains(&"a".to_string()));
    }

    #[test]
    fn selection_starts_on_the_first_task_not_a_header() {
        // Headers are selectable, but they are not what you came to act on.
        assert_eq!(app().selected_id.as_deref(), Some("d"));
    }

    #[test]
    fn movement_clamps_at_both_ends() {
        let mut a = app();
        a.select_delta(-99);
        assert_eq!(
            a.selected_id.as_deref(),
            Some(section_row_id(BoardState::Blocked).as_str())
        );
        // The last row is the folded `done` header, not a task.
        a.select_delta(99);
        assert_eq!(a.selected_id.as_deref(), Some(DONE_ROW));
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
            Vec::new(),
        );
        assert_eq!(a.selected_id.as_deref(), Some("a"));
    }

    #[test]
    fn selection_falls_back_when_its_task_leaves_the_board() {
        let mut a = app();
        a.selected_id = Some("a".into());
        a.refresh(vec![view("b", BoardState::Working)], sync(), Vec::new());
        assert_eq!(a.selected_id.as_deref(), Some("b"));
    }

    fn unadopted(slug: &str) -> crate::adopt::Unadopted {
        crate::adopt::Unadopted {
            label: slug.rsplit('/').next().unwrap().into(),
            slug: slug.into(),
            repo_root: format!("/code/{slug}"),
            missing: crate::adopt::Missing::Both,
        }
    }

    #[test]
    fn unadopted_repos_sit_below_the_queue_not_above_it() {
        // A blocked agent is burning wall-clock right now; a repo that is not
        // being polled has been quietly not being polled for days. Pushing the
        // queue down for it would be the wrong trade every time.
        let mut a = app();
        a.unadopted = vec![unadopted("o/thing")];
        let rows = a.rows();
        assert_eq!(rows[0], Row::Section(BoardState::Blocked));
        assert_eq!(
            &rows[rows.len() - 2..],
            &[Row::UnadoptedSection, Row::Unadopted("o/thing".into())]
        );
    }

    #[test]
    fn the_section_is_absent_entirely_when_there_is_nothing_to_adopt() {
        let a = app();
        assert!(!a.rows().contains(&Row::UnadoptedSection));
        assert!(!a.visible_task_ids().iter().any(|i| i == UNADOPTED_ROW));
    }

    #[test]
    fn an_unadopted_row_is_selectable_but_is_not_a_task() {
        let mut a = app();
        a.unadopted = vec![unadopted("o/thing")];
        a.selected_id = Some(crate::adopt::row_id("o/thing"));
        assert_eq!(a.unadopted_selected().map(|u| u.slug.as_str()), Some("o/thing"));
        // ...and every key that acts on a task finds nothing here.
        assert!(a.selected().is_none());
        assert!(a.on_section_header().is_none());
        assert!(!a.on_unadopted_header());
    }

    #[test]
    fn row_ids_can_never_collide_with_a_task_id() {
        // Both headers and repo rows live in the same selection space as tasks.
        let ids = [
            section_row_id(BoardState::Ready),
            UNADOPTED_ROW.to_string(),
            crate::adopt::row_id("o/thing"),
        ];
        for id in &ids {
            assert!(id.starts_with('\u{0}'), "{id:?} could collide with a task");
        }
    }

    #[test]
    fn the_section_folds_and_its_header_survives_folding() {
        let mut a = app();
        a.unadopted = vec![unadopted("o/a"), unadopted("o/b")];
        // Open by default: the whole point is that a silent repo stops being
        // silent, and a folded notice is one nobody reads.
        assert!(!a.collapsed_unadopted);
        assert_eq!(a.rows().iter().filter(|r| matches!(r, Row::Unadopted(_))).count(), 2);

        a.collapsed_unadopted = true;
        assert_eq!(a.rows().iter().filter(|r| matches!(r, Row::Unadopted(_))).count(), 0);
        assert!(a.rows().contains(&Row::UnadoptedSection), "no way back in");
    }

    #[test]
    fn a_board_with_nothing_but_unadopted_repos_lands_the_cursor_on_one() {
        // This is the state AGE-18 exists for. Landing on the header would put
        // `a` one keypress further away for no reason.
        let mut a = App::new(Vec::new(), sync(), "/cfg".into());
        a.refresh(Vec::new(), sync(), vec![unadopted("o/thing")]);
        assert_eq!(a.selected_id, Some(crate::adopt::row_id("o/thing")));
    }

    #[test]
    fn adopting_the_last_repo_moves_the_cursor_off_a_row_that_no_longer_exists() {
        let mut a = app();
        a.unadopted = vec![unadopted("o/thing")];
        a.selected_id = Some(crate::adopt::row_id("o/thing"));
        a.refresh(a.views.clone(), sync(), Vec::new());
        assert_eq!(a.selected_id.as_deref(), Some("d"), "back onto a task");
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
