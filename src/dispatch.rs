//! Dispatch: route → worktree → herdr pane → agent → prompt (impl spec §6).

use crate::config::{Route, RoutingConfig, herdr_kind_for_runtime, interpolate};
use crate::db::{Db, NewAttempt};
use crate::herdr::{Herdr, agent_name};
use crate::log::Logger;
use crate::model::{Dispatcher, Outcome, Task};
use crate::sync::{SyncEngine, route_context};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The repo's current HEAD, which is what a new worktree branches from.
fn head_sha(repo: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", &repo.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !sha.is_empty()).then_some(sha)
}

/// Per-dispatch choices the picker can override.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub workspace: Option<String>,
    pub runtime: Option<String>,
    pub branch: Option<String>,
    /// Parent task id, when an agent is releasing this task rather than the
    /// operator. Normally left `None`: the calling pane already identifies the
    /// agent, and this only adds the *task* to name it by — which an
    /// orchestrator the board never dispatched does not have.
    pub via: Option<String>,
}

/// Who released a task.
///
/// Agent-initiated dispatch is a primary path: any agent can run
/// `herdr-board dispatch --task <id>` from its own pane, and the row lands on
/// the board within a tick. Rows therefore appear in `working` that the operator
/// never released, routinely — so every dispatch records its origin.
///
/// Reads the environment and asks herdr what is in the calling pane; the
/// decision itself is [`dispatcher_from`].
pub fn resolve_dispatcher(db: &Db, herdr: &Herdr, explicit: Option<&str>) -> Dispatcher {
    // A command running inside a herdr pane carries that pane's id — every
    // pane, not only the ones the board dispatched.
    let pane = std::env::var("HERDR_PANE_ID").ok().filter(|p| !p.is_empty());
    // The board records its own pane so the link handler can focus it; here it
    // is what separates a keypress from an agent.
    let board = db
        .meta_get(crate::ui::app::BOARD_PANE)
        .ok()
        .flatten()
        .filter(|p| !p.is_empty());
    // Only herdr can say whether a pane holds an agent at all. A pane it does
    // not know — a popup already torn down — is not claimed as one.
    //
    // Asked only when it is the deciding fact: the board's own pane and a pane
    // the board already has a live attempt for are settled without a
    // subprocess, which keeps it off the picker's keystroke path.
    let undecided = pane
        .as_deref()
        .filter(|p| Some(*p) != board.as_deref())
        .filter(|p| !matches!(db.live_attempt_for_pane(p), Ok(Some(_))));
    let has_agent = undecided.is_some_and(|p| {
        herdr
            .pane_get(p)
            .ok()
            .flatten()
            .is_some_and(|info| info.agent.is_some())
    });
    dispatcher_from(db, explicit, pane.as_deref(), board.as_deref(), has_agent)
}

/// The provenance decision, without reading the environment.
///
/// The pane is the identity. `pane_has_agent` is herdr's answer for the calling
/// pane, and a live attempt owning that pane is the same answer arrived at from
/// the board's own records — either one makes this an agent, and the attempt
/// additionally names its task.
///
/// The operator is then the narrow case it should be: a keypress on the board
/// (which dispatches from the board's own pane), the picker popup (which gets
/// no pane id at all), or a CLI run from a pane with no agent in it. Not merely
/// "no attempt owns this pane" — that was true of every orchestrator pane, and
/// it is why provenance never fired (AGE-24).
pub fn dispatcher_from(
    db: &Db,
    explicit: Option<&str>,
    pane: Option<&str>,
    board_pane: Option<&str>,
    pane_has_agent: bool,
) -> Dispatcher {
    // The board dispatching from its own pane is the operator pressing a key,
    // whatever herdr reports is running there.
    let pane = pane.filter(|p| !p.is_empty() && Some(*p) != board_pane);
    let live = pane.and_then(|p| db.live_attempt_for_pane(p).ok().flatten());
    let task = explicit
        .map(str::to_string)
        .or_else(|| live.as_ref().map(|a| a.task_id.clone()));
    // `--via` names a parent; it does not turn the shell it was typed in into
    // an agent, and recording that pane would give the follow-up notifier a
    // wrong address to deliver to.
    let pane = pane
        .filter(|_| pane_has_agent || live.is_some())
        .map(str::to_string);
    Dispatcher::agent(task, pane)
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
    /// Who is releasing this task.
    pub dispatcher: Dispatcher,
}

impl Plan {
    /// At cap: the picker states the fact and removes `enter` from its footer.
    pub fn at_cap(&self) -> bool {
        self.live_in_workspace >= self.max_concurrent
    }
}

/// The branch-safe form of a task's identifier, and the value of
/// `{identifier_lower}`.
///
/// Slugified, not merely lowercased: a GitHub identifier lowercases to `gh#506`,
/// which makes a branch name that git tolerates and every shell does not.
///
/// **Repo-qualified for GitHub**, because `gh#2` is unique in its repository and
/// nowhere else. Issue 2 of every watched repo rendered the same `board/gh-2`,
/// and the board keys real decisions on that string: which pull request belongs
/// to which task, and which polled pull request is already somebody's attempt.
/// A merged PR in another repo attached itself to an untouched task and derived
/// it straight to review (AGE-20) — the same ambiguity the display fix for
/// `gh#507` named in the UI, in the data path. Linear identifiers carry their
/// team and are globally unique, so they are unchanged: `LIN-145` → `lin-145`.
///
/// The identifier leads and the repo follows, so that the part which
/// distinguishes two issues survives [`agent_name`]'s 32-character cap.
pub fn branch_slug(task: &Task) -> String {
    match crate::model::gh_repo_name(&task.id) {
        Some(repo) => crate::config::slugify(&format!("{}-{repo}", task.identifier)),
        None => crate::config::slugify(&task.identifier),
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
    v.insert("identifier_lower", branch_slug(task));
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
///
/// Takes `herdr` because provenance does: whether the calling pane holds an
/// agent is herdr's to answer, and the picker shows the answer before anything
/// is created.
pub fn plan(
    db: &Db,
    herdr: &Herdr,
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
        .join(format!("{}-{}", branch_slug(task), attempt_no));

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
        dispatcher: resolve_dispatcher(db, herdr, ov.via.as_deref()),
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
    let p = plan(&engine.db, herdr, &engine.cfg, &engine.paths, task, ov)?;

    if p.at_cap() {
        bail!(
            "ws:{} is at {} of {} working — cancel one first",
            p.workspace,
            p.live_in_workspace,
            p.max_concurrent
        );
    }

    // Where the agent starts from. herdr cuts the worktree from the repo's
    // current HEAD, so reading it here — before the branch exists — records the
    // attempt's true starting point. Without this, "has the agent produced
    // anything" gets measured against the remote, and a repo sitting one commit
    // ahead of its origin looks finished before the prompt is even sent.
    let base_sha = head_sha(&p.repo);
    if base_sha.is_none() {
        log.info(format!(
            "{}: could not read HEAD of {} — completion will fall back to a \
             remote-relative commit count",
            p.identifier,
            p.repo.display()
        ));
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
        dispatched_by: p.dispatcher.task().map(str::to_string),
        dispatched_by_pane: p.dispatcher.pane().map(str::to_string),
        base_sha,
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

        // Where this attempt *actually* starts, now that the checkout exists.
        //
        // The provisional base recorded before dispatch is the repo's HEAD,
        // which is right only for a branch that did not exist yet. A retry
        // reuses the branch, and the branch already carries the previous
        // attempt's commits — so measuring from the repo's HEAD counted work
        // this attempt had not done, and closed it as `done` seconds after it
        // started. Seen live on gh#71: cancelled, re-dispatched, marked done
        // 62 seconds later with four commits from the cancelled run, while its
        // agent was still typing (herdr-board#10).
        //
        // The worktree's own HEAD is the honest answer for both cases: for a
        // fresh branch it equals the repo HEAD, and for a reused one it is the
        // tip the agent is building on.
        match head_sha(&wt.path) {
            Some(sha) => engine.db.set_attempt_base_sha(attempt_id, &sha)?,
            None => log.info(format!(
                "{}: could not read HEAD of {} — completion falls back to the \
                 provisional base",
                p.identifier,
                wt.path.display()
            )),
        }
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

        // Named from the branch slug rather than the identifier: herdr requires
        // agent names to be unique among live agents, and two repos releasing
        // their issue 2 at once both asked it for `gh-2-1` (AGE-20).
        let name = agent_name(&branch_slug(task), p.attempt_no);
        start_agent_when_ready(herdr, log, &name, p.herdr_kind, &pane_id)?;

        crate::wake::first_prompt(herdr, log, &name, &p.prompt);
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
                dispatcher_phrase(&engine.db, &p.dispatcher),
            ));
            engine.enqueue_dispatch(
                task,
                &p.runtime,
                &p.workspace,
                p.attempt_no,
                dispatcher_name(&engine.db, &p.dispatcher).as_deref(),
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

/// A parent task's issue identifier, falling back to the raw task id when the
/// row has been reaped — the id is still the truth we have, and naming it beats
/// saying nothing.
fn identifier_for(db: &Db, task_id: &str) -> String {
    db.get_task(task_id)
        .ok()
        .flatten()
        .map(|t| t.identifier)
        .unwrap_or_else(|| task_id.to_string())
}

/// The short name for a dispatcher: the parent's issue identifier when the
/// board dispatched it too, the pane it is running in otherwise. `None` is the
/// operator, who is named by the surrounding copy rather than by this.
pub fn dispatcher_name(db: &Db, d: &Dispatcher) -> Option<String> {
    match d {
        Dispatcher::Operator => None,
        Dispatcher::Agent { task, pane } => task
            .as_deref()
            .map(|id| identifier_for(db, id))
            .or_else(|| pane.clone()),
    }
}

/// The same, in prose. A bare pane id needs saying what it is.
pub fn dispatcher_phrase(db: &Db, d: &Dispatcher) -> String {
    match d {
        Dispatcher::Operator => "you".to_string(),
        Dispatcher::Agent { task: Some(id), .. } => identifier_for(db, id),
        Dispatcher::Agent { pane: Some(p), .. } => format!("the agent in {p}"),
        // `Dispatcher::agent` collapses "neither" to `Operator`, so this arm is
        // unreachable — but not worth a panic if it ever stops being.
        Dispatcher::Agent { .. } => "an agent".to_string(),
    }
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
    /// The parent's issue identifier, e.g. `LIN-138` — or the pane it is
    /// running in, which is all there is to go on for an orchestrator the board
    /// never dispatched.
    pub name: String,
    /// The parent still holds a live attempt, or its pane is still open, so it
    /// is plausibly waiting. A parent that has already finished is named
    /// without the claim.
    pub live: bool,
    /// Where the parent is, when the dispatch recorded a pane. This is the
    /// address the operator would go to — and, in time, the one a notifier
    /// would deliver to.
    pub pane: Option<String>,
}

impl Parent {
    /// The clause appended to a cancel confirmation.
    pub fn phrase(&self) -> String {
        if self.live {
            format!("released by {}, which may be waiting on it", self.name)
        } else {
            format!("released by {}", self.name)
        }
    }
}

/// Resolve a recorded dispatcher into something worth showing.
///
/// `pane_live` is whether herdr still knows the pane; it stands in for "is the
/// parent still there" when there is no attempt to read that off.
fn parent_of(db: &Db, d: &Dispatcher, pane_live: bool) -> Option<Parent> {
    match d {
        Dispatcher::Operator => None,
        Dispatcher::Agent { task, pane } => Some(Parent {
            name: dispatcher_phrase(db, d),
            live: match task.as_deref() {
                Some(id) => db
                    .get_task(id)
                    .ok()
                    .flatten()
                    .is_some_and(|p| p.live_attempt().is_some()),
                // Nothing on the board to read; the pane still being open is
                // the whole of what we know.
                None => pane_live,
            },
            pane: pane.clone(),
        }),
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
    let dispatcher = attempt.dispatcher();
    // Only asked when the answer is load-bearing — a parent known only by its
    // pane has nothing else to say whether it is still running.
    let pane_live = dispatcher.task().is_none()
        && dispatcher
            .pane()
            .is_some_and(|p| herdr.pane_get(p).ok().flatten().is_some());
    let parent = parent_of(&engine.db, &dispatcher, pane_live);
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
                dispatched_by_pane: None,
                base_sha: None,
            })
            .unwrap();
        db.set_attempt_pane(a, pane).unwrap();
    }

    /// herdr is only consulted about the calling pane; in tests there is
    /// nothing for it to find, so discovery failing is the point.
    fn herdr() -> Herdr {
        Herdr::discover(std::sync::Arc::new(Logger::new("", false)))
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
            dispatched_by_pane: Some("w1:p4".into()),
            base_sha: None,
        })
        .unwrap();
        let engine = engine_with(db);
        let t = engine.db.get_task(&t.id).unwrap().unwrap();
        let h = Herdr::discover(engine.log.clone());
        let parent = cancel(&engine, &h, &engine.log, &t).unwrap().unwrap();
        // Named by identifier, not the raw task id — that is what is on screen.
        assert_eq!(parent.name, "LIN-138");
        assert!(parent.live, "LIN-138 still holds its own attempt");
        // And reachable: the pane the dispatch came from is the address.
        assert_eq!(parent.pane.as_deref(), Some("w1:p4"));
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
            dispatched_by_pane: Some("w1:p4".into()),
            base_sha: None,
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
            dispatched_by_pane: None,
            base_sha: None,
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
            dispatched_by_pane: None,
            base_sha: None,
        })
        .unwrap();
        let engine = engine_with(db);
        let t = engine.db.get_task(&t.id).unwrap().unwrap();
        let h = Herdr::discover(engine.log.clone());
        let parent = cancel(&engine, &h, &engine.log, &t).unwrap().unwrap();
        assert_eq!(parent.name, "linear:LIN-999");
        assert!(!parent.live);
    }

    /// The case that broke provenance for every attempt ever recorded: a
    /// long-lived orchestrator pane the operator started and keeps around. It
    /// owns no attempt — it is a session, not a dispatch — so the old test
    /// answered "no agent here" and the operator got the credit (AGE-24).
    #[test]
    fn a_dispatch_from_an_orchestrator_pane_records_that_pane() {
        let db = Db::open_in_memory().unwrap();
        task(&db);
        // No attempt owns w7:p1; herdr says there is an agent sitting in it.
        let d = dispatcher_from(&db, None, Some("w7:p1"), Some("w9:p1"), true);
        assert_eq!(
            d,
            Dispatcher::Agent {
                task: None,
                pane: Some("w7:p1".into())
            }
        );
        assert!(d.is_agent());
        assert_eq!(dispatcher_phrase(&db, &d), "the agent in w7:p1");
    }

    /// A board-dispatched agent keeps the richer label — the pane is recorded
    /// as well, but the row can still say `via LIN-138`.
    #[test]
    fn a_dispatch_from_an_agents_pane_records_that_agent_as_the_parent() {
        let db = Db::open_in_memory().unwrap();
        task(&db);
        working_agent(&db, "linear:LIN-138", "w1:p4");
        let d = dispatcher_from(&db, None, Some("w1:p4"), Some("w9:p1"), true);
        assert_eq!(d.task(), Some("linear:LIN-138"));
        assert_eq!(d.pane(), Some("w1:p4"));
        assert_eq!(dispatcher_name(&db, &d).as_deref(), Some("LIN-138"));
    }

    /// The board's own attempt records survive herdr not answering — a pane
    /// with a live attempt is an agent whether or not herdr says so.
    #[test]
    fn a_live_attempt_makes_it_an_agent_even_if_herdr_says_nothing() {
        let db = Db::open_in_memory().unwrap();
        task(&db);
        working_agent(&db, "linear:LIN-138", "w1:p4");
        let d = dispatcher_from(&db, None, Some("w1:p4"), None, false);
        assert_eq!(d.task(), Some("linear:LIN-138"));
        assert_eq!(d.pane(), Some("w1:p4"));
    }

    /// "The operator" means a keypress on the board or a CLI run from a pane
    /// with no agent in it — not merely "no attempt owns this pane".
    #[test]
    fn a_dispatch_from_the_board_or_picker_is_the_operators() {
        let db = Db::open_in_memory().unwrap();
        task(&db);
        working_agent(&db, "linear:LIN-138", "w1:p4");
        // The board dispatches from its own pane, which herdr does report an
        // agent for on some setups. It is still a keypress.
        assert_eq!(
            dispatcher_from(&db, None, Some("w9:p1"), Some("w9:p1"), true),
            Dispatcher::Operator
        );
        // A popup gets no pane id at all.
        assert_eq!(
            dispatcher_from(&db, None, None, Some("w9:p1"), false),
            Dispatcher::Operator
        );
        // And a plain shell the operator typed into is nobody's agent.
        assert_eq!(
            dispatcher_from(&db, None, Some("w3:p9"), Some("w9:p1"), false),
            Dispatcher::Operator
        );
        assert_eq!(dispatcher_phrase(&db, &Dispatcher::Operator), "you");
        assert_eq!(dispatcher_name(&db, &Dispatcher::Operator), None);
    }

    #[test]
    fn an_explicit_via_wins_over_the_calling_pane() {
        let db = Db::open_in_memory().unwrap();
        task(&db);
        working_agent(&db, "linear:LIN-138", "w1:p4");
        assert_eq!(
            dispatcher_from(&db, Some("linear:LIN-999"), Some("w1:p4"), None, true).task(),
            Some("linear:LIN-999")
        );
    }

    /// `--via` names a parent; it does not turn the shell it was typed in into
    /// that parent's pane. Recording it would give the operator — and, later, a
    /// notifier — an address that answers for somebody else.
    #[test]
    fn an_explicit_via_from_a_plain_shell_records_no_pane() {
        let db = Db::open_in_memory().unwrap();
        task(&db);
        let d = dispatcher_from(&db, Some("linear:LIN-138"), Some("w3:p9"), None, false);
        assert_eq!(d.task(), Some("linear:LIN-138"));
        assert_eq!(d.pane(), None);
    }

    /// An agent whose *task* has finished may still be sitting in its pane,
    /// waiting on what it released. It is no longer named by that task — but it
    /// is still an agent, and still reachable.
    #[test]
    fn a_pane_whose_attempt_has_ended_is_still_the_agent_in_it() {
        let db = Db::open_in_memory().unwrap();
        task(&db);
        working_agent(&db, "linear:LIN-138", "w1:p4");
        let live = db.live_attempts().unwrap();
        db.close_attempt(live[0].id, Outcome::Done).unwrap();
        let d = dispatcher_from(&db, None, Some("w1:p4"), None, true);
        assert_eq!(d.task(), None);
        assert_eq!(d.pane(), Some("w1:p4"));
        // With no agent left in the pane either, it is the operator's.
        assert_eq!(
            dispatcher_from(&db, None, Some("w1:p4"), None, false),
            Dispatcher::Operator
        );
    }

    #[test]
    fn the_plan_carries_provenance_into_the_attempt() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let h = Herdr::discover(std::sync::Arc::new(Logger::new("", false)));
        let p = plan(
            &db,
            &h,
            &cfg(),
            &paths(),
            &t,
            &Overrides {
                via: Some("linear:LIN-138".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p.dispatcher.task(), Some("linear:LIN-138"));
    }

    #[test]
    fn plan_resolves_route_branch_and_runtime_kind() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let p = plan(&db, &herdr(), &cfg(), &paths(), &t, &Overrides::default()).unwrap();
        assert_eq!(p.workspace, "offhand");
        assert_eq!(p.branch, "board/lin-145");
        assert_eq!(p.runtime, "claude-code");
        // The display name stays as configured; only the herdr kind translates.
        assert_eq!(p.herdr_kind, "claude");
        assert_eq!(p.attempt_no, 1);
    }

    /// A GitHub issue, whose id carries the repo the number belongs to.
    fn gh_task(db: &Db, id: &str, identifier: &str) -> Task {
        db.upsert_task(&crate::db::UpsertTask {
            id: id.into(),
            source: Source::Github,
            source_id: "n".into(),
            identifier: identifier.into(),
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
        db.get_task(id).unwrap().unwrap()
    }

    #[test]
    fn a_github_identifier_makes_a_shell_safe_branch() {
        let db = Db::open_in_memory().unwrap();
        let t = gh_task(&db, "gh:acme/widgets#506", "gh#506");
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
        assert_eq!(branch, "board/gh-506-widgets");
        assert!(!branch.contains('#'), "{branch} is hostile in a shell");
    }

    /// AGE-20, seen live: `gh#2` exists in every repo that has two issues, and
    /// `board/{identifier_lower}` gave them all one branch. The board keys real
    /// decisions on that string — which pull request is whose — so OIOS's merged
    /// `board/gh-2` attached itself to tripletex-mcp's untouched task.
    #[test]
    fn two_repos_issue_two_do_not_share_a_branch() {
        let db = Db::open_in_memory().unwrap();
        let tripletex = gh_task(&db, "gh:Florin-AS/tripletex-mcp#2", "gh#2");
        let oios = gh_task(&db, "gh:bredebjorhovd/OIOS#2", "gh#2");

        assert_eq!(branch_slug(&tripletex), "gh-2-tripletex-mcp");
        assert_eq!(branch_slug(&oios), "gh-2-oios");
        // The identifier leads so that the cap on herdr agent names truncates
        // the repo rather than the number that tells two issues apart.
        assert_eq!(agent_name(&branch_slug(&tripletex), 1), "gh-2-tripletex-mcp-1");
        assert_ne!(
            agent_name(&branch_slug(&tripletex), 1),
            agent_name(&branch_slug(&oios), 1),
            "two repos releasing their issue 2 must not ask herdr for one name"
        );
    }

    /// Linear identifiers carry their team and are unique across every project
    /// the board watches, so nothing about them changes — a branch an operator
    /// has in a worktree today is the branch they get tomorrow.
    #[test]
    fn a_linear_branch_is_left_alone() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(branch_slug(&task(&db)), "lin-145");
    }

    #[test]
    fn the_resolved_prompt_is_interpolated_not_the_template() {
        let db = Db::open_in_memory().unwrap();
        let t = task(&db);
        let p = plan(&db, &herdr(), &cfg(), &paths(), &t, &Overrides::default()).unwrap();
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
            &herdr(),
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
        let err = plan(&db, &herdr(), &cfg(), &paths(), &t, &Overrides::default())
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
                dispatched_by_pane: None,
                base_sha: None,
            })
            .unwrap();
        }
        let p = plan(&db, &herdr(), &c, &paths(), &t, &Overrides::default()).unwrap();
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
                dispatched_by_pane: None,
                base_sha: None,
            })
            .unwrap();
        db.close_attempt(a, Outcome::Cancelled).unwrap();
        let t = db.get_task("linear:LIN-145").unwrap().unwrap();
        let p = plan(&db, &herdr(), &cfg(), &paths(), &t, &Overrides::default()).unwrap();
        assert_eq!(p.attempt_no, 2);
        assert!(p.worktree.to_string_lossy().ends_with("lin-145-2"));
        // A retry reuses the same branch, so the work is not stranded.
        assert_eq!(p.branch, "board/lin-145");
    }
}
