//! Dispatch: route → worktree → herdr pane → agent → prompt (impl spec §6).

use crate::config::{Route, RoutingConfig, herdr_kind_for_runtime, interpolate};
use crate::db::{Db, NewAttempt};
use crate::herdr::{Herdr, agent_name};
use crate::log::Logger;
use crate::model::{Outcome, Task};
use crate::sync::{SyncEngine, route_context};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

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
        // The actual worktree may differ from the planned path: a retry reuses
        // the one already holding this branch.
        let worktree = prepare_worktree(&p.repo, &p.worktree, &p.branch, log)?;
        if worktree != p.worktree {
            engine
                .db
                .conn
                .execute(
                    "UPDATE attempts SET worktree = ?2 WHERE id = ?1",
                    rusqlite::params![attempt_id, worktree.to_string_lossy()],
                )
                .ok();
        }

        let workspace_id = herdr
            .workspace_id_for_label(&p.workspace)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no herdr workspace labelled `{}` — create it, or fix routing.toml",
                    p.workspace
                )
            })?;

        // Split into the routed workspace's active tab, so the agent is
        // visible there and its approval prompts can be answered. A tab per
        // attempt hid the agent until you went looking for it.
        let placement =
            herdr.agent_placement(&workspace_id, engine.cfg.defaults.max_panes_per_tab);
        let pane_id = match placement {
            Some(crate::herdr::Placement::Split { target, direction }) => {
                let direction = engine
                    .cfg
                    .defaults
                    .split_direction
                    .as_deref()
                    .filter(|d| *d != "auto")
                    .unwrap_or(direction);
                log.info(format!("splitting {target} {direction} for the agent"));
                herdr.pane_split(&target, &worktree, direction)?
            }
            // The tab is full. A fourth sliver helps nobody; the agent gets a
            // tab of its own, labelled so the tab bar says which task it is.
            Some(crate::herdr::Placement::NewTab) | None => {
                log.info(format!(
                    "tab is at its pane limit; giving {} a tab of its own",
                    p.identifier
                ));
                let tab = herdr.tab_create(&workspace_id, &worktree, &p.identifier)?;
                tab.root_pane_id
            }
        };
        log.info(format!("agent pane {pane_id} in ws:{}", p.workspace));
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

    // Delivery is confirmed by the screen changing, not by the agent's reported
    // state. herdr's detection can report `idle` for an agent that is visibly
    // working — Claude Code 2.1.220 keeps an empty prompt box live while it
    // thinks, and the `live_prompt_box` rule matches it — so waiting for
    // `working` produces false negatives and re-sends a prompt that landed.
    let before = herdr.pane_read_visible(pane_id).unwrap_or_default();

    for attempt in 1..=2 {
        if let Err(e) = herdr.agent_prompt(name, prompt) {
            log.warn(format!("prompt delivery failed for {name}: {e}"));
            return;
        }
        for _ in 0..24 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            match herdr.pane_read_visible(pane_id) {
                Some(now) if now != before => {
                    if attempt > 1 {
                        log.info(format!("prompt for {name} landed on attempt {attempt}"));
                    }
                    return;
                }
                None => return,
                _ => {}
            }
        }
        log.warn(format!(
            "{name} showed no reaction to its prompt (attempt {attempt})"
        ));
    }
    log.error(format!(
        "{name} never reacted to its prompt — it may be running with no instructions"
    ));
}

/// Where a branch is already checked out, if anywhere.
///
/// git allows one worktree per branch, so a retry cannot cut a second checkout
/// of the same branch — and worktrees are never removed automatically, so the
/// previous attempt's is still holding it. Reusing that checkout is also the
/// behaviour you want: a retry should continue the work, not start beside it.
fn worktree_holding_branch(repo: &Path, branch: &str) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["-C", &repo.to_string_lossy(), "worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut current: Option<&str> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(path);
        } else if let Some(head) = line.strip_prefix("branch ")
            && head.trim_start_matches("refs/heads/") == branch
        {
            return current.map(PathBuf::from);
        }
    }
    None
}

/// Create (or reuse) the git worktree for an attempt. Returns the path used.
fn prepare_worktree(
    repo: &Path,
    worktree: &Path,
    branch: &str,
    log: &Logger,
) -> Result<PathBuf> {
    if !repo.exists() {
        bail!("repo path {} does not exist", repo.display());
    }
    if worktree.exists() {
        log.info(format!("reusing worktree {}", worktree.display()));
        return Ok(worktree.to_path_buf());
    }
    if let Some(existing) = worktree_holding_branch(repo, branch) {
        log.info(format!(
            "branch {branch} is already checked out at {}; reusing it",
            existing.display()
        ));
        return Ok(existing);
    }
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Reuse the branch if it already exists (a retry lands on the same branch).
    let exists = Command::new("git")
        .args(["-C", &repo.to_string_lossy(), "rev-parse", "--verify"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let wt = worktree.to_string_lossy().into_owned();
    let repo_s = repo.to_string_lossy().into_owned();
    let mut args: Vec<String> = vec!["-C".into(), repo_s, "worktree".into(), "add".into()];
    if exists {
        args.push(wt);
        args.push(branch.to_string());
    } else {
        args.push(wt);
        args.push("-b".into());
        args.push(branch.to_string());
    }
    log.info(format!("git {}", args.join(" ")));

    let out = Command::new("git")
        .args(&args)
        .output()
        .context("running git worktree add")?;
    if !out.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(worktree.to_path_buf())
}

/// Cancel a live attempt: kill the pane, close the attempt, queue the trail.
///
/// Cancelling ends the **attempt**, not the issue — the task derives back to
/// `ready` with its history intact.
pub fn cancel(engine: &SyncEngine, herdr: &Herdr, log: &Logger, task: &Task) -> Result<()> {
    let Some(attempt) = task.live_attempt() else {
        bail!("{} has no live attempt", task.identifier);
    };
    if let Some(pane) = attempt.pane_id.as_deref()
        && let Err(e) = herdr.pane_close(pane)
    {
        // The pane may already be gone; that is not a reason to keep the
        // attempt open.
        log.warn(format!("closing pane {pane}: {e}"));
    }
    engine.db.close_attempt(attempt.id, Outcome::Cancelled)?;
    engine.enqueue_outcome(task, Outcome::Cancelled, None)?;
    log.info(format!("cancelled {}", task.identifier));
    Ok(())
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
