//! `gc`: prune the worktrees of attempts that are finished for good (impl spec
//! §6).
//!
//! Nothing removes a worktree during normal operation, and that is deliberate:
//! git allows a branch in only one worktree, so a retry cannot cut a second
//! checkout of the same branch and instead reuses the one the previous attempt
//! left behind. The cost is one checkout per attempt, and it never shrinks.
//!
//! What makes clearing them safe is the spec's pair of conditions: **terminal
//! and aged**. Terminal is a property of the *task*, not of the attempt — a
//! `cancelled` attempt returns its row to `ready`, so its checkout is exactly
//! the one a retry is going to reuse, and the same is true of `failed` and
//! `review`. Only a task that has reached `done` has no retry left to strand.
//! Age is the second guard: a task closed an hour ago is still one you might be
//! looking at.
//!
//! The branch is never touched. Removing the checkout frees it for a fresh one;
//! deleting it would throw the work away.
//!
//! # Whose checkouts these are
//!
//! Dispatch does not cut worktrees any more — `herdr worktree create` does, and
//! herdr keeps them under its own `[worktrees] directory` as workspaces grouped
//! with the parent repo. So there is no directory for gc to walk: it asks herdr
//! what checkouts a repository has, which reports real paths and covers both
//! herdr's directory and the `$STATE/wt/` an older board used.
//!
//! That leaves two ways a checkout comes back, and gc has to do both:
//!
//! * **herdr still holds it open as a workspace** — `herdr worktree remove`
//!   runs `git worktree remove` *and* closes the workspace. Doing it with git
//!   alone would leave herdr showing a workspace whose directory is gone.
//! * **Nothing holds it** — the common case, because a workspace whose last
//!   pane closes is dropped: cancelling an attempt takes the workspace with it
//!   and leaves the checkout on disk, still holding its branch. There is no
//!   workspace left to name, so git removes it and prunes the stale entry.

use crate::config::{Paths, RoutingConfig, shorten_home};
use crate::db::Db;
use crate::herdr::{Herdr, WorktreeInfo};
use crate::log::Logger;
use crate::model::{BoardState, Task};
use crate::sync::derivation_for;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// What herdr knows about a repository's checkouts, and how to hand one back.
///
/// Behind a trait so the decisions below are testable without a running herdr —
/// and so that a herdr which is not answering degrades to plain git rather than
/// collecting nothing.
pub trait Worktrees {
    fn list(&self, repo: &Path) -> Result<Vec<WorktreeInfo>>;
    fn remove(&self, workspace_id: &str) -> Result<()>;
}

impl Worktrees for Herdr {
    fn list(&self, repo: &Path) -> Result<Vec<WorktreeInfo>> {
        self.worktree_list(repo)
    }
    fn remove(&self, workspace_id: &str) -> Result<()> {
        self.worktree_remove(workspace_id)
    }
}

/// The spec's default: two weeks is long enough that a closed task is not one
/// you are still looking at.
pub const DEFAULT_OLDER_THAN: &str = "14d";

/// Why a worktree was left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keep {
    /// An attempt in this checkout is still open — an agent may be in it.
    Live,
    /// The task has not finished, so a retry would reuse this checkout.
    Retryable(BoardState),
    /// Terminal, but not yet past the cutoff.
    TooYoung,
}

impl Keep {
    pub fn reason(&self) -> String {
        match self {
            Keep::Live => "a live attempt is in it".into(),
            Keep::Retryable(s) => format!("{s} — a retry would reuse it"),
            Keep::TooYoung => "not old enough yet".into(),
        }
    }
}

/// One worktree path, with everything the decision was made from.
///
/// A path, not an attempt: retries share a checkout, so several attempts — in
/// principle across several tasks — can name the same directory, and it is only
/// collectable when *all* of them are done with it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub worktree: PathBuf,
    /// Identifiers of the tasks whose attempts point here, first seen first.
    pub identifiers: Vec<String>,
    pub branch: Option<String>,
    pub attempts: usize,
    /// Seconds since the newest attempt here ended. Zero when no timestamp
    /// could be read — which keeps the entry too young to collect.
    pub age_secs: i64,
    /// `None` means collectable.
    pub keep: Option<Keep>,
    /// The herdr workspace holding this checkout, when herdr has one open.
    ///
    /// Filled in from herdr, not from the attempt row: the attempt records the
    /// *route's* workspace name, and the workspace herdr opened for the
    /// checkout is a different thing that outlives neither the attempt nor,
    /// often, the agent — closing its last pane drops it.
    pub herdr_workspace: Option<String>,
}

impl Entry {
    pub fn collectable(&self) -> bool {
        self.keep.is_none()
    }
}

/// A checkout herdr reports that no attempt on the board claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Untracked {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// What one `gc` run did, or would do under `--dry-run`.
#[derive(Debug, Default)]
pub struct Report {
    pub dry_run: bool,
    pub older_than_secs: i64,
    /// The longest directory prefix every reported checkout shares, named once
    /// so a row can be a short name instead of forty identical characters.
    ///
    /// Computed rather than fixed: the checkouts are herdr's, it puts them
    /// under a `[worktrees] directory` the operator can configure, and a board
    /// that has been running a while has them under both that and the
    /// `$STATE/wt/` an older version used.
    pub root: PathBuf,
    /// Removed, or — under `--dry-run` — what removal would be attempted on.
    pub collected: Vec<Entry>,
    /// Recorded on an attempt, but already off disk. Only git's administrative
    /// entry is left, and pruning takes care of that.
    pub gone: Vec<Entry>,
    /// Collectable, but the removal was refused. Usually uncommitted work.
    pub skipped: Vec<(Entry, String)>,
    pub kept: Vec<Entry>,
    /// Checkouts herdr reports that no attempt claims — left alone, because
    /// nothing on the board can say whether they are still wanted.
    pub untracked: Vec<Untracked>,
    /// Repositories whose stale worktree entries were pruned.
    pub pruned: Vec<PathBuf>,
}

impl Report {
    fn counts(&self) -> (usize, usize, usize) {
        let live = self.kept.iter().filter(|e| e.keep == Some(Keep::Live)).count();
        let young = self
            .kept
            .iter()
            .filter(|e| e.keep == Some(Keep::TooYoung))
            .count();
        (live, self.kept.len() - live - young, young)
    }
}

/// Decide what is collectable, from the database alone.
///
/// Pure: no filesystem, no git, no clock — `now` and the cutoff are handed in so
/// the whole matrix is testable, the same way `derive_state` is.
pub fn plan(tasks: &[Task], now: DateTime<Utc>, older_than_secs: i64) -> Vec<Entry> {
    struct Acc {
        identifiers: Vec<String>,
        branch: Option<String>,
        attempts: usize,
        live: bool,
        retryable: Option<BoardState>,
        newest_end: Option<DateTime<Utc>>,
        /// A timestamp we could not read. Treated as "recent", never as old.
        unknown_age: bool,
    }

    let mut by_path: BTreeMap<PathBuf, Acc> = BTreeMap::new();

    for task in tasks {
        // The stored `state` column is only as fresh as the last daemon tick,
        // and gc removes directories — so it derives rather than trusting it.
        let state = crate::model::derive_state(derivation_for(task, &HashMap::new()));
        for attempt in &task.attempts {
            let Some(path) = attempt.worktree.as_deref().filter(|p| !p.is_empty()) else {
                continue;
            };
            let acc = by_path.entry(PathBuf::from(path)).or_insert_with(|| Acc {
                identifiers: Vec::new(),
                branch: None,
                attempts: 0,
                live: false,
                retryable: None,
                newest_end: None,
                unknown_age: false,
            });
            acc.attempts += 1;
            if !acc.identifiers.contains(&task.identifier) {
                acc.identifiers.push(task.identifier.clone());
            }
            if acc.branch.is_none() {
                acc.branch = attempt.branch.clone();
            }
            if attempt.outcome.is_none() {
                acc.live = true;
            }
            if !state.is_terminal() && acc.retryable.is_none() {
                acc.retryable = Some(state);
            }
            // An attempt with no `ended_at` is either live (already caught
            // above) or a row from a crash mid-close; either way its age is not
            // knowable from `started_at` alone.
            match attempt
                .ended_at
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            {
                Some(t) => {
                    let t = t.with_timezone(&Utc);
                    if acc.newest_end.is_none_or(|cur| t > cur) {
                        acc.newest_end = Some(t);
                    }
                }
                None if attempt.outcome.is_some() => acc.unknown_age = true,
                None => {}
            }
        }
    }

    let mut entries: Vec<Entry> = by_path
        .into_iter()
        .map(|(worktree, a)| {
            let age_secs = a
                .newest_end
                .map(|t| (now - t).num_seconds().max(0))
                .unwrap_or(0);
            let keep = if a.live {
                Some(Keep::Live)
            } else if let Some(s) = a.retryable {
                Some(Keep::Retryable(s))
            } else if a.unknown_age || age_secs < older_than_secs {
                Some(Keep::TooYoung)
            } else {
                None
            };
            Entry {
                worktree,
                identifiers: a.identifiers,
                branch: a.branch,
                attempts: a.attempts,
                age_secs,
                keep,
                // Only herdr can answer this, and `plan` is deliberately pure.
                herdr_workspace: None,
            }
        })
        .collect();
    // Collectable first, then by path: the lines that matter are at the top.
    entries.sort_by(|a, b| {
        a.keep
            .is_some()
            .cmp(&b.keep.is_some())
            .then_with(|| a.worktree.cmp(&b.worktree))
    });
    entries
}

/// `14d`, `36h`, `2w`. A bare number is refused rather than guessed at: the same
/// string means seconds to `[sync] interval`, and silently reading `14` as
/// fourteen seconds here would delete everything.
pub fn parse_age(s: &str) -> Result<i64> {
    let t = s.trim();
    if t.chars().next_back().is_some_and(|c| c.is_ascii_digit()) {
        bail!("--older-than needs a unit: 14d, 36h, 2w");
    }
    crate::config::parse_duration_secs(t)
        .map(|n| n as i64)
        .ok_or_else(|| anyhow::anyhow!("could not read `{s}` as an age; try 14d, 36h or 2w"))
}

/// Run a garbage collection.
pub fn run(
    paths: &Paths,
    cfg: &RoutingConfig,
    herdr: &dyn Worktrees,
    log: Arc<Logger>,
    older_than: &str,
    dry_run: bool,
) -> Result<Report> {
    let older_than_secs = parse_age(older_than)?;
    let db = Db::open(&paths.db())?;
    let tasks = db.load_tasks()?;
    let mut entries = plan(&tasks, Utc::now(), older_than_secs);

    let census = Census::take(herdr, &log, &repos_to_ask(cfg, &entries));
    for e in &mut entries {
        e.herdr_workspace = census.workspace_for(&e.worktree);
    }

    let mut report = Report {
        dry_run,
        older_than_secs,
        untracked: census.unclaimed(&entries),
        ..Default::default()
    };

    // Repositories a removal touched. git prunes its own entry on a successful
    // `worktree remove`, so this is for the rest — above all a checkout deleted
    // by hand, whose leftover entry still holds the branch against a retry.
    let mut touched: Vec<PathBuf> = Vec::new();
    let mut touch = |repo: Option<PathBuf>| {
        if let Some(r) = repo
            && !touched.contains(&r)
        {
            touched.push(r);
        }
    };

    for mut entry in entries {
        if entry.keep.is_some() {
            report.kept.push(entry);
            continue;
        }
        if !entry.worktree.exists() {
            touch(census.repo_for(&entry.worktree));
            report.gone.push(entry);
            continue;
        }
        if dry_run {
            report.collected.push(entry);
            continue;
        }
        match collect(herdr, &log, &mut entry) {
            Ok(()) => {
                log.info(format!(
                    "gc removed {} ({}); branch {} left in place",
                    entry.worktree.display(),
                    entry.identifiers.join(", "),
                    entry.branch.as_deref().unwrap_or("—"),
                ));
                touch(
                    census
                        .repo_for(&entry.worktree)
                        .or_else(|| main_repo_for(&entry.worktree).ok()),
                );
                report.collected.push(entry);
            }
            Err(e) => {
                log.warn(format!("gc left {}: {e}", entry.worktree.display()));
                report.skipped.push((entry, e.to_string()));
            }
        }
    }

    if !dry_run {
        for repo in touched {
            match prune(&repo) {
                Ok(()) => report.pruned.push(repo),
                Err(e) => log.warn(format!("gc could not prune {}: {e}", repo.display())),
            }
        }
    }
    report.root = common_root(&report);
    Ok(report)
}

/// Hand one checkout back, by whichever route owns it.
///
/// On success `entry.herdr_workspace` describes what actually happened, so the
/// report cannot claim a workspace was closed when it was already gone.
fn collect(herdr: &dyn Worktrees, log: &Logger, entry: &mut Entry) -> Result<()> {
    let Some(ws) = entry.herdr_workspace.clone() else {
        return remove_worktree(&entry.worktree);
    };
    match herdr.remove(&ws) {
        Ok(()) => Ok(()),
        // The workspace went away between the listing and now — closing its
        // last pane is all it takes. The checkout is still there, so this is
        // the ordinary orphan case arriving a moment late, not a failure.
        Err(e) if e.to_string().contains("workspace_not_found") => {
            log.info(format!(
                "workspace {ws} is already gone; removing {} with git",
                entry.worktree.display()
            ));
            entry.herdr_workspace = None;
            remove_worktree(&entry.worktree)
        }
        Err(e) => Err(e),
    }
}

/// Remove one checkout with git, leaving its branch alone.
///
/// The repository is asked of the worktree itself rather than looked up in
/// `routing.toml`: a route can be re-pointed at another repo long after an
/// attempt ran, and the checkout always knows where it came from.
pub fn remove_worktree(worktree: &Path) -> Result<()> {
    let repo = main_repo_for(worktree)?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "remove"])
        .arg(worktree)
        .output()
        .context("running git worktree remove")?;
    if !out.status.success() {
        // git's own message is the useful one — it names uncommitted work
        // explicitly — but it is written to stand alone: a `fatal:` prefix and
        // the full path, both of which the report has already said.
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim().trim_start_matches("fatal: ").replace('\n', "; ");
        let quoted = format!("'{}' ", worktree.display());
        bail!("{}", err.strip_prefix(&quoted).unwrap_or(&err));
    }
    Ok(())
}

/// Drop git's administrative entries for checkouts that are no longer on disk.
///
/// Idempotent, and it never touches a checkout that is still there — which is
/// what makes it safe to run over every repository a collection touched.
fn prune(repo: &Path) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output()
        .context("running git worktree prune")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// The main repository a linked worktree belongs to.
fn main_repo_for(worktree: &Path) -> Result<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .context("running git rev-parse")?;
    if !out.status.success() {
        bail!(
            "not a git worktree: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    // `/repo/.git` → `/repo`. A bare repository has no working tree to strip.
    match (common.file_name(), common.parent()) {
        (Some(name), Some(parent)) if name == ".git" => Ok(parent.to_path_buf()),
        _ => Ok(common),
    }
}

/// Every checkout herdr knows of, across the repositories the board works in.
///
/// This is what replaced walking a directory. Dispatch hands the checkout to
/// herdr, so herdr is the only thing that knows where it went — and it reports
/// the real path whether that is under its own `[worktrees] directory` or the
/// `$STATE/wt/` an older board used.
#[derive(Debug, Default)]
struct Census {
    /// Checkout path → what herdr said about it.
    seen: BTreeMap<PathBuf, Held>,
}

#[derive(Debug)]
struct Held {
    repo: PathBuf,
    info: WorktreeInfo,
    /// Its branch is one the board would have cut, so an unclaimed checkout
    /// here is a leak rather than somebody's own work.
    ours: bool,
}

/// A repository to ask about, and how to recognise the board's own checkouts
/// in it.
#[derive(Debug, Clone)]
struct Repo {
    path: PathBuf,
    /// Literal heads of the `branch_template`s that route here — `board/` for
    /// the default `board/{identifier_lower}`. Empty means the templates say
    /// nothing recognisable, and every checkout in the repo is treated as ours.
    branch_prefixes: Vec<String>,
}

impl Census {
    /// Ask each repository in turn. A repository that does not answer — herdr
    /// down, or a path that is no longer a git checkout — is skipped rather
    /// than fatal: gc then falls back to git for everything in it, which is
    /// exactly the right behaviour for an orphaned checkout anyway.
    fn take(herdr: &dyn Worktrees, log: &Logger, repos: &[Repo]) -> Census {
        let mut census = Census::default();
        for repo in repos {
            let worktrees = match herdr.list(&repo.path) {
                Ok(w) => w,
                Err(e) => {
                    log.warn(format!(
                        "gc could not ask herdr about {}: {e}",
                        repo.path.display()
                    ));
                    continue;
                }
            };
            for info in worktrees {
                // The repository's own working tree is not a checkout to
                // collect, and must never be reported as untracked.
                if !info.is_linked_worktree {
                    continue;
                }
                let ours = repo.branch_prefixes.is_empty()
                    || info.branch.as_deref().is_some_and(|b| {
                        repo.branch_prefixes.iter().any(|p| b.starts_with(p.as_str()))
                    });
                census.seen.insert(
                    info.path.clone(),
                    Held {
                        repo: repo.path.clone(),
                        info,
                        ours,
                    },
                );
            }
        }
        census
    }

    fn workspace_for(&self, path: &Path) -> Option<String> {
        self.seen.get(path)?.info.open_workspace_id.clone()
    }

    fn repo_for(&self, path: &Path) -> Option<PathBuf> {
        self.seen.get(path).map(|h| h.repo.clone())
    }

    /// Checkouts on a branch the board cuts that no attempt claims.
    ///
    /// They are reported, never removed: the board cannot say whether they are
    /// still wanted, so the operator decides.
    ///
    /// Narrowed by branch, because a repository the board works in is also a
    /// repository *you* work in. herdr lists every checkout of it — your own
    /// worktrees, and the ones other agents cut under `.claude/worktrees/` —
    /// and reporting all of them buries the one line that matters under a dozen
    /// that never change. The cost is a checkout dispatched with an overridden
    /// branch, which is only ever unclaimed if the database was thrown away.
    fn unclaimed(&self, entries: &[Entry]) -> Vec<Untracked> {
        let known: HashSet<&Path> = entries.iter().map(|e| e.worktree.as_path()).collect();
        self.seen
            .iter()
            .filter(|(path, held)| held.ours && !known.contains(path.as_path()))
            .map(|(path, held)| Untracked {
                path: path.clone(),
                branch: held.info.branch.clone(),
            })
            .collect()
    }
}

/// The repositories to ask about.
///
/// The routes are the definition of where the board works, and they are what
/// makes discovering an *unclaimed* checkout possible at all. The repository
/// behind each known checkout is added too, so a route that has since been
/// re-pointed — or removed — does not hide the checkouts it left behind.
fn repos_to_ask(cfg: &RoutingConfig, entries: &[Entry]) -> Vec<Repo> {
    let mut repos: Vec<Repo> = Vec::new();
    let mut add = |path: PathBuf, prefix: Option<String>| {
        let repo = match repos.iter_mut().find(|r| r.path == path) {
            Some(r) => r,
            None => {
                repos.push(Repo {
                    path,
                    branch_prefixes: Vec::new(),
                });
                repos.last_mut().expect("just pushed")
            }
        };
        // Several routes can share a repository, each with its own template,
        // and a checkout on any of their branches is the board's.
        if let Some(p) = prefix
            && !repo.branch_prefixes.contains(&p)
        {
            repo.branch_prefixes.push(p);
        }
    };

    for route in &cfg.routes {
        let path = route.repo_path();
        if path.is_dir() {
            add(path, branch_prefix(cfg.branch_template(route)));
        }
    }
    for entry in entries {
        // A repository reached only through a checkout has no route to name a
        // template, so the default is the best guess at what the board cut.
        if entry.worktree.is_dir()
            && let Ok(path) = main_repo_for(&entry.worktree)
        {
            add(path, branch_prefix(&cfg.defaults.branch_template));
        }
    }
    repos
}

/// The literal head of a branch template: `board/` for
/// `board/{identifier_lower}`.
///
/// `None` for a template that opens with a placeholder — there is nothing to
/// recognise a checkout by, and guessing would be worse than not filtering.
fn branch_prefix(template: &str) -> Option<String> {
    let head = template.split('{').next().unwrap_or_default();
    (!head.is_empty()).then(|| head.to_string())
}

/// The longest directory prefix every reported checkout shares.
///
/// An empty result means there is nothing to shorten against — one path from
/// each of two unrelated roots, say — and the rows fall back to full paths.
fn common_root(report: &Report) -> PathBuf {
    let all = report
        .collected
        .iter()
        .chain(&report.gone)
        .chain(&report.kept)
        .chain(report.skipped.iter().map(|(e, _)| e))
        .map(|e| e.worktree.as_path())
        .chain(report.untracked.iter().map(|u| u.path.as_path()));

    let mut root: Option<PathBuf> = None;
    for path in all {
        let Some(parent) = path.parent() else {
            return PathBuf::new();
        };
        match &mut root {
            None => root = Some(parent.to_path_buf()),
            Some(root) => {
                while !parent.starts_with(&root) {
                    if !root.pop() {
                        return PathBuf::new();
                    }
                }
            }
        }
    }
    root.unwrap_or_default()
}

// ---- reporting ---------------------------------------------------------

pub fn print_report(report: &Report) {
    if report.collected.is_empty()
        && report.kept.is_empty()
        && report.skipped.is_empty()
        && report.untracked.is_empty()
    {
        println!("no checkouts to collect");
        return;
    }
    if report.root.as_os_str().is_empty() {
        println!("checkouts across several roots");
    } else {
        println!("{}", shorten_home(&report.root));
    }

    let verb = if report.dry_run { "would go" } else { "removed" };
    for e in &report.collected {
        let branch = format!(
            "branch {} left in place",
            e.branch.as_deref().unwrap_or("(none)")
        );
        // Which route it went by is worth saying: a workspace that is still
        // open is the case where git alone would have left herdr showing a
        // space with no directory behind it.
        let detail = match &e.herdr_workspace {
            Some(ws) if report.dry_run => format!("{branch}; workspace {ws} closes with it"),
            Some(ws) => format!("{branch}; workspace {ws} closed"),
            None => branch,
        };
        println!("{}", row(verb, report, e, &detail));
    }
    for (e, why) in &report.skipped {
        println!("{}", row("left", report, e, why));
    }
    // Under `--dry-run` the question being asked is "what would you do, and why
    // not to the rest" — so the kept rows carry their reason. A real run has
    // already answered it.
    if report.dry_run {
        for e in &report.kept {
            let why = e.keep.as_ref().map(Keep::reason).unwrap_or_default();
            println!("{}", row("kept", report, e, &why));
        }
    }
    for u in &report.untracked {
        // The same columns as `row`, with the task and age left blank: neither
        // is knowable for a checkout no attempt claims. The branch is, and it
        // is usually enough to recognise what the checkout was for.
        println!(
            "untracked  {:<22}  {:<10} {:>4}  no attempt on the board claims it (on {})",
            name_in(&report.root, &u.path),
            "",
            "",
            u.branch.as_deref().unwrap_or("no branch"),
        );
    }

    let (live, retryable, young) = report.counts();
    println!();
    println!(
        "{} {}, {} kept ({live} live, {retryable} still retryable, {young} younger \
         than {}).",
        report.collected.len(),
        if report.dry_run {
            "would be removed"
        } else {
            "removed"
        },
        report.kept.len(),
        human_age(report.older_than_secs),
    );
    if !report.collected.is_empty() && !report.dry_run {
        println!("Branches are untouched: a retry cuts a fresh checkout from the same branch.");
    }
    if !report.pruned.is_empty() {
        println!(
            "Pruned stale worktree entries in {}.",
            report
                .pruned
                .iter()
                .map(|p| shorten_home(p))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !report.skipped.is_empty() {
        // The forcing command differs by which route the checkout was on, and
        // the wrong one either fails or leaves herdr holding a dead workspace.
        let held: Vec<&str> = report
            .skipped
            .iter()
            .filter_map(|(e, _)| e.herdr_workspace.as_deref())
            .collect();
        println!(
            "{} could not be removed. `git worktree remove --force <path>` if the \
             changes in them are not wanted.",
            report.skipped.len()
        );
        if !held.is_empty() {
            println!(
                "{} of them is herdr's: `herdr worktree remove --workspace {} --force`, \
                 so the workspace closes with it.",
                if held.len() == 1 { "One" } else { "Some" },
                held.join("` / `--workspace "),
            );
        }
    }
}

fn row(verb: &str, report: &Report, e: &Entry, detail: &str) -> String {
    format!(
        "{verb:<9}  {:<22}  {:<10} {:>4}  {detail}",
        name_in(&report.root, &e.worktree),
        e.identifiers.join(","),
        human_age(e.age_secs),
    )
}

/// A checkout's name within the report's common root, falling back to the full
/// path when it sits outside — which is what an empty root means.
fn name_in(root: &Path, worktree: &Path) -> String {
    match worktree.strip_prefix(root) {
        Ok(rest) => rest.to_string_lossy().into_owned(),
        Err(_) => shorten_home(worktree),
    }
}

/// Coarse on purpose: `gc` deals in days, and `13d` reads better than `1123200s`.
pub fn human_age(secs: i64) -> String {
    match secs {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3_600 => format!("{}h", s / 3_600),
        s if s > 0 => format!("{}m", s / 60),
        _ => "0m".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{NewAttempt, UpsertTask, rfc3339};
    use crate::model::{Outcome, Source, UpstreamState};
    use chrono::Duration;

    const DAY: i64 = 86_400;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn now() -> DateTime<Utc> {
        // A fixed clock, so `age_secs` in the assertions is exact.
        DateTime::parse_from_rfc3339("2026-07-26T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn seed(db: &Db, id: &str, upstream: UpstreamState) {
        db.upsert_task(&UpsertTask {
            id: id.into(),
            source: Source::Linear,
            source_id: "u".into(),
            identifier: id.trim_start_matches("linear:").into(),
            title: "t".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream,
            updated_at: crate::db::now(),
        })
        .unwrap();
    }

    /// An attempt on `task` in `worktree`, closed `days_ago` with `outcome`.
    /// `outcome: None` leaves it live.
    fn attempt(
        db: &Db,
        task: &str,
        worktree: &str,
        outcome: Option<Outcome>,
        days_ago: i64,
    ) -> i64 {
        let id = db
            .insert_attempt(&NewAttempt {
                task_id: task.into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: Some(worktree.into()),
                branch: Some("board/x".into()),
                dispatched_by: None,
            })
            .unwrap();
        if let Some(o) = outcome {
            db.close_attempt(id, o).unwrap();
            // `close_attempt` stamps the real clock; the tests need a chosen one.
            db.conn
                .execute(
                    "UPDATE attempts SET ended_at = ?2 WHERE id = ?1",
                    rusqlite::params![id, rfc3339(now() - Duration::days(days_ago))],
                )
                .unwrap();
        }
        id
    }

    fn plan_now(db: &Db, older_than: i64) -> Vec<Entry> {
        plan(&db.load_tasks().unwrap(), now(), older_than)
    }

    fn only(entries: &[Entry]) -> &Entry {
        assert_eq!(entries.len(), 1, "{entries:#?}");
        &entries[0]
    }

    #[test]
    fn a_closed_task_past_the_cutoff_is_collected() {
        let db = db();
        seed(&db, "linear:LIN-1", UpstreamState::Terminal);
        attempt(&db, "linear:LIN-1", "/wt/lin-1-1", Some(Outcome::Done), 21);
        let e = plan_now(&db, 14 * DAY).to_vec();
        let e = only(&e);
        assert!(e.collectable(), "{e:#?}");
        assert_eq!(e.age_secs, 21 * DAY);
        assert_eq!(e.identifiers, vec!["LIN-1"]);
    }

    #[test]
    fn a_closed_task_inside_the_cutoff_is_kept() {
        let db = db();
        seed(&db, "linear:LIN-1", UpstreamState::Terminal);
        attempt(&db, "linear:LIN-1", "/wt/lin-1-1", Some(Outcome::Done), 3);
        assert_eq!(only(&plan_now(&db, 14 * DAY)).keep, Some(Keep::TooYoung));
    }

    /// The rule that must not break: cancelling ends the attempt, not the issue.
    /// The row is back in `ready`, and a retry reuses this very checkout because
    /// it is the one holding the branch.
    #[test]
    fn a_cancelled_attempt_is_never_collected_however_old() {
        let db = db();
        seed(&db, "linear:LIN-1", UpstreamState::Started);
        attempt(
            &db,
            "linear:LIN-1",
            "/wt/lin-1-1",
            Some(Outcome::Cancelled),
            400,
        );
        assert_eq!(
            only(&plan_now(&db, 14 * DAY)).keep,
            Some(Keep::Retryable(BoardState::Ready))
        );
    }

    #[test]
    fn a_failed_attempt_is_kept_for_the_retry() {
        // `failed` is the state you retry from, and the retry needs this
        // checkout — the branch is in it.
        let db = db();
        for (id, outcome) in [
            ("linear:LIN-1", Outcome::Failed),
            ("linear:LIN-2", Outcome::Orphaned),
        ] {
            seed(&db, id, UpstreamState::Started);
            attempt(&db, id, &format!("/wt/{id}"), Some(outcome), 90);
        }
        let entries = plan_now(&db, 14 * DAY);
        assert_eq!(entries.len(), 2);
        for e in &entries {
            assert_eq!(e.keep, Some(Keep::Retryable(BoardState::Failed)), "{e:#?}");
        }
    }

    #[test]
    fn a_task_waiting_for_review_is_kept() {
        let db = db();
        seed(&db, "linear:LIN-1", UpstreamState::Started);
        attempt(&db, "linear:LIN-1", "/wt/lin-1-1", Some(Outcome::Done), 90);
        db.set_pr("linear:LIN-1", Some("https://pr"), Some(1), true)
            .unwrap();
        assert_eq!(
            only(&plan_now(&db, 14 * DAY)).keep,
            Some(Keep::Retryable(BoardState::Review))
        );
    }

    #[test]
    fn a_live_attempt_is_never_collected() {
        let db = db();
        // Terminal upstream and a live pane: the board says `done`, but an agent
        // is still standing in the directory.
        seed(&db, "linear:LIN-1", UpstreamState::Terminal);
        attempt(&db, "linear:LIN-1", "/wt/lin-1-1", None, 0);
        assert_eq!(only(&plan_now(&db, 14 * DAY)).keep, Some(Keep::Live));
    }

    #[test]
    fn a_checkout_shared_by_a_retry_is_judged_once_for_all_of_them() {
        // A retry reuses the previous attempt's worktree, so the path carries
        // two attempts. It is only collectable when the task is done with it.
        let db = db();
        seed(&db, "linear:LIN-1", UpstreamState::Started);
        attempt(&db, "linear:LIN-1", "/wt/lin-1-1", Some(Outcome::Failed), 90);
        attempt(&db, "linear:LIN-1", "/wt/lin-1-1", None, 0);
        let e = plan_now(&db, 14 * DAY).to_vec();
        let e = only(&e);
        assert_eq!(e.attempts, 2);
        assert_eq!(e.keep, Some(Keep::Live));
    }

    #[test]
    fn the_age_is_the_newest_attempt_in_the_checkout() {
        // An old first attempt does not make a recently-retried checkout old.
        let db = db();
        seed(&db, "linear:LIN-1", UpstreamState::Terminal);
        attempt(&db, "linear:LIN-1", "/wt/lin-1-1", Some(Outcome::Failed), 90);
        attempt(&db, "linear:LIN-1", "/wt/lin-1-1", Some(Outcome::Done), 2);
        let e = plan_now(&db, 14 * DAY).to_vec();
        let e = only(&e);
        assert_eq!(e.age_secs, 2 * DAY);
        assert_eq!(e.keep, Some(Keep::TooYoung));
    }

    #[test]
    fn an_attempt_closed_without_a_timestamp_is_not_treated_as_ancient() {
        // A NULL `ended_at` on a closed attempt is a crashed close, not proof of
        // age — and "unknown age" must never mean "old enough to delete".
        let db = db();
        seed(&db, "linear:LIN-1", UpstreamState::Terminal);
        let a = attempt(&db, "linear:LIN-1", "/wt/lin-1-1", Some(Outcome::Done), 90);
        db.conn
            .execute(
                "UPDATE attempts SET ended_at = NULL WHERE id = ?1",
                rusqlite::params![a],
            )
            .unwrap();
        assert_eq!(only(&plan_now(&db, 14 * DAY)).keep, Some(Keep::TooYoung));
    }

    #[test]
    fn an_attempt_with_no_worktree_is_not_an_entry() {
        let db = db();
        seed(&db, "linear:LIN-1", UpstreamState::Terminal);
        let id = db
            .insert_attempt(&NewAttempt {
                task_id: "linear:LIN-1".into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: None,
                branch: None,
                dispatched_by: None,
            })
            .unwrap();
        db.close_attempt(id, Outcome::Done).unwrap();
        assert!(plan_now(&db, 14 * DAY).is_empty());
    }

    #[test]
    fn marking_a_task_done_by_hand_makes_its_checkout_collectable() {
        // `d mark done` is terminal too — it outranks derivation everywhere else,
        // so it has to here.
        let db = db();
        seed(&db, "linear:LIN-1", UpstreamState::Started);
        attempt(
            &db,
            "linear:LIN-1",
            "/wt/lin-1-1",
            Some(Outcome::Cancelled),
            30,
        );
        db.set_local_done("linear:LIN-1", true).unwrap();
        assert!(only(&plan_now(&db, 14 * DAY)).collectable());
    }

    #[test]
    fn collectable_entries_sort_first() {
        let db = db();
        seed(&db, "linear:LIN-1", UpstreamState::Started);
        attempt(&db, "linear:LIN-1", "/wt/a-keep", Some(Outcome::Failed), 90);
        seed(&db, "linear:LIN-2", UpstreamState::Terminal);
        attempt(&db, "linear:LIN-2", "/wt/z-collect", Some(Outcome::Done), 90);
        let entries = plan_now(&db, 14 * DAY);
        assert!(entries[0].collectable());
        assert!(entries[0].worktree.ends_with("z-collect"));
    }

    #[test]
    fn an_age_without_a_unit_is_refused() {
        // `14` means seconds to the interval parser, and fourteen seconds here
        // would collect the lot.
        let err = parse_age("14").unwrap_err().to_string();
        assert!(err.contains("needs a unit"), "{err}");
        assert!(parse_age("later").is_err());
    }

    #[test]
    fn ages_parse_in_the_units_gc_deals_in() {
        assert_eq!(parse_age("14d").unwrap(), 14 * DAY);
        assert_eq!(parse_age("2w").unwrap(), 14 * DAY);
        assert_eq!(parse_age("36h").unwrap(), 36 * 3_600);
        assert_eq!(parse_age(" 14d ").unwrap(), 14 * DAY);
    }

    #[test]
    fn ages_render_in_the_largest_unit_that_fits() {
        assert_eq!(human_age(21 * DAY), "21d");
        assert_eq!(human_age(5 * 3_600), "5h");
        assert_eq!(human_age(90), "1m");
        assert_eq!(human_age(0), "0m");
    }

    // ---- the census: what herdr says, and what gc makes of it -------------

    /// A herdr that answers from a fixed listing, and records what it was asked
    /// to remove.
    struct FakeHerdr {
        worktrees: Vec<WorktreeInfo>,
        removed: std::cell::RefCell<Vec<String>>,
    }

    impl FakeHerdr {
        fn with(worktrees: Vec<WorktreeInfo>) -> FakeHerdr {
            FakeHerdr {
                worktrees,
                removed: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl Worktrees for FakeHerdr {
        fn list(&self, _repo: &Path) -> Result<Vec<WorktreeInfo>> {
            Ok(self.worktrees.clone())
        }
        fn remove(&self, workspace_id: &str) -> Result<()> {
            self.removed.borrow_mut().push(workspace_id.to_string());
            Ok(())
        }
    }

    fn wt(path: &str, branch: &str, workspace: Option<&str>) -> WorktreeInfo {
        WorktreeInfo {
            path: PathBuf::from(path),
            branch: Some(branch.into()),
            is_linked_worktree: true,
            is_prunable: false,
            open_workspace_id: workspace.map(str::to_string),
        }
    }

    fn entry(path: &str) -> Entry {
        Entry {
            worktree: PathBuf::from(path),
            identifiers: vec!["LIN-1".into()],
            branch: None,
            attempts: 1,
            age_secs: 0,
            keep: None,
            herdr_workspace: None,
        }
    }

    /// A census of one repository the board cuts `board/…` branches in.
    fn census_of(worktrees: Vec<WorktreeInfo>) -> Census {
        Census::take(
            &FakeHerdr::with(worktrees),
            &Logger::new("", false),
            &[Repo {
                path: PathBuf::from("/repo"),
                branch_prefixes: vec!["board/".into()],
            }],
        )
    }

    #[test]
    fn a_checkout_no_attempt_claims_is_listed_not_removed() {
        // The board cannot say whether it is still wanted, so it is reported
        // and the operator decides.
        let census = census_of(vec![
            wt("/wt/lin-1", "board/lin-1", None),
            wt("/wt/stray", "board/lin-9", None),
        ]);
        assert_eq!(
            census.unclaimed(&[entry("/wt/lin-1")]),
            vec![Untracked {
                path: PathBuf::from("/wt/stray"),
                branch: Some("board/lin-9".into()),
            }]
        );
    }

    #[test]
    fn the_repositorys_own_checkout_is_never_a_stray() {
        // It is not a worktree gc has any business reporting, and it is the one
        // entry in every listing that is guaranteed to be unclaimed.
        let mut main = wt("/repo", "master", Some("wB"));
        main.is_linked_worktree = false;
        assert!(census_of(vec![main]).unclaimed(&[]).is_empty());
    }

    #[test]
    fn a_live_workspace_is_found_for_the_checkout_it_holds() {
        let census = census_of(vec![
            wt("/wt/lin-1", "board/lin-1", Some("wE")),
            wt("/wt/lin-2", "board/lin-2", None),
        ]);
        assert_eq!(
            census.workspace_for(Path::new("/wt/lin-1")).as_deref(),
            Some("wE")
        );
        // Cancelling drops the workspace and leaves the checkout: the common
        // case, and the one that must resolve to git rather than herdr.
        assert_eq!(census.workspace_for(Path::new("/wt/lin-2")), None);
        assert_eq!(census.workspace_for(Path::new("/wt/unknown")), None);
    }

    #[test]
    fn a_herdr_that_does_not_answer_leaves_everything_to_git() {
        // Not fatal: gc falls back to git for every checkout, which is the
        // right handling for an orphan anyway. Collecting nothing is not.
        struct Silent;
        impl Worktrees for Silent {
            fn list(&self, _repo: &Path) -> Result<Vec<WorktreeInfo>> {
                bail!("herdr is not running")
            }
            fn remove(&self, _workspace_id: &str) -> Result<()> {
                bail!("herdr is not running")
            }
        }
        let census = Census::take(
            &Silent,
            &Logger::new("", false),
            &[Repo {
                path: PathBuf::from("/repo"),
                branch_prefixes: vec!["board/".into()],
            }],
        );
        assert_eq!(census.workspace_for(Path::new("/wt/lin-1")), None);
        assert!(census.unclaimed(&[]).is_empty());
    }

    #[test]
    fn a_checkout_on_somebody_elses_branch_is_not_reported_at_all() {
        // A repository the board works in is one you work in too, and herdr
        // lists every checkout of it. Reporting your own worktrees — and the
        // ones other agents cut under `.claude/worktrees/` — buries the one
        // line that matters under a dozen that never change.
        let census = census_of(vec![
            wt("/wt/mine", "feat/altinn-innboks", None),
            wt("/wt/agent", "chore/turbo-env", None),
            wt("/wt/leak", "board/lin-9", None),
        ]);
        let found = census.unclaimed(&[]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].path, PathBuf::from("/wt/leak"));
    }

    #[test]
    fn a_template_with_nothing_to_recognise_filters_nothing() {
        // `{identifier_lower}` gives no literal head, and guessing would be
        // worse than reporting everything.
        assert_eq!(branch_prefix("board/{identifier_lower}").as_deref(), Some("board/"));
        assert_eq!(branch_prefix("{identifier_lower}"), None);

        let census = Census::take(
            &FakeHerdr::with(vec![wt("/wt/anything", "whatever", None)]),
            &Logger::new("", false),
            &[Repo {
                path: PathBuf::from("/repo"),
                branch_prefixes: Vec::new(),
            }],
        );
        assert_eq!(census.unclaimed(&[]).len(), 1);
    }

    #[test]
    fn a_checkout_the_board_claims_is_never_untracked_whatever_its_branch() {
        // The branch filter decides what counts as a *stray*; it must not touch
        // what an attempt already claims, or a dispatch with an overridden
        // branch would vanish from the report.
        let census = census_of(vec![wt("/wt/lin-1", "custom-branch", None)]);
        assert!(census.unclaimed(&[entry("/wt/lin-1")]).is_empty());
        assert!(
            census.unclaimed(&[]).is_empty(),
            "and unclaimed, it is somebody's own work as far as gc can tell"
        );
    }

    #[test]
    fn a_workspace_that_vanished_between_listing_and_removal_falls_back_to_git() {
        // The whole reason both paths exist: closing the last pane drops the
        // workspace, and the checkout outlives it.
        struct Vanished;
        impl Worktrees for Vanished {
            fn list(&self, _repo: &Path) -> Result<Vec<WorktreeInfo>> {
                Ok(Vec::new())
            }
            fn remove(&self, _workspace_id: &str) -> Result<()> {
                bail!("workspace_not_found: workspace wE not found")
            }
        }
        let mut e = entry("/wt/definitely-not-here");
        e.herdr_workspace = Some("wE".into());
        // git then fails on a path that is not a checkout — but the point is
        // that it *tried* git, and stopped claiming the workspace was closed.
        let err = collect(&Vanished, &Logger::new("", false), &mut e).unwrap_err();
        assert!(err.to_string().contains("not a git worktree"), "{err}");
        assert_eq!(e.herdr_workspace, None, "the report must not claim it closed");
    }

    #[test]
    fn rows_are_named_against_the_prefix_they_share() {
        let mut report = Report {
            collected: vec![entry("/Users/b/.herdr/worktrees/repo/board-lin-1")],
            untracked: vec![Untracked {
                path: PathBuf::from("/Users/b/.herdr/worktrees/repo/board-lin-2"),
                branch: None,
            }],
            ..Default::default()
        };
        report.root = common_root(&report);
        assert_eq!(
            report.root,
            PathBuf::from("/Users/b/.herdr/worktrees/repo"),
            "the checkouts share a directory, so the rows are bare names"
        );
        assert_eq!(
            name_in(&report.root, &report.collected[0].worktree),
            "board-lin-1"
        );
    }

    #[test]
    fn checkouts_under_two_roots_shorten_to_what_they_share() {
        // What a board that has been running across the switch looks like: some
        // checkouts under herdr's directory, some under the old `$STATE/wt`.
        let mut report = Report {
            collected: vec![
                entry("/Users/b/.herdr/worktrees/repo/board-lin-1"),
                entry("/Users/b/.local/state/herdr/plugins/board/wt/lin-2-1"),
            ],
            ..Default::default()
        };
        report.root = common_root(&report);
        assert_eq!(report.root, PathBuf::from("/Users/b"));
        assert_eq!(
            name_in(&report.root, &report.collected[0].worktree),
            ".herdr/worktrees/repo/board-lin-1"
        );
    }

    #[test]
    fn an_empty_report_has_no_root_to_name() {
        assert_eq!(common_root(&Report::default()), PathBuf::new());
    }
}
