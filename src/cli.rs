//! Subcommand implementations and the shared engine constructor.

use crate::config::{
    Credentials, Paths, RoutingConfig, github_token, herdr_kind_for_runtime, linear_api_key,
};
use crate::db::Db;
use crate::herdr::Herdr;
use crate::log::Logger;
use crate::sources::github::{Github, HttpRest, Rest};
use crate::sources::linear::{GraphQl, HttpTransport, Linear};
use crate::sync::SyncEngine;
use anyhow::Result;
use std::sync::Arc;

/// Build a sync engine with whichever credentials are present. A missing key is
/// not an error: the board still renders, and `doctor` says what is missing.
pub fn build_engine(
    db: Db,
    cfg: RoutingConfig,
    paths: Paths,
    log: Arc<Logger>,
) -> Result<SyncEngine> {
    let credentials = Credentials::load(&paths);
    build_engine_with_credentials(db, cfg, paths, log, credentials)
}

fn build_engine_with_credentials(
    db: Db,
    cfg: RoutingConfig,
    paths: Paths,
    log: Arc<Logger>,
    credentials: Credentials,
) -> Result<SyncEngine> {
    let linear = credentials
        .linear_api_key
        .clone()
        .and_then(|k| HttpTransport::new(k).ok())
        .map(|t| Linear::new(Box::new(t) as Box<dyn GraphQl>));
    // GitHub is optional entirely; without repos configured it is never polled.
    let github = if cfg.github.repos.is_empty() {
        None
    } else {
        HttpRest::new(credentials.github_token.clone())
            .ok()
            .map(|r| Github::new(Box::new(r) as Box<dyn Rest>))
    };
    Ok(SyncEngine {
        db,
        cfg,
        credentials,
        paths,
        log,
        linear,
        github,
    })
}

pub fn engine_from_paths(paths: &Paths, log: Arc<Logger>) -> Result<SyncEngine> {
    let cfg = RoutingConfig::load_or_default(&paths.routing());
    let db = Db::open(&paths.db())?;
    build_engine(db, cfg, paths.clone(), log)
}

// ---- sync --once -------------------------------------------------------

pub fn sync_once(paths: &Paths, log: Arc<Logger>) -> Result<()> {
    let engine = engine_from_paths(paths, log.clone())?;
    let herdr = Herdr::discover(log);
    engine.sync_once(Some(&herdr))
}

// ---- syncd -------------------------------------------------------------

/// Start the daemon if it is not already running, then exit immediately.
///
/// This is what the `[[startup]]` hook calls. Startup hooks are **one-shot, not
/// supervised**, and they run again when a new server takes over during live
/// handoff — so this must be safe to re-run and must not block the server.
pub fn syncd_ensure(paths: &Paths, log: Arc<Logger>) -> Result<()> {
    if let Some(pid) = running_pid(paths) {
        log.info(format!("syncd already running as pid {pid}"));
        println!("syncd already running (pid {pid})");
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let logfile = paths.logfile();
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&logfile)?;
    let err = out.try_clone()?;

    // Detach: new session, so the daemon survives the hook process exiting.
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("syncd")
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);
    unsafe {
        cmd.pre_exec(|| {
            // setsid() detaches from the controlling terminal; without it the
            // daemon dies with the pane that ran the startup hook.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    write_pidfile(paths, child.id())?;
    log.info(format!("syncd started as pid {}", child.id()));
    println!("syncd started (pid {})", child.id());
    Ok(())
}

/// The pid of a live daemon, if there is one.
///
/// A stale pidfile is the normal case after a crash or a reboot, so liveness is
/// checked rather than trusted.
pub fn running_pid(paths: &Paths) -> Option<u32> {
    let raw = std::fs::read_to_string(paths.pidfile()).ok()?;
    let mut parts = raw.trim().splitn(2, ' ');
    let pid: u32 = parts.next()?.parse().ok()?;
    let recorded_start = parts.next().unwrap_or("").to_string();

    // Signal 0 tests for existence without delivering anything.
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        return None;
    }
    // The pid may have been recycled by an unrelated process; compare the
    // process start time to be sure it is still ours.
    if !recorded_start.is_empty() {
        let now_start = process_start(pid).unwrap_or_default();
        if !now_start.is_empty() && now_start != recorded_start {
            return None;
        }
    }
    Some(pid)
}

fn write_pidfile(paths: &Paths, pid: u32) -> Result<()> {
    let start = process_start(pid).unwrap_or_default();
    std::fs::write(paths.pidfile(), format!("{pid} {start}"))?;
    Ok(())
}

/// Process start time, used to detect a recycled pid.
fn process_start(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// The daemon loop.
pub fn syncd(paths: &Paths, log: Arc<Logger>) -> Result<()> {
    write_pidfile(paths, std::process::id())?;
    let mut engine = engine_from_paths(paths, log.clone())?;
    let herdr = Herdr::discover(log.clone());
    let interval = std::time::Duration::from_secs(engine.cfg.sync.interval_secs());
    log.info(format!(
        "syncd up (pid {}, interval {}s)",
        std::process::id(),
        interval.as_secs()
    ));

    loop {
        // Credentials and routes are read at startup, but the daemon outlives
        // the setup: adding a key or a repo should take effect on the next
        // cycle rather than requiring a restart nobody would think to do.
        if let Some(fresh) = reload_if_configuration_changed(paths, &engine, &log) {
            engine = fresh;
        }
        if let Err(e) = engine.sync_once(Some(&herdr)) {
            log.error(format!("sync cycle failed: {e}"));
        }
        std::thread::sleep(interval);
    }
}

/// Rebuild the engine when credentials or source configuration change.
/// Returns `None` when nothing relevant changed.
pub(crate) fn reload_if_configuration_changed(
    paths: &Paths,
    engine: &SyncEngine,
    log: &Arc<Logger>,
) -> Option<SyncEngine> {
    let credentials = Credentials::load(paths);
    let cfg = RoutingConfig::load_or_default(&paths.routing());
    rebuild_if_configuration_changed(paths, engine, log, credentials, cfg)
}

fn rebuild_if_configuration_changed(
    paths: &Paths,
    engine: &SyncEngine,
    log: &Arc<Logger>,
    credentials: Credentials,
    cfg: RoutingConfig,
) -> Option<SyncEngine> {
    let linear_changed = credentials.linear_api_key != engine.credentials.linear_api_key;
    let github_changed = credentials.github_token != engine.credentials.github_token;
    let repos_changed = cfg.github.repos != engine.cfg.github.repos;
    let routes_changed = cfg.routes.len() != engine.cfg.routes.len();

    if !(linear_changed || github_changed || repos_changed || routes_changed) {
        return None;
    }
    log.info(format!(
        "configuration changed (linear credential:{linear_changed} \
         github credential:{github_changed} \
         repos:{repos_changed} routes:{routes_changed}) — rebuilding"
    ));
    let rebuilt = Db::open(&paths.db()).and_then(|db| {
        build_engine_with_credentials(db, cfg, paths.clone(), log.clone(), credentials)
    });
    match rebuilt {
        Ok(e) => Some(e),
        Err(e) => {
            log.error(format!("could not rebuild after a config change: {e}"));
            None
        }
    }
}

// ---- init --------------------------------------------------------------

/// `owner/repo` from a git remote URL, for the SSH and HTTPS forms.
pub fn github_slug(remote: &str) -> Option<String> {
    let r = remote.trim().trim_end_matches(".git");
    let rest = r
        .strip_prefix("git@github.com:")
        .or_else(|| r.strip_prefix("https://github.com/"))
        .or_else(|| r.strip_prefix("ssh://git@github.com/"))?;
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

fn git_remote(repo_root: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", repo_root, "remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Generate a starter `routing.toml` from the herdr workspaces that actually
/// exist, so nobody has to hand-write repo lists to see anything.
///
/// Linked worktrees are skipped: those are attempts the board itself creates,
/// not projects to route to.
pub fn init(paths: &Paths, log: Arc<Logger>, force: bool) -> Result<()> {
    let target = paths.routing();
    if target.exists() && !force {
        anyhow::bail!(
            "{} already exists; pass --force to overwrite it",
            target.display()
        );
    }
    let herdr = Herdr::discover(log);
    let workspaces = herdr.workspace_list()?;

    let mut routes = String::new();
    let mut repos: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    // Linear routes need team keys, which only the API can supply. With a key
    // present there is no reason to make anyone look them up by hand.
    let mut linear_routes = String::new();
    match linear_api_key(paths) {
        None => {
            linear_routes.push_str(
                "# No LINEAR_API_KEY when this ran, so Linear teams could not be\n\
                 # discovered. Add the key and re-run `herdr-board init --force`,\n\
                 # or add routes by hand:\n\
                 #\n\
                 # [[route]]\n\
                 # match = { linear_team = \"ABC\" }\n\
                 # workspace = \"my-workspace\"\n\
                 # repo = \"~/code/thing\"\n\
                 # runtime = \"claude-code\"\n",
            );
        }
        Some(key) => {
            let teams = crate::sources::linear::HttpTransport::new(key)
                .map(crate::sources::linear::Linear::new)
                .and_then(|l| l.teams());
            match teams {
                Err(e) => {
                    linear_routes
                        .push_str(&format!("# Could not list Linear teams: {e}\n"));
                }
                Ok(teams) => {
                    for (key, name) in teams {
                        // Guess the workspace by name, then by key; say so when
                        // there is no match rather than inventing one.
                        let ws = workspaces.iter().find(|w| {
                            w.label.eq_ignore_ascii_case(&name)
                                || w.label.eq_ignore_ascii_case(&key)
                        });
                        match ws {
                            Some(w) => {
                                let repo = w
                                    .repo_root
                                    .clone()
                                    .unwrap_or_else(|| "~/code/CHANGE-ME".into());
                                linear_routes.push_str(&format!(
                                    "\n# Linear team {key} ({name})\n[[route]]\nmatch = {{ linear_team = \"{key}\" }}\nworkspace = \"{}\"\nrepo = \"{repo}\"\nruntime = \"claude-code\"\n",
                                    w.label
                                ));
                            }
                            None => {
                                linear_routes.push_str(&format!(
                                    "\n# Linear team {key} ({name}) — no herdr workspace matched by\n# name or key, so fill these in and uncomment.\n# [[route]]\n# match = {{ linear_team = \"{key}\" }}\n# workspace = \"CHANGE-ME\"\n# repo = \"~/code/CHANGE-ME\"\n# runtime = \"claude-code\"\n"
                                ));
                                skipped.push(format!(
                                    "Linear team {key} ({name}) — no workspace matched"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    for w in &workspaces {
        if w.is_linked_worktree {
            continue;
        }
        let Some(root) = w.repo_root.as_deref() else {
            skipped.push(format!("{} (not a git checkout)", w.label));
            continue;
        };
        match git_remote(root).as_deref().and_then(github_slug) {
            Some(slug) => {
                routes.push_str(&format!(
                    "\n[[route]]\nmatch = {{ gh_repo = \"{slug}\" }}\nworkspace = \"{}\"\nrepo = \"{root}\"\nruntime = \"claude-code\"\n",
                    w.label
                ));
                if !repos.contains(&slug) {
                    repos.push(slug);
                }
            }
            None => skipped.push(format!("{} (no GitHub remote)", w.label)),
        }
    }

    let repo_list = repos
        .iter()
        .map(|r| format!("\"{r}\""))
        .collect::<Vec<_>>()
        .join(", ");

    let body = format!(
        r#"# Generated by `herdr-board init` from the herdr workspaces that existed at
# the time. Edit freely — this is a starting point, not managed config.

[sync]
interval = "30s"
# Linear labels that mean "dispatchable". Issues assigned to you are always
# included; these widen that set.
labels = ["herd"]

{linear_routes}{routes}
[defaults]
max_concurrent_per_workspace = 3
branch_template = "board/{{identifier_lower}}"

# Repos polled for issues and pull requests. Needs GITHUB_TOKEN in .env for
# private repos. Remove the label filter to see every open issue.
[github]
repos = [{repo_list}]
labels = []
writeback = true
"#
    );

    std::fs::write(&target, body)?;
    println!("wrote {}", target.display());
    if !repos.is_empty() {
        println!("routed {} GitHub repo(s): {}", repos.len(), repos.join(", "));
    }
    for s in &skipped {
        println!("skipped {s}");
    }
    let mut missing = Vec::new();
    if linear_api_key(paths).is_none() {
        missing.push("LINEAR_API_KEY");
    }
    if !repos.is_empty() && github_token(paths).is_none() {
        missing.push("GITHUB_TOKEN");
    }
    if missing.is_empty() {
        println!("\nnext: herdr-board doctor");
    } else {
        println!(
            "\nnext: add {} to {}",
            missing.join(" and "),
            crate::config::shorten_home(&paths.env_file())
        );
    }
    Ok(())
}

// ---- reading the board -------------------------------------------------

/// One row, as an orchestrating agent sees it.
#[derive(serde::Serialize)]
pub struct TaskRow {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub state: String,
    pub source: String,
    pub url: String,
    pub labels: Vec<String>,
    /// False when no route matches: the task is on the board but cannot be
    /// dispatched, and `dispatch` will refuse it.
    pub dispatchable: bool,
    pub route: Option<String>,
    pub workspace: Option<String>,
    pub runtime: Option<String>,
    pub pane_id: Option<String>,
    /// Set on `review` rows, which is how a PR reaches an orchestrator.
    pub pr_url: Option<String>,
    pub pr_number: Option<i64>,
    pub branch: Option<String>,
    /// Parent task id when an agent released this one; null means the operator.
    pub dispatched_by: Option<String>,
    pub attempts: usize,
}

/// Read the board.
///
/// The counterpart to `dispatch`: an orchestrator needs to see what is ready
/// before it can release anything, and reading `board.db` directly would tie
/// callers to a schema that is ours to change.
pub fn list_tasks(
    paths: &Paths,
    log: Arc<Logger>,
    state: Option<&str>,
    source: Option<&str>,
) -> Result<Vec<TaskRow>> {
    // Reject an unknown filter rather than returning an empty list: to a caller,
    // `[]` from a typo is indistinguishable from `[]` meaning "nothing is
    // ready", and the second is a normal answer worth acting on.
    if let Some(want) = state
        && crate::model::BoardState::parse(want).is_none()
    {
        anyhow::bail!(
            "unknown state `{want}`; expected one of: {}",
            crate::model::BoardState::SECTION_ORDER
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if let Some(want) = source
        && crate::model::Source::parse(want).is_none()
    {
        anyhow::bail!("unknown source `{want}`; expected linear or github");
    }

    let engine = engine_from_paths(paths, log)?;
    let cfg = &engine.cfg;
    let mut rows: Vec<TaskRow> = Vec::new();

    for task in engine.db.load_tasks()? {
        if let Some(want) = state
            && task.state.as_str() != want
        {
            continue;
        }
        if let Some(want) = source
            && task.source.as_str() != want
        {
            continue;
        }
        let route = cfg.resolve(&crate::sync::route_context(&task));
        let live = task.live_attempt();
        let last = task.attempts.last();
        rows.push(TaskRow {
            id: task.id.clone(),
            identifier: task.identifier.clone(),
            title: task.title.clone(),
            state: task.state.as_str().to_string(),
            source: task.source.as_str().to_string(),
            url: task.url.clone(),
            labels: task.labels.clone(),
            dispatchable: route.is_some(),
            route: route.map(|r| r.display_name().to_string()),
            workspace: live
                .or(last)
                .map(|a| a.workspace.clone())
                .or_else(|| route.map(|r| r.workspace.clone())),
            runtime: live
                .or(last)
                .map(|a| a.runtime.clone())
                .or_else(|| route.map(|r| r.runtime.clone())),
            pane_id: live.and_then(|a| a.pane_id.clone()),
            pr_url: task.pr_url.clone(),
            pr_number: task.pr_number,
            branch: live.or(last).and_then(|a| a.branch.clone()),
            dispatched_by: live.or(last).and_then(|a| a.dispatched_by.clone()),
            attempts: task.attempts.len(),
        });
    }

    // Board order, so `list` and the pane agree on what is most urgent.
    rows.sort_by_key(|r| {
        crate::model::BoardState::SECTION_ORDER
            .iter()
            .position(|s| s.as_str() == r.state)
            .unwrap_or(usize::MAX)
    });
    Ok(rows)
}

pub fn print_tasks(rows: &[TaskRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("nothing on the board");
        return Ok(());
    }
    for r in rows {
        let extra = match (&r.pr_url, r.dispatchable) {
            (Some(pr), _) => format!("  {pr}"),
            (None, false) => "  (no route)".to_string(),
            _ => String::new(),
        };
        println!(
            "{:<8} {:<24} {:<10} {}{}",
            r.state,
            r.id,
            r.workspace.as_deref().unwrap_or("-"),
            crate::ui::render::truncate(&r.title, 48),
            extra
        );
    }
    Ok(())
}

// ---- the board toggle --------------------------------------------------

/// Open the board, or close it if it is already up.
///
/// This is what a key binding fires. The board is an overlay, so opening it
/// covers whatever you were doing and closing it hands focus straight back —
/// there is nothing to tidy up afterwards.
pub const BOARD_PANE_LABEL: &str = "Board";

/// `[defaults] split_direction` when it forces a direction, for the board's own
/// pane as well as dispatched ones.
fn engine_split_override(paths: &Paths) -> Option<&'static str> {
    let cfg = RoutingConfig::load_or_default(&paths.routing());
    match cfg.defaults.split_direction.as_deref() {
        Some("right") => Some("right"),
        Some("down") => Some("down"),
        _ => None,
    }
}

/// Every live board pane, newest last.
///
/// Identity is confirmed against the label herdr sets from the manifest title,
/// never from our own recorded id: pane ids are reused, so a stale note can
/// point at a pane that now belongs to something else, and closing that would
/// take out an unrelated pane.
pub fn find_board_panes(herdr: &Herdr) -> Vec<crate::herdr::PaneInfo> {
    herdr
        .pane_list()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.label.as_deref() == Some(BOARD_PANE_LABEL))
        .collect()
}

/// Close a board pane, falling back to the ordinary close for one left over
/// from a previous plugin id (`plugin pane close` only knows panes belonging to
/// a currently registered plugin).
fn close_board_pane(herdr: &Herdr, log: &Logger, id: &str) -> Result<()> {
    herdr.plugin_pane_close(id).or_else(|e| {
        log.info(format!(
            "{id} is not a live plugin pane ({e}); closing it directly"
        ));
        herdr.pane_close(id)
    })
}

/// Toggle the board **in the workspace you are in**.
///
/// There is exactly one board, and it comes to you: toggling in a workspace that
/// does not have it moves it here rather than closing the copy you cannot see.
/// A herdr pane always lives in one tab of one workspace — nothing can span
/// them — so "global" means one board that follows you, not one pane visible
/// everywhere.
pub fn toggle_board(paths: &Paths, log: Arc<Logger>) -> Result<()> {
    let db = Db::open(&paths.db())?;
    let herdr = Herdr::discover(log.clone());
    let here = std::env::var("HERDR_WORKSPACE_ID").unwrap_or_default();

    let panes = find_board_panes(&herdr);
    let in_this_workspace = panes
        .iter()
        .find(|p| here.is_empty() || p.workspace_id == here);

    if let Some(p) = in_this_workspace {
        let id = p.pane_id.clone();
        log.info(format!("closing the board pane {id} in ws:{}", p.workspace_id));
        // A close that fails is not fatal: the pane may have gone between the
        // list and the close, and the right recovery is to open a fresh one.
        match close_board_pane(&herdr, &log, &id) {
            Ok(()) => {
                db.meta_set(crate::ui::app::BOARD_PANE, "")?;
                println!("closed the board");
                return Ok(());
            }
            Err(e) => log.warn(format!("could not close {id}: {e}; opening instead")),
        }
    }

    // The board is somewhere else. Retire it, so there is never more than one,
    // then open here.
    for p in &panes {
        log.info(format!(
            "moving the board out of ws:{} and into ws:{here}",
            p.workspace_id
        ));
        let _ = close_board_pane(&herdr, &log, &p.pane_id);
    }
    db.meta_set(crate::ui::app::BOARD_PANE, "")?;

    // Split to the right of whatever is focused, so the board sits next to your
    // work rather than on top of it — and explicitly in the workspace this
    // toggle was fired from, so placement matches the check above.
    // Same rule as a dispatched agent pane: split right into a quiet tab, down
    // into one that already holds two panes. Three narrow columns is worse than
    // a stacked pane, and the board reads fine at reduced height.
    let (target, direction) = match herdr.split_target_and_direction(&here) {
        Some((t, d)) => (Some(t), d),
        None => (None, "right"),
    };
    let direction = engine_split_override(paths).unwrap_or(direction);
    let pane = herdr.plugin_pane_open_in(
        "board",
        Some("split"),
        Some(direction),
        target.as_deref(),
    )?;
    match pane {
        Some(id) => println!("opened the board beside you ({id})"),
        None => println!("opened the board"),
    }
    Ok(())
}

// ---- link handler ------------------------------------------------------

/// Resolve a clicked URL to a task id.
///
/// Linear issue URLs carry the identifier as a path segment
/// (`.../issue/LIN-142/some-slug`); GitHub issue URLs carry `owner/repo` and a
/// number. Both are matched against what is actually on the board rather than
/// constructed blindly, so a link to something we do not track says so.
pub fn task_id_for_url(db: &Db, url: &str) -> Result<Option<String>> {
    // Deliberately not swallowed: a database that cannot be read is a very
    // different thing from a link that matches nothing, and reporting the second
    // when it is the first sends you looking in the wrong place.
    let tasks = db.load_tasks()?;

    if let Some(rest) = url.split("/issue/").nth(1)
        && let Some(seg) = rest.split(['/', '?', '#']).next()
    {
        let identifier = seg.to_uppercase();
        if let Some(t) = tasks
            .iter()
            .find(|t| t.identifier.eq_ignore_ascii_case(&identifier))
        {
            return Ok(Some(t.id.clone()));
        }
    }

    // https://github.com/owner/repo/issues/87
    if let Some(rest) = url.split("github.com/").nth(1) {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 4 && (parts[2] == "issues" || parts[2] == "pull") {
            let number = parts[3].split(['?', '#']).next().unwrap_or_default();
            let id = format!("gh:{}/{}#{}", parts[0], parts[1], number);
            if tasks.iter().any(|t| t.id == id) {
                return Ok(Some(id));
            }
        }
    }
    Ok(None)
}

/// The `[[link_handlers]]` target: select the clicked issue on the board.
///
/// Lands on the **list** with that row selected — detail would hide the queue
/// the operator came to see.
pub fn open_link(paths: &Paths, log: Arc<Logger>) -> Result<()> {
    let url = std::env::var("HERDR_PLUGIN_CLICKED_URL").unwrap_or_default();
    if url.is_empty() {
        anyhow::bail!("no clicked URL in the environment");
    }
    let db = Db::open(&paths.db())?;
    let herdr = Herdr::discover(log.clone());

    let mut found = task_id_for_url(&db, &url)?;
    if found.is_none() {
        // Sync first: the issue may simply not have been polled yet.
        let _ = sync_once(paths, log.clone());
        found = task_id_for_url(&db, &url)?;
    }
    match found {
        Some(id) => {
            db.meta_set(crate::ui::app::SELECT_TASK, &id)?;
            log.info(format!("link {url} → {id}"));
            println!("selecting {id}");
        }
        None => {
            println!("{url} is not on the board");
            log.warn(format!("link {url} matched no task"));
        }
    }

    // Focus the board if it is already open; otherwise open it.
    if let Some(p) = find_board_panes(&herdr).first()
        && herdr.plugin_pane_focus(&p.pane_id).is_ok()
    {
        return Ok(());
    }
    herdr.plugin_pane_open_as("board", Some("split"), Some("right"))?;
    Ok(())
}

// ---- doctor ------------------------------------------------------------

pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Check the environment: keys present, herdr reachable, db writable, routes
/// valid. Plain stdout — it runs outside a pane.
pub fn doctor(paths: &Paths) -> Result<Vec<Check>> {
    let mut checks = Vec::new();
    // Silent: doctor's output *is* the report, and argv logging on stderr
    // buries it.
    let log = Arc::new(Logger::new("", false));

    checks.push(Check {
        name: "config dir".into(),
        ok: paths.config_dir.exists(),
        detail: paths.config_dir.display().to_string(),
    });

    let db_ok = Db::open(&paths.db());
    checks.push(Check {
        name: "database".into(),
        ok: db_ok.is_ok(),
        detail: match &db_ok {
            Ok(_) => format!("{} is writable", paths.db().display()),
            Err(e) => format!("{e:#}"),
        },
    });

    checks.push(Check {
        name: "LINEAR_API_KEY".into(),
        ok: linear_api_key(paths).is_some(),
        detail: match linear_api_key(paths) {
            Some(_) => "present".into(),
            None => format!("missing — add it to {}", paths.env_file().display()),
        },
    });


    let herdr = Herdr::discover(log);
    let version = herdr.version();
    checks.push(Check {
        name: "herdr".into(),
        ok: version.is_ok(),
        detail: match &version {
            Ok(v) => format!("{v} via {}", herdr.bin().display()),
            Err(e) => format!("not reachable: {e}"),
        },
    });

    // Routing is where most misconfiguration lives, so it is checked in detail.
    // Parsing is deliberately separated from validation: a single bad runtime
    // must not hide the problems in every other route.
    match RoutingConfig::load_unvalidated(&paths.routing()) {
        Err(e) => checks.push(Check {
            // `{:#}` so the cause chain shows: "reading X: No such file" is
            // actionable, "reading X" alone is not.
            name: "routing.toml".into(),
            ok: false,
            detail: format!("{e:#}"),
        }),
        Ok(cfg) => {
            let valid = cfg.check();
            checks.push(Check {
                name: "routing.toml".into(),
                ok: valid.is_ok(),
                detail: match &valid {
                    Ok(()) => format!("{} route(s)", cfg.routes.len()),
                    Err(e) => format!("{e:#}"),
                },
            });
            // The token is only optional until repos are configured: GitHub
            // answers 404 (not 401) for a private repo you cannot see, so
            // without this check the first symptom is a mystery 404 in the log.
            let repos = &cfg.github.repos;
            checks.push(Check {
                name: "GITHUB_TOKEN".into(),
                ok: repos.is_empty() || github_token(paths).is_some(),
                detail: match (github_token(paths).is_some(), repos.is_empty()) {
                    (true, _) => "present".into(),
                    (false, true) => "not needed — no repos under [github]".into(),
                    (false, false) => format!(
                        "missing, but {} repo(s) are configured — private repos \
                         answer 404 without it. Add it to {}",
                        repos.len(),
                        paths.env_file().display()
                    ),
                },
            });

            for repo in repos {
                let reachable = crate::sources::github::HttpRest::new(github_token(paths))
                    .ok()
                    .map(|r| {
                        use crate::sources::github::Rest;
                        r.get(&format!("/repos/{repo}"))
                    });
                let (ok, detail) = match reachable {
                    Some(Ok(_)) => (true, "reachable".to_string()),
                    Some(Err(e)) if e.to_string().contains("404") => (
                        false,
                        "404 — either it does not exist, or the token cannot see it"
                            .to_string(),
                    ),
                    Some(Err(e)) => (false, format!("{e}")),
                    None => (false, "could not build an HTTP client".to_string()),
                };
                checks.push(Check {
                    name: format!("github {repo}"),
                    ok,
                    detail,
                });
            }

            let workspaces = herdr.workspace_list().unwrap_or_default();
            for r in &cfg.routes {
                let name = r.display_name().to_string();

                let ws_ok = workspaces
                    .iter()
                    .any(|w| w.label.eq_ignore_ascii_case(&r.workspace));
                checks.push(Check {
                    name: format!("route {name}: workspace"),
                    ok: ws_ok,
                    detail: if ws_ok {
                        format!("`{}` exists", r.workspace)
                    } else {
                        format!(
                            "no herdr workspace labelled `{}` (have: {})",
                            r.workspace,
                            workspaces
                                .iter()
                                .map(|w| w.label.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    },
                });

                let repo = r.repo_path();
                let repo_ok = repo.join(".git").exists();
                checks.push(Check {
                    name: format!("route {name}: repo"),
                    ok: repo_ok,
                    detail: if repo_ok {
                        repo.display().to_string()
                    } else {
                        format!("{} is not a git repo", repo.display())
                    },
                });

                let kind = herdr_kind_for_runtime(&r.runtime);
                checks.push(Check {
                    name: format!("route {name}: runtime"),
                    ok: kind.is_some(),
                    detail: match kind {
                        Some(k) if k == r.runtime => format!("`{k}`"),
                        Some(k) => format!("`{}` → herdr kind `{k}`", r.runtime),
                        None => format!("`{}` is not a herdr agent kind", r.runtime),
                    },
                });
            }
        }
    }

    let daemon = running_pid(paths);
    checks.push(Check {
        name: "syncd".into(),
        ok: daemon.is_some(),
        detail: match daemon {
            Some(p) => format!("running as pid {p}"),
            None => "not running — it starts with herdr, or run `herdr-board syncd --ensure`"
                .into(),
        },
    });

    Ok(checks)
}

pub fn print_doctor(checks: &[Check]) -> bool {
    let mut all_ok = true;
    for c in checks {
        if !c.ok {
            all_ok = false;
        }
        println!("{} {:<26} {}", if c.ok { "ok  " } else { "FAIL" }, c.name, c.detail);
    }
    all_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::UpsertTask;
    use crate::model::{Source, UpstreamState};

    fn tmp() -> Paths {
        let d = std::env::temp_dir().join(format!(
            "hb-cli-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        Paths {
            config_dir: d.clone(),
            state_dir: d,
        }
    }

    fn seeded_db() -> Db {
        let db = Db::open_in_memory().unwrap();
        for (id, ident) in [
            ("linear:LIN-142", "LIN-142"),
            ("gh:offhand/tally#87", "gh#87"),
        ] {
            db.upsert_task(&UpsertTask {
                id: id.into(),
                source: if id.starts_with("gh:") {
                    Source::Github
                } else {
                    Source::Linear
                },
                source_id: "u".into(),
                identifier: ident.into(),
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
        }
        db
    }

    #[test]
    fn a_linear_url_resolves_to_its_row() {
        let db = seeded_db();
        assert_eq!(
            task_id_for_url(&db, "https://linear.app/offhand/issue/LIN-142/add-retry")
                .unwrap()
                .as_deref(),
            Some("linear:LIN-142")
        );
        // Without the trailing slug too.
        assert_eq!(
            task_id_for_url(&db, "https://linear.app/offhand/issue/LIN-142")
                .unwrap()
                .as_deref(),
            Some("linear:LIN-142")
        );
    }

    #[test]
    fn a_github_issue_url_resolves_to_its_row() {
        let db = seeded_db();
        assert_eq!(
            task_id_for_url(&db, "https://github.com/offhand/tally/issues/87")
                .unwrap()
                .as_deref(),
            Some("gh:offhand/tally#87")
        );
    }

    #[test]
    fn a_url_we_do_not_track_resolves_to_nothing() {
        let db = seeded_db();
        // Right shape, wrong issue: better to say so than to invent a row.
        assert_eq!(
            task_id_for_url(&db, "https://linear.app/offhand/issue/LIN-999").unwrap(),
            None
        );
        assert_eq!(
            task_id_for_url(&db, "https://example.com/nope").unwrap(),
            None
        );
    }

    #[test]
    fn github_slugs_parse_from_both_remote_forms() {
        assert_eq!(
            github_slug("git@github.com:offhand/tally.git").as_deref(),
            Some("offhand/tally")
        );
        assert_eq!(
            github_slug("https://github.com/offhand/tally").as_deref(),
            Some("offhand/tally")
        );
        assert_eq!(
            github_slug("ssh://git@github.com/offhand/tally.git").as_deref(),
            Some("offhand/tally")
        );
        // Not GitHub, so not a GitHub source.
        assert_eq!(github_slug("git@gitlab.com:offhand/tally.git"), None);
        assert_eq!(github_slug(""), None);
    }

    #[test]
    fn a_missing_pidfile_means_no_daemon() {
        assert_eq!(running_pid(&tmp()), None);
    }

    #[test]
    fn a_stale_pid_is_not_mistaken_for_a_live_daemon() {
        let p = tmp();
        // A pid that cannot be running.
        std::fs::write(p.pidfile(), "999999 Mon Jan  1 00:00:00 2001").unwrap();
        assert_eq!(running_pid(&p), None);
    }

    #[test]
    fn a_recycled_pid_is_rejected_by_its_start_time() {
        let p = tmp();
        // Our own pid, but recorded with someone else's start time.
        std::fs::write(
            p.pidfile(),
            format!("{} Mon Jan  1 00:00:00 2001", std::process::id()),
        )
        .unwrap();
        assert_eq!(running_pid(&p), None);
    }

    #[test]
    fn our_own_live_pid_is_recognised() {
        let p = tmp();
        write_pidfile(&p, std::process::id()).unwrap();
        assert_eq!(running_pid(&p), Some(std::process::id()));
    }

    #[test]
    fn doctor_reports_a_missing_routing_file_without_panicking() {
        let p = tmp();
        let checks = doctor(&p).unwrap();
        assert!(checks.iter().any(|c| c.name == "routing.toml" && !c.ok));
        // The database check must still pass — doctor creates it.
        assert!(checks.iter().any(|c| c.name == "database" && c.ok));
    }

    #[test]
    fn edited_and_removed_credentials_rebuild_source_clients() {
        let paths = tmp();
        let cfg = RoutingConfig::load_or_default(&paths.routing());
        let log = Arc::new(Logger::new("", false));
        let engine = build_engine_with_credentials(
            Db::open(&paths.db()).unwrap(),
            cfg,
            paths.clone(),
            log.clone(),
            Credentials {
                linear_api_key: Some("first".into()),
                github_token: None,
            },
        )
        .unwrap();

        let edited = rebuild_if_configuration_changed(
            &paths,
            &engine,
            &log,
            Credentials {
                linear_api_key: Some("second".into()),
                github_token: None,
            },
            RoutingConfig::load_or_default(&paths.routing()),
        )
        .expect("an edited key should rebuild the engine");
        assert_eq!(
            edited.credentials.linear_api_key.as_deref(),
            Some("second")
        );
        assert!(edited.linear.is_some());

        let removed = rebuild_if_configuration_changed(
            &paths,
            &edited,
            &log,
            Credentials::default(),
            RoutingConfig::load_or_default(&paths.routing()),
        )
        .expect("a removed key should rebuild the engine");
        assert_eq!(removed.credentials.linear_api_key, None);
        assert!(removed.linear.is_none());
    }
}
