//! Dispatch: route → worktree → herdr pane → agent → prompt (impl spec §6).

use crate::config::{Route, RoutingConfig, herdr_kind_for_runtime, interpolate};
use crate::db::{Db, NewAttempt};
use crate::herdr::{Herdr, agent_name};
use crate::log::Logger;
use crate::model::{AgentStatus, Outcome, Task};
use crate::sync::{SyncEngine, route_context};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Per-dispatch choices the picker can override.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub workspace: Option<String>,
    pub runtime: Option<String>,
    pub branch: Option<String>,
    /// Parent task id, when an agent is releasing this task rather than the
    /// operator. Normally left `None` and resolved from the calling pane.
    pub via: Option<String>,
}

/// Who released a task.
///
/// Agent-initiated dispatch is a primary path: any agent can run
/// `herdr-board dispatch --task <id>` from its own pane, and the row lands on
/// the board within a tick. Rows therefore appear in `working` that the operator
/// never released, routinely — so every dispatch records its origin.
pub fn resolve_dispatcher(db: &Db, explicit: Option<&str>) -> Option<String> {
    // A command running inside a herdr pane carries that pane's id.
    let pane = std::env::var("HERDR_PANE_ID").ok().filter(|p| !p.is_empty());
    dispatcher_from(db, explicit, pane.as_deref())
}

/// The provenance decision, without reading the environment.
///
/// If a live attempt owns the calling pane, the agent in it is the parent. The
/// board pane and the picker popup own no attempt — and a popup gets no pane id
/// at all — so an operator dispatch resolves to `None` on its own, with no flag
/// to remember.
pub fn dispatcher_from(db: &Db, explicit: Option<&str>, pane: Option<&str>) -> Option<String> {
    if let Some(v) = explicit {
        return Some(v.to_string());
    }
    db.live_attempt_for_pane(pane?)
        .ok()
        .flatten()
        .map(|a| a.task_id)
}

/// Everything resolved for a dispatch, before anything is created. The picker
/// renders this as its confirmation.
#[derive(Debug, Clone)]
pub struct Plan {
    pub identifier: String,
    pub route_name: String,
    pub workspace: String,
    pub repo: PathBuf,
    pub runtime: String,
    pub herdr_kind: &'static str,
    pub branch: String,
    pub worktree: PathBuf,
    pub prompt: String,
    pub attempt_no: usize,
    /// Live attempts already in the target workspace, and the cap.
    pub live_in_workspace: usize,
    pub max_concurrent: usize,
    /// Parent task id when an agent released this task.
    pub dispatched_by: Option<String>,
}

impl Plan {
    /// At cap: the picker states the fact and removes `enter` from its footer.
    pub fn at_cap(&self) -> bool {
        self.live_in_workspace >= self.max_concurrent
    }
}

/// Build the interpolation variables for a task. Shared by the prompt view and
/// the dispatcher so what you read is what gets sent.
pub fn prompt_vars<'a>(
    task: &'a Task,
    branch: &'a str,
    workspace: &'a str,
    worktree: &'a Path,
) -> BTreeMap<&'static str, String> {
    let mut v = BTreeMap::new();
    v.insert("title", task.title.clone());
    v.insert("identifier", task.identifier.clone());
    // Slugified, not merely lowercased: this feeds `branch_template`, and a
    // GitHub identifier lowercases to `gh#506`, which makes a branch name that
    // git tolerates and every shell does not. Linear ids are unaffected
    // (`LIN-145` → `lin-145` either way).
    v.insert("identifier_lower", crate::config::slugify(&task.identifier));
    v.insert("body", task.body.clone().unwrap_or_default());
    v.insert("url", task.url.clone());
    v.insert("branch", branch.to_string());
    v.insert("workspace", workspace.to_string());
    v.insert("worktree", worktree.to_string_lossy().into_owned());
    v
}

/// The prompt actually sent for a task under a route, fully interpolated.
///
/// The design's prompt view must show the **resolved** prompt, not the
/// template, so this is the single place that resolution happens.
pub fn resolve_prompt(cfg: &RoutingConfig, route: &Route, task: &Task, branch: &str, worktree: &Path) -> String {
    let vars = prompt_vars(task, branch, &route.workspace, worktree);
    let template = route.prompt.clone().unwrap_or_else(|| {
        // A route with no prompt still needs to say something useful.
        "You are working on: {title} ({identifier})\n\n{body}\n\n\
         Work in this worktree; the branch {branch} is prepared. \
         Open a pull request when done."
            .to_string()
    });
    let _ = cfg;
    interpolate(&template, &vars)
}

/// Resolve the branch name for an attempt.
pub fn resolve_branch(cfg: &RoutingConfig, route: &Route, task: &Task) -> String {
    let vars = prompt_vars(task, "", &route.workspace, Path::new(""));
    interpolate(cfg.branch_template(route), &vars)
}

/// Build a dispatch plan without creating anything.
pub fn plan(
    db: &Db,
    cfg: &RoutingConfig,
    paths: &crate::config::Paths,
    task: &Task,
    ov: &Overrides,
) -> Result<Plan> {
    // A reaped task keeps its row for the history on it, but there is no issue
    // behind it any more: no state to move, nothing to comment on, and no way
    // to tell whether the work is still wanted.
    if task.upstream == crate::model::UpstreamState::Gone {
        bail!(
            "{} no longer exists in {} — its row is kept for the attempts on it, \
             not to dispatch from",
            task.identifier,
            task.source.as_str()
        );
    }
    let ctx = route_context(task);
    let route = cfg
        .resolve(&ctx)
        .ok_or_else(|| anyhow::anyhow!("no route for {}", task.identifier))?;

    let workspace = ov
        .workspace
        .clone()
        .unwrap_or_else(|| route.workspace.clone());
    let runtime = ov.runtime.clone().unwrap_or_else(|| route.runtime.clone());
    let herdr_kind = herdr_kind_for_runtime(&runtime).ok_or_else(|| {
        anyhow::anyhow!(
            "runtime `{runtime}` is not a herdr agent kind; \
             see `herdr-board doctor`"
        )
    })?;
    let branch = ov
        .branch
        .clone()
        .unwrap_or_else(|| resolve_branch(cfg, route, task));

    let attempt_no = task.attempt_count() + 1;
    let worktree = paths
        .worktree_root()
        .join(format!("{}-{}", crate::config::slugify(&task.identifier), attempt_no));

    Ok(Plan {
        identifier: task.identifier.clone(),
        route_name: route.display_name().to_string(),
        workspace: workspace.clone(),
        repo: route.repo_path(),
        runtime,
        herdr_kind,
        prompt: resolve_prompt(cfg, route, task, &branch, &worktree),
        branch,
        worktree,
        attempt_no,
        live_in_workspace: db.live_count_in_workspace(&workspace)?,
        max_concurrent: cfg.max_concurrent(route),
        dispatched_by: resolve_dispatcher(db, ov.via.as_deref()),
    })
}

/// Perform a dispatch.
///
/// Ordering is deliberate: the attempt row is inserted **first**, so the partial
/// unique index refuses a duplicate before anything is created. A failure after
/// that point closes the attempt rather than leaving it live forever.
pub fn dispatch(
    engine: &SyncEngine,
    herdr: &Herdr,
    log: &Logger,
    task: &Task,
    ov: &Overrides,
) -> Result<Plan> {
    let p = plan(&engine.db, &engine.cfg, &engine.paths, task, ov)?;

    if p.at_cap() {
        bail!(
            "ws:{} is at {} of {} working — cancel one first",
            p.workspace,
            p.live_in_workspace,
            p.max_concurrent
        );
    }

    // The duplicate-dispatch guard. A second concurrent dispatch fails here,
    // before a worktree or pane exists.
    let attempt_id = engine.db.insert_attempt(&NewAttempt {
        task_id: task.id.clone(),
        pane_id: None,
        workspace: p.workspace.clone(),
        runtime: p.runtime.clone(),
        worktree: Some(p.worktree.to_string_lossy().into_owned()),
        branch: Some(p.branch.clone()),
        dispatched_by: p.dispatched_by.clone(),
    })?;

    let result = (|| -> Result<String> {
        // herdr cuts the checkout and opens it as its own workspace, grouped
        // under the parent repo in the spaces sidebar. A dispatched agent
        // belongs in a space of its own — not a sliver of the tab you happen to
        // be working in, and not a tab you will not notice.
        //
        // A retry reuses the existing checkout, because git allows a branch in
        // only one worktree.
        let wt = match herdr.worktree_create(&p.repo, &p.branch, &p.identifier) {
            Ok(wt) => wt,
            Err(e) if e.to_string().contains("already") || e.to_string().contains("exists") => {
                log.info(format!("{} already has a checkout; reopening it", p.branch));
                herdr.worktree_open(&p.repo, &p.branch)?
            }
            Err(e) => return Err(e),
        };
        log.info(format!(
            "{} → workspace {} at {}",
            p.identifier,
            wt.workspace_id,
            wt.path.display()
        ));
        engine
            .db
            .conn
            .execute(
                "UPDATE attempts SET worktree = ?2 WHERE id = ?1",
                rusqlite::params![attempt_id, wt.path.to_string_lossy()],
            )
            .ok();

        let pane_id = wt.root_pane_id.clone();
        engine.db.set_attempt_pane(attempt_id, &pane_id)?;

        let name = agent_name(&p.identifier, p.attempt_no);
        start_agent_when_ready(herdr, log, &name, p.herdr_kind, &pane_id)?;

        deliver_prompt(herdr, log, &name, &pane_id, &p.prompt);
        Ok(pane_id)
    })();

    match result {
        Ok(pane_id) => {
            log.info(format!(
                "dispatched {} → ws:{} pane {} ({}, attempt {}, by {})",
                p.identifier,
                p.workspace,
                pane_id,
                p.runtime,
                p.attempt_no,
                p.dispatched_by.as_deref().unwrap_or("operator"),
            ));
            engine.enqueue_dispatch(
                task,
                &p.runtime,
                &p.workspace,
                p.attempt_no,
                p.dispatched_by.as_deref(),
            )?;
            Ok(p)
        }
        Err(e) => {
            // Never leave a live attempt behind for a dispatch that did not
            // happen — it would block every retry via the unique index.
            log.error(format!("dispatch of {} failed: {e}", p.identifier));
            engine.db.close_attempt(attempt_id, Outcome::Failed)?;
            Err(e)
        }
    }
}

/// Start the agent, waiting for the new pane's shell to be ready.
///
/// `tab create` returns as soon as the pane exists, but `agent start` requires
/// "an available shell pane" — the shell at its prompt owning the foreground.
/// A shell takes a moment to get there, so starting immediately races it and
/// herdr answers `agent_pane_busy`, leaving an empty terminal and no agent.
/// There is no "shell ready" signal to wait on, so this retries.
fn start_agent_when_ready(
    herdr: &Herdr,
    log: &Logger,
    name: &str,
    kind: &str,
    pane_id: &str,
) -> Result<()> {
    const ATTEMPTS: u32 = 12;
    let mut last: Option<anyhow::Error> = None;
    for i in 0..ATTEMPTS {
        match herdr.agent_start(name, kind, pane_id, 60_000) {
            Ok(()) => {
                if i > 0 {
                    log.info(format!("agent {name} started after {} retries", i));
                }
                return Ok(());
            }
            // Only this error means "not yet"; anything else is a real failure
            // and retrying it just delays the report.
            Err(e) if e.to_string().contains("agent_pane_busy") => {
                std::thread::sleep(std::time::Duration::from_millis(500));
                last = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("agent {name} never became startable")))
}

/// Send the prompt, and make sure it actually arrived.
///
/// `agent start` returns once herdr *detects* the agent, but a full-screen agent
/// is often still painting its welcome screen and silently swallows a paste that
/// arrives too early. herdr reports the send as successful either way, so the
/// only evidence of delivery is the agent leaving its idle state.
///
/// Deliberately still not `--wait`: that would block for the length of the whole
/// turn. This waits only for the agent to *start* reacting.
fn deliver_prompt(herdr: &Herdr, log: &Logger, name: &str, pane_id: &str, prompt: &str) {
    // Let the agent's UI settle before typing into it.
    for _ in 0..20 {
        if herdr.agent_status(name).is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // Delivered is not the same as sent.
    //
    // `agent prompt` is documented to submit text plus Enter atomically, and it
    // usually does — but against a full-screen agent that is still settling it
    // can leave the text sitting unsent in the input box. The pane changes
    // either way, so watching the screen cannot tell the two apart; only the
    // agent actually starting work can.
    let _ = pane_id;
    if let Err(e) = herdr.agent_prompt(name, prompt) {
        log.warn(format!("prompt delivery failed for {name}: {e}"));
        return;
    }
    if started(herdr, name, 16) {
        return;
    }

    // Text pasted but never submitted. Nudge it — and only ever nudge: sending
    // the prompt a second time leaves the agent reading the same instructions
    // twice, which it notices and comments on.
    for nudge in 1..=3 {
        log.info(format!("{name} has not started; sending enter ({nudge})"));
        if herdr.agent_send_keys(name, &["enter"]).is_err() {
            break;
        }
        if started(herdr, name, 20) {
            log.info(format!("prompt for {name} needed an explicit enter"));
            return;
        }
    }
    log.error(format!(
        "{name} never started work — it may be sitting on an unsent prompt"
    ));
}

/// Did the agent start reacting within `ticks` quarter-seconds?
fn started(herdr: &Herdr, name: &str, ticks: u32) -> bool {
    for _ in 0..ticks {
        std::thread::sleep(std::time::Duration::from_millis(250));
        match herdr.agent_status(name) {
            Some(AgentStatus::Working) | Some(AgentStatus::Blocked) => return true,
            // Gone: nothing left to wait for.
            None => return true,
            _ => {}
        }
    }
    false
}

/// The agent that released a task, as the operator would recognise it.
///
/// Cancelling is the one board action with a consequence off the board: it can
/// strand a parent agent that is blocked on the child. The herd has no push
/// channel — the board is how agents learn anything (see `list --json`'s
/// `last_outcome`) — so the operator is the only actor who can go tell the
/// parent, and that only works if the board says a parent exists.
#[derive(Debug, Clone)]
pub struct Parent {
    /// The parent's issue identifier, e.g. `LIN-138`. Falls back to the raw task
    /// id if the row is not on the board.
    pub identifier: String,
    /// The parent still holds a live attempt, so it is plausibly waiting. A
    /// parent that has already finished is named without the claim.
    pub live: bool,
}

impl Parent {
    /// The clause appended to a cancel confirmation.
    pub fn phrase(&self) -> String {
        if self.live {
            format!("released by {}, which may be waiting on it", self.identifier)
        } else {
            format!("released by {}", self.identifier)
        }
    }
}

/// Resolve a `dispatched_by` task id into something worth showing.
fn parent_of(db: &Db, task_id: &str) -> Parent {
    match db.get_task(task_id) {
        Ok(Some(p)) => Parent {
            identifier: p.identifier.clone(),
            live: p.live_attempt().is_some(),
        },
        // The parent's row may have been reaped; the id is still the truth we
        // have, and naming it beats saying nothing.
        _ => Parent {
            identifier: task_id.to_string(),
            live: false,
        },
    }
}

/// Cancel a live attempt: kill the pane, close the attempt, queue the trail.
///
/// Cancelling ends the **attempt**, not the issue — the task derives back to
/// `ready` with its history intact.
///
/// Returns the parent agent that released this task, when one did. The caller is
/// expected to say so: the parent is not notified — nothing in the herd pushes —
/// and the operator who pressed `x` is the only one in a position to tell it.
pub fn cancel(
    engine: &SyncEngine,
    herdr: &Herdr,
    log: &Logger,
    task: &Task,
) -> Result<Option<Parent>> {
    let Some(attempt) = task.live_attempt() else {
        bail!("{} has no live attempt", task.identifier);
    };
    // Read the parent off the attempt before closing it: afterwards there is no
    // live attempt to read it from.
    let parent = attempt
        .dispatched_by
        .as_deref()
        .map(|id| parent_of(&engine.db, id));
    if let Some(pane) = attempt.pane_id.as_deref()
        && let Err(e) = herdr.pane_close(pane)
    {
        // The pane may already be gone; that is not a reason to keep the
        // attempt open.
        log.warn(format!("closing pane {pane}: {e}"));
    }
    engine.db.close_attempt(attempt.id, Outcome::Cancelled)?;
    engine.enqueue_outcome(task, Outcome::Cancelled, None)?;
    match &parent {
        Some(p) => log.info(format!(
            "cancelled {} — {} (not notified; the board is the channel)",
            task.identifier,
            p.phrase()
        )),
        None => log.info(format!("cancelled {}", task.identifier)),
    }
    Ok(parent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::db::UpsertTask;
    use crate::model::{Outcome, Source, UpstreamState};

    fn cfg() -> RoutingConfig {
        toml::from_str(
            r#"
[sync]
labels = ["herd"]

[[route]]
match = { linear_team = "LIN" }
workspace = "offhand"
repo = "/tmp/repo"
runtime = "claude-code"
prompt = """
You are working on: {title} ({identifier})
{body}
Branch {branch} is prepared."""

[defaults]
max_concurrent_per_workspace = 2
branch_template = "board/{identifier_lower}"
"#,
        )
        .unwrap()
    }

    fn paths() -> Paths {
        let d = std::env::temp_dir().join(format!("hb-dispatch-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        Paths {
            config_dir: d.clone(),
            state_dir: d,
        }
    }

    fn task(db: &Db) -> Task {
        db.upsert_task(&UpsertTask {
            id: "linear:LIN-145".into(),
            source: Source::Linear,
            source_id: "uuid".into(),
            identifier: "LIN-145".into(),
            title: "Add retry to Altinn poller".into(),
            body: Some("The poller gives up on the first 502.".into()),
            url: "https://linear.app/x/issue/LIN-145".into(),
            labels: vec!["herd".into()],
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
        db.get_task("linear:LIN-145").unwrap().unwrap()
    }

    /// A live attempt in another pane, as if an agent were working there.
    fn working_agent(db: &Db, task_id: &str, pane: &str) {
        db.upsert_task(&crate::db::UpsertTask {
            id: task_id.into(),
            source: Source::Linear,
            source_id: "u".into(),
            identifier: task_id.trim_start_matches("linear:").into(),
            title: "parent".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Started,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let a = db
            .insert_attempt(&NewAttempt {
                task_id: task_id.into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "codex".into(),
                worktree: None,
                branch: None,
                dispatched_by: None,
            })
            .unwrap();
        db.set_attempt_pane(a, pane).unwrap();
    }

    fn engine_with(db: Db) -> SyncEngine {
        SyncEngine {
            db,
            cfg: cfg(),
            credentials: Default::default(),
            paths: paths(),
            log: std::sync::Arc::new(Logger::new("", false)),
            linear: None,
            github: None,
        }
    }

    /// Cancelling an agent-dispatched task hands the parent back, so the
    /// operator can be told. Nothing in the herd pushes; this is the whole
    /// notification path (AGE-3).
    #[test]
    fn cancelling_a_child_names_the_parent_that_released_it() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        working_agent(&db, "linear:LIN-138", "w1:p4");
        // The child, released by LIN-138 and still live.
        db.insert_attempt(&NewAttempt {
            task_id: t.id.clone(),
            pane_id: None,
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: None,
            dispatched_by: Some("linear:LIN-138".into()),
        })
        .unwrap();
        let engine = engine_with(db);
        let t = engine.db.get_task(&t.id).unwrap().unwrap();
        let h = Herdr::discover(engine.log.clone());
        let parent = cancel(&engine, &h, &engine.log, &t).unwrap().unwrap();
        // Named by identifier, not the raw task id — that is what is on screen.
        assert_eq!(parent.identifier, "LIN-138");
        assert!(parent.live, "LIN-138 still holds its own attempt");
        assert_eq!(
            parent.phrase(),
            "released by LIN-138, which may be waiting on it"
        );
    }

    /// A parent that has already finished is named without the claim that it is
    /// waiting — the operator should not be sent after an agent that is gone.
    #[test]
    fn a_finished_parent_is_named_but_not_called_waiting() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        working_agent(&db, "linear:LIN-138", "w1:p4");
        let parent_attempt = db.live_attempts().unwrap()[0].id;
        db.close_attempt(parent_attempt, Outcome::Done).unwrap();
        db.insert_attempt(&NewAttempt {
            task_id: t.id.clone(),
            pane_id: None,
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: None,
            dispatched_by: Some("linear:LIN-138".into()),
        })
        .unwrap();
        let engine = engine_with(db);
        let t = engine.db.get_task(&t.id).unwrap().unwrap();
        let h = Herdr::discover(engine.log.clone());
        let parent = cancel(&engine, &h, &engine.log, &t).unwrap().unwrap();
        assert_eq!(parent.phrase(), "released by LIN-138");
    }

    #[test]
    fn cancelling_an_operator_dispatched_task_names_nobody() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        db.insert_attempt(&NewAttempt {
            task_id: t.id.clone(),
            pane_id: None,
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: None,
            dispatched_by: None,
        })
        .unwrap();
        let engine = engine_with(db);
        let t = engine.db.get_task(&t.id).unwrap().unwrap();
        let h = Herdr::discover(engine.log.clone());
        assert!(cancel(&engine, &h, &engine.log, &t).unwrap().is_none());
    }

    /// A reaped parent leaves an id with no row behind it. The id is still the
    /// truth we have, and naming it beats saying nothing.
    #[test]
    fn a_parent_whose_row_is_gone_is_named_by_its_id() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        db.insert_attempt(&NewAttempt {
            task_id: t.id.clone(),
            pane_id: None,
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: None,
            dispatched_by: Some("linear:LIN-999".into()),
        })
        .unwrap();
        let engine = engine_with(db);
        let t = engine.db.get_task(&t.id).unwrap().unwrap();
        let h = Herdr::discover(engine.log.clone());
        let parent = cancel(&engine, &h, &engine.log, &t).unwrap().unwrap();
        assert_eq!(parent.identifier, "linear:LIN-999");
        assert!(!parent.live);
    }

    #[test]
    fn a_dispatch_from_an_agents_pane_records_that_agent_as_the_parent() {
        // The primary agent-initiated path: no flag, just the pane it ran in.
        let db = Db::open_in_memory().unwrap();
        task(&db);
        working_agent(&db, "linear:LIN-138", "w1:p4");
        assert_eq!(
            dispatcher_from(&db, None, Some("w1:p4")).as_deref(),
            Some("linear:LIN-138")
        );
    }

    #[test]
    fn a_dispatch_from_the_board_or_picker_is_the_operators() {
        let db = Db::open_in_memory().unwrap();
        task(&db);
        working_agent(&db, "linear:LIN-138", "w1:p4");
        // The board pane owns no attempt...
        assert_eq!(dispatcher_from(&db, None, Some("w9:p1")), None);
        // ...and a popup gets no pane id at all.
        assert_eq!(dispatcher_from(&db, None, None), None);
    }

    #[test]
    fn an_explicit_via_wins_over_the_calling_pane() {
        let db = Db::open_in_memory().unwrap();
        task(&db);
        working_agent(&db, "linear:LIN-138", "w1:p4");
        assert_eq!(
            dispatcher_from(&db, Some("linear:LIN-999"), Some("w1:p4")).as_deref(),
            Some("linear:LIN-999")
        );
    }

    #[test]
    fn a_pane_whose_attempt_has_ended_is_no_longer_a_parent() {
        let db = Db::open_in_memory().unwrap();
        task(&db);
        working_agent(&db, "linear:LIN-138", "w1:p4");
        let live = db.live_attempts().unwrap();
        db.close_attempt(live[0].id, Outcome::Done).unwrap();
        assert_eq!(dispatcher_from(&db, None, Some("w1:p4")), None);
    }

    #[test]
    fn the_plan_carries_provenance_into_the_attempt() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let p = plan(
            &db,
            &cfg(),
            &paths(),
            &t,
            &Overrides {
                via: Some("linear:LIN-138".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p.dispatched_by.as_deref(), Some("linear:LIN-138"));
    }

    #[test]
    fn plan_resolves_route_branch_and_runtime_kind() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let p = plan(&db, &cfg(), &paths(), &t, &Overrides::default()).unwrap();
        assert_eq!(p.workspace, "offhand");
        assert_eq!(p.branch, "board/lin-145");
        assert_eq!(p.runtime, "claude-code");
        // The display name stays as configured; only the herdr kind translates.
        assert_eq!(p.herdr_kind, "claude");
        assert_eq!(p.attempt_no, 1);
    }

    #[test]
    fn a_github_identifier_makes_a_shell_safe_branch() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_task(&crate::db::UpsertTask {
            id: "gh:acme/widgets#506".into(),
            source: Source::Github,
            source_id: "n".into(),
            identifier: "gh#506".into(),
            title: "t".into(),
            body: None,
            url: "u".into(),
            labels: vec!["herd".into()],
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let t = db.get_task("gh:acme/widgets#506").unwrap().unwrap();
        let c: RoutingConfig = toml::from_str(
            r#"
[[route]]
match = { gh_repo = "acme/widgets" }
workspace = "tally"
repo = "/tmp"
runtime = "claude-code"
"#,
        )
        .unwrap();
        let route = c.resolve(&crate::sync::route_context(&t)).unwrap();
        let branch = resolve_branch(&c, route, &t);
        assert_eq!(branch, "board/gh-506");
        assert!(!branch.contains('#'), "{branch} is hostile in a shell");
    }

    #[test]
    fn the_resolved_prompt_is_interpolated_not_the_template() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let p = plan(&db, &cfg(), &paths(), &t, &Overrides::default()).unwrap();
        assert!(p.prompt.contains("Add retry to Altinn poller (LIN-145)"));
        assert!(p.prompt.contains("The poller gives up on the first 502."));
        assert!(p.prompt.contains("Branch board/lin-145 is prepared."));
        assert!(!p.prompt.contains('{'), "unresolved placeholder: {}", p.prompt);
    }

    #[test]
    fn picker_overrides_win_over_the_route() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let p = plan(
            &db,
            &cfg(),
            &paths(),
            &t,
            &Overrides {
                workspace: Some("fintech".into()),
                runtime: Some("codex".into()),
                branch: Some("custom-branch".into()),
                via: None,
            },
        )
        .unwrap();
        assert_eq!(p.workspace, "fintech");
        assert_eq!(p.herdr_kind, "codex");
        assert_eq!(p.branch, "custom-branch");
    }

    #[test]
    fn a_task_with_no_route_cannot_be_planned() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_task(&UpsertTask {
            id: "gh:o/r#91".into(),
            source: Source::Github,
            source_id: "n".into(),
            identifier: "gh#91".into(),
            title: "x".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let t = db.get_task("gh:o/r#91").unwrap().unwrap();
        let err = plan(&db, &cfg(), &paths(), &t, &Overrides::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("no route for gh#91"), "{err}");
    }

    #[test]
    fn at_cap_is_reported_from_live_attempts_in_that_workspace() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let c = cfg();
        for id in ["linear:LIN-1", "linear:LIN-2"] {
            db.upsert_task(&UpsertTask {
                id: id.into(),
                source: Source::Linear,
                source_id: "u".into(),
                identifier: id.into(),
                title: "t".into(),
                body: None,
                url: "u".into(),
                labels: vec![],
                source_state: None,
                linear_team: None,
                linear_project: None,
                upstream: UpstreamState::Unstarted,
                updated_at: crate::db::now(),
            })
            .unwrap();
            db.insert_attempt(&NewAttempt {
                task_id: id.into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: None,
                branch: None,
                dispatched_by: None,
            })
            .unwrap();
        }
        let p = plan(&db, &c, &paths(), &t, &Overrides::default()).unwrap();
        assert_eq!(p.live_in_workspace, 2);
        assert_eq!(p.max_concurrent, 2);
        assert!(p.at_cap());
    }

    #[test]
    fn the_attempt_number_advances_the_worktree_path() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let a = db
            .insert_attempt(&NewAttempt {
                task_id: t.id.clone(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: None,
                branch: None,
                dispatched_by: None,
            })
            .unwrap();
        db.close_attempt(a, Outcome::Cancelled).unwrap();
        let t = db.get_task("linear:LIN-145").unwrap().unwrap();
        let p = plan(&db, &cfg(), &paths(), &t, &Overrides::default()).unwrap();
        assert_eq!(p.attempt_no, 2);
        assert!(p.worktree.to_string_lossy().ends_with("lin-145-2"));
        // A retry reuses the same branch, so the work is not stranded.
        assert_eq!(p.branch, "board/lin-145");
    }
}
