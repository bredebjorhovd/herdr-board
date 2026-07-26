//! `gc`: prune the worktrees of attempts that are finished for good (impl spec
//! §6).
//!
//! Nothing removes a worktree during normal operation, and that is deliberate:
//! git allows a branch in only one worktree, so a retry cannot cut a second
//! checkout of the same branch and instead reuses the one the previous attempt
//! left behind. The cost is that `$STATE/wt/` grows by one checkout per attempt
//! and never shrinks.
//!
//! What makes clearing them safe is the spec's pair of conditions: **terminal
//! and aged**. Terminal is a property of the *task*, not of the attempt — a
//! `cancelled` attempt returns its row to `ready`, so its checkout is exactly
//! the one a retry is going to reuse, and the same is true of `failed` and
//! `review`. Only a task that has reached `done` has no retry left to strand.
//! Age is the second guard: a task closed an hour ago is still one you might be
//! looking at.
//!
//! The branch is never touched. `git worktree remove` frees it for a fresh
//! checkout; deleting it would throw the work away.

use crate::config::{Paths, shorten_home};
use crate::db::Db;
use crate::log::Logger;
use crate::model::{BoardState, Task};
use crate::sync::derivation_for;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

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
}

impl Entry {
    pub fn collectable(&self) -> bool {
        self.keep.is_none()
    }
}

/// What one `gc` run did, or would do under `--dry-run`.
#[derive(Debug, Default)]
pub struct Report {
    pub dry_run: bool,
    pub older_than_secs: i64,
    /// `$STATE/wt`. Named once in the report so every row can be a bare
    /// directory name instead of forty identical characters of prefix.
    pub root: PathBuf,
    /// Removed, or — under `--dry-run` — what removal would be attempted on.
    pub collected: Vec<Entry>,
    /// Recorded on an attempt, but already off disk. Nothing to do.
    pub gone: Vec<Entry>,
    /// Collectable, but git refused. Usually uncommitted work.
    pub skipped: Vec<(Entry, String)>,
    pub kept: Vec<Entry>,
    /// Directories under `$STATE/wt/` that no attempt claims — left alone,
    /// because nothing here knows which repo they came from.
    pub untracked: Vec<PathBuf>,
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
pub fn run(paths: &Paths, log: Arc<Logger>, older_than: &str, dry_run: bool) -> Result<Report> {
    let older_than_secs = parse_age(older_than)?;
    let db = Db::open(&paths.db())?;
    let tasks = db.load_tasks()?;
    let entries = plan(&tasks, Utc::now(), older_than_secs);

    let mut report = Report {
        dry_run,
        older_than_secs,
        root: paths.worktree_root(),
        untracked: untracked_worktrees(&paths.worktree_root(), &entries),
        ..Default::default()
    };

    for entry in entries {
        if entry.keep.is_some() {
            report.kept.push(entry);
            continue;
        }
        if !entry.worktree.exists() {
            report.gone.push(entry);
            continue;
        }
        if dry_run {
            report.collected.push(entry);
            continue;
        }
        match remove_worktree(&entry.worktree) {
            Ok(()) => {
                log.info(format!(
                    "gc removed {} ({}); branch {} left in place",
                    entry.worktree.display(),
                    entry.identifiers.join(", "),
                    entry.branch.as_deref().unwrap_or("—"),
                ));
                report.collected.push(entry);
            }
            Err(e) => {
                log.warn(format!("gc left {}: {e}", entry.worktree.display()));
                report.skipped.push((entry, e.to_string()));
            }
        }
    }
    Ok(report)
}

/// Remove one checkout, leaving its branch alone.
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

/// Directories under `$STATE/wt/` that no attempt in the database claims.
///
/// They are reported, never removed: a task reaped from the board takes its
/// attempts with it, which leaves a checkout nothing here can attribute to a
/// repo or a state — so the operator decides.
fn untracked_worktrees(root: &Path, entries: &[Entry]) -> Vec<PathBuf> {
    let known: HashSet<&Path> = entries.iter().map(|e| e.worktree.as_path()).collect();
    let Ok(dir) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = dir
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !known.contains(p.as_path()))
        .collect();
    found.sort();
    found
}

// ---- reporting ---------------------------------------------------------

pub fn print_report(report: &Report) {
    if report.collected.is_empty()
        && report.kept.is_empty()
        && report.skipped.is_empty()
        && report.untracked.is_empty()
    {
        println!("nothing under {} to collect", shorten_home(&report.root));
        return;
    }
    println!("{}", shorten_home(&report.root));

    let verb = if report.dry_run { "would go" } else { "removed" };
    for e in &report.collected {
        println!(
            "{}",
            row(
                verb,
                report,
                e,
                &format!(
                    "branch {} left in place",
                    e.branch.as_deref().unwrap_or("(none)")
                ),
            )
        );
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
    for p in &report.untracked {
        // The same columns as `row`, with the task and age left blank: neither
        // is knowable for a directory no attempt claims.
        println!(
            "untracked  {:<41}no attempt on the board claims it",
            name_in(&report.root, p),
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
    if !report.skipped.is_empty() {
        println!(
            "{} could not be removed. `git worktree remove --force <path>` if the \
             changes in them are not wanted.",
            report.skipped.len()
        );
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

/// A checkout's name within the worktree root. A reused one can sit outside it
/// — dispatch follows the branch wherever it is already checked out — so that
/// case falls back to the full path rather than pretending.
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

    #[test]
    fn a_directory_no_attempt_claims_is_listed_not_removed() {
        let root = std::env::temp_dir().join(format!("hb-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("tracked")).unwrap();
        std::fs::create_dir_all(root.join("reaped")).unwrap();
        let entries = vec![Entry {
            worktree: root.join("tracked"),
            identifiers: vec!["LIN-1".into()],
            branch: None,
            attempts: 1,
            age_secs: 0,
            keep: None,
        }];
        let found = untracked_worktrees(&root, &entries);
        assert_eq!(found, vec![root.join("reaped")]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
