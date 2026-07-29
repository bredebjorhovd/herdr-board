//! The sync cycle: poll sources, reconcile panes, derive state, drain
//! writebacks (impl spec §4, §6, §7).

use crate::config::{Credentials, Paths, RouteContext, RoutingConfig};
use crate::db::{Db, NewWriteback, Reaped};
use crate::herdr::{Herdr, PaneInfo};
use crate::log::Logger;
use crate::model::*;
use crate::screen::{self, Vitals};
use crate::settled::Settled;
use crate::sources::github::{Github, PullRequest, Rest, pr_matches_branch};
use crate::sources::linear::{GraphQl, Linear};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;

impl GraphQl for Box<dyn GraphQl> {
    fn query(&self, body: &Value) -> Result<Value> {
        (**self).query(body)
    }
}

impl Rest for Box<dyn Rest> {
    fn get(&self, path: &str) -> Result<Value> {
        (**self).get(path)
    }
    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        (**self).post(path, body)
    }
    fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        (**self).patch(path, body)
    }
    fn put(&self, path: &str, body: &Value) -> Result<Value> {
        (**self).put(path, body)
    }
}

/// Health of one upstream source, rendered in the board header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceHealth {
    /// Not configured — the header omits it entirely.
    Absent,
    Ok,
    Down { error: String, retry_in: u64 },
}

/// How a writeback left the queue — see [`SyncEngine::drain_writebacks`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sent {
    /// Reached the source.
    Upstream,
    /// Never sent, and never will be: there is nothing upstream to send it to.
    Dropped(String),
}

pub struct SyncEngine {
    pub db: Db,
    pub cfg: RoutingConfig,
    pub credentials: Credentials,
    pub paths: Paths,
    pub log: Arc<Logger>,
    pub linear: Option<Linear<Box<dyn GraphQl>>>,
    pub github: Option<Github<Box<dyn Rest>>>,
}

/// Meta keys. Kept together so the TUI and the daemon cannot drift.
pub mod meta {
    pub const LAST_SYNC: &str = "last_sync";
    pub const LINEAR_WATERMARK: &str = "linear_watermark";
    pub const LINEAR_STATUS: &str = "linear_status";
    pub const LINEAR_LAST_OK: &str = "linear_last_ok";
    pub const LINEAR_FAILURES: &str = "linear_failures";
    pub const GITHUB_STATUS: &str = "github_status";
    pub const GITHUB_LAST_OK: &str = "github_last_ok";
    pub const GITHUB_FAILURES: &str = "github_failures";
    /// When the last full (unwatermarked) poll ran, which is the only poll that
    /// can tell "deleted upstream" from "unchanged".
    pub const LAST_FULL_SWEEP: &str = "last_full_sweep";
    /// Timestamp of our own last writeback for a task, for the loop guard.
    pub fn writeback_at(task_id: &str) -> String {
        format!("wb_at:{task_id}")
    }
    /// Which review comments have already been delivered into a task's pane —
    /// see [`crate::review::Delivered`].
    pub fn reviews_for(task_id: &str) -> String {
        format!("reviews:{task_id}")
    }
}

impl SyncEngine {
    /// One full cycle. Never returns `Err` for a source outage — a poll failure
    /// marks the header and serves stale data (impl spec §7).
    pub fn sync_once(&self, herdr: Option<&Herdr>) -> Result<()> {
        self.poll_linear();
        // The polled pull requests are handed on rather than refetched: review
        // delivery needs each one's `updated_at` to decide whether asking about
        // its comments is worth a call at all.
        let pulls = self.poll_github();

        // Reconciliation needs to know what herdr currently believes.
        let panes = match herdr {
            Some(h) => match h.pane_list() {
                Ok(p) => Some(p),
                Err(e) => {
                    self.log.warn(format!("pane list failed: {e}"));
                    None
                }
            },
            None => None,
        };
        if let Some(panes) = &panes {
            self.reconcile_with(panes, herdr)?;
        }

        self.rederive_all()?;
        // After deriving, so a task that just reached `done` is disposed of on
        // the same tick rather than the next.
        if let Some(h) = herdr
            && let Err(e) = self.dispose_finished_panes(h)
        {
            self.log.warn(format!("disposing finished panes: {e}"));
        }
        // After derivation, because `review` is the state that keeps a pane
        // alive, and after disposal, so a task that just reached `done` is not
        // woken on its way out (gh#13).
        if let (Some(h), Some(panes)) = (herdr, &panes) {
            self.deliver_reviews(h, panes, &pulls);
        }
        self.drain_writebacks();
        // A repo with a herdr workspace and no board config is silent — its
        // issues are never polled, and until this ran nothing said so. Detection
        // is automatic; adoption is not (AGE-18).
        if let Some(h) = herdr {
            self.detect_unadopted(h);
        }
        self.db.meta_set(meta::LAST_SYNC, &crate::db::now())?;
        Ok(())
    }

    /// Refresh the UNADOPTED section's contents.
    ///
    /// One `workspace list` plus a `git remote` per checkout — cheap enough to
    /// ride every cycle, and the reason the `pane.created` hook can be an
    /// optimization rather than a dependency.
    pub fn detect_unadopted(&self, herdr: &Herdr) -> Vec<crate::adopt::Unadopted> {
        crate::adopt::refresh(&self.db, &self.cfg, herdr, &self.log)
    }

    // ---- polling --------------------------------------------------------

    /// How often to poll without the watermark. An incremental poll cannot see
    /// a deletion — a deleted issue is simply never returned again, which is
    /// indistinguishable from one that has not changed — so periodically the
    /// whole set is fetched and anything missing is reaped.
    const FULL_SWEEP_SECS: i64 = 120;

    fn due_for_full_sweep(&self) -> bool {
        let Ok(Some(last)) = self.db.meta_get(meta::LAST_FULL_SWEEP) else {
            return true;
        };
        chrono::DateTime::parse_from_rfc3339(&last)
            .map(|t| {
                (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds()
                    >= Self::FULL_SWEEP_SECS
            })
            .unwrap_or(true)
    }

    /// Retire tasks the source no longer returns.
    ///
    /// A task with a live attempt is left alone entirely: an agent is working on
    /// it, and the row vanishing underneath a running pane would be worse than a
    /// stale row. Reconciliation will orphan it if the pane dies.
    ///
    /// Closed attempts are kept too, as a `gone` row rather than a deletion —
    /// see [`Db::reap_task`]. Only a task nobody ever dispatched is forgotten.
    fn reap_missing(&self, source: Source, seen: &std::collections::HashSet<String>) {
        let Ok(known) = self.db.reapable_task_ids(source) else {
            return;
        };
        let live: std::collections::HashSet<String> = self
            .db
            .live_attempts()
            .unwrap_or_default()
            .into_iter()
            .map(|a| a.task_id)
            .collect();
        for id in known {
            if seen.contains(&id) {
                continue;
            }
            if live.contains(&id) {
                self.log.warn(format!(
                    "{id} is gone from {} but still has a live attempt — keeping it",
                    source.as_str()
                ));
                continue;
            }
            match self.db.reap_task(&id) {
                Ok(Reaped::Forgotten) => self.log.info(format!(
                    "{id} no longer exists upstream and was never dispatched — removed"
                )),
                Ok(Reaped::Kept { attempts }) => self.log.info(format!(
                    "{id} no longer exists upstream — marked gone, keeping {attempts} \
                     attempt(s) so `gc` can still collect their worktrees"
                )),
                Err(e) => self.log.error(format!("reaping {id}: {e}")),
            }
        }
    }

    fn poll_linear(&self) {
        let Some(linear) = &self.linear else {
            return;
        };
        let full_sweep = self.due_for_full_sweep();
        let watermark = if full_sweep {
            None
        } else {
            self.db.meta_get(meta::LINEAR_WATERMARK).ok().flatten()
        };

        // Issues we hold live attempts against are fetched regardless of the
        // board filter, so writeback targets stay fresh after they leave the
        // queue (impl spec §4.1).
        let live_ids: Vec<String> = self
            .db
            .live_attempts()
            .unwrap_or_default()
            .iter()
            .filter_map(|a| self.db.get_task(&a.task_id).ok().flatten())
            .filter(|t| t.source == Source::Linear)
            .map(|t| t.source_id)
            .collect();

        // Local midnight, so today's finished work stays on the board and
        // yesterday's falls off by itself.
        let today = chrono::Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|d| {
                d.and_local_timezone(chrono::Local)
                    .earliest()
                    .map(|t| crate::db::rfc3339(t.with_timezone(&chrono::Utc)))
            })
            .unwrap_or_default();

        let result = linear
            .fetch_board_issues(
                &self.cfg.sync.labels,
                watermark.as_deref(),
                today.as_deref(),
            )
            .and_then(|mut issues| {
                let extra = linear.fetch_issues_by_id(&live_ids)?;
                let known: std::collections::HashSet<_> =
                    issues.iter().map(|i| i.id.clone()).collect();
                issues.extend(extra.into_iter().filter(|i| !known.contains(&i.id)));
                Ok(issues)
            });

        match result {
            Ok(issues) => {
                let mut high = watermark.clone().unwrap_or_default();
                for i in &issues {
                    if self.is_own_echo(&i.task_id(), &i.updated_at) {
                        self.log
                            .info(format!("loop guard: ignoring our own update on {}", i.identifier));
                    }
                    if let Err(e) = self.db.upsert_task(&i.to_upsert()) {
                        self.log.error(format!("upsert {}: {e}", i.identifier));
                        continue;
                    }
                    // A PR attached to the Linear issue is one of the two ways a
                    // task reaches `review`.
                    if let Some(pr) = i.pr_url() {
                        let number = pr
                            .rsplit('/')
                            .next()
                            .and_then(|n| n.parse::<i64>().ok());
                        let _ = self.db.set_pr(&i.task_id(), Some(pr), number, true);
                    }
                    if i.updated_at > high {
                        high.clone_from(&i.updated_at);
                    }
                }
                if !high.is_empty() {
                    let _ = self.db.meta_set(meta::LINEAR_WATERMARK, &high);
                }
                if full_sweep {
                    // This response is the complete set, so anything of ours
                    // that is missing from it is genuinely gone.
                    let seen: std::collections::HashSet<String> =
                        issues.iter().map(|i| i.task_id()).collect();
                    self.reap_missing(Source::Linear, &seen);
                    let _ = self.db.meta_set(meta::LAST_FULL_SWEEP, &crate::db::now());
                }
                let _ = self.db.meta_set(meta::LINEAR_STATUS, "ok");
                let _ = self.db.meta_set(meta::LINEAR_LAST_OK, &crate::db::now());
                let _ = self.db.meta_set(meta::LINEAR_FAILURES, "0");
                self.log.info(format!("linear: {} issues", issues.len()));
            }
            Err(e) => {
                // Serve stale data and mark the header. Never blank the list.
                let failures = self.bump_failures(meta::LINEAR_FAILURES);
                let _ = self
                    .db
                    .meta_set(meta::LINEAR_STATUS, &format!("error:{e}"));
                self.log
                    .warn(format!("linear poll failed (attempt {failures}): {e}"));
            }
        }
    }

    /// Returns every pull request seen this cycle, so the caller does not have
    /// to ask GitHub for them a second time.
    fn poll_github(&self) -> Vec<PullRequest> {
        let Some(gh) = &self.github else {
            return Vec::new();
        };
        if self.cfg.github.repos.is_empty() {
            return Vec::new();
        }
        let mut all_pulls: Vec<PullRequest> = Vec::new();
        let mut failed: Option<String> = None;
        // GitHub is always polled in full, so every cycle can reap.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for repo in &self.cfg.github.repos {
            // Per repo, not one filter for all of them: `labels = []` is the
            // right answer for a curated tracker and a backlog dump for a repo
            // that keeps its roadmap as open issues.
            match gh.issues(repo, self.cfg.github.labels_for(repo)) {
                Ok(issues) => {
                    for i in issues {
                        seen.insert(i.task_id());
                        // Two writers on one issue now — the board and the agent
                        // in the pane — so the §4.1 loop guard carries across.
                        if self.is_own_echo(&i.task_id(), &i.updated_at) {
                            self.log.info(format!(
                                "loop guard: ignoring our own update on {}",
                                i.task_id()
                            ));
                        }
                        if let Err(e) = self.db.upsert_task(&i.to_upsert()) {
                            self.log.error(format!("upsert {}: {e}", i.task_id()));
                        }
                    }
                }
                Err(e) => failed = Some(e.to_string()),
            }
            match gh.pulls(repo) {
                Ok(p) => all_pulls.extend(p),
                Err(e) => failed = Some(e.to_string()),
            }
        }

        // A PR whose branch belongs to an attempt is that task's PR, not a row
        // of its own — otherwise dispatched work would appear twice.
        //
        // Scoped by repository where the task names one: `board/gh-2` in OIOS is
        // not the tripletex attempt's branch merely because the strings match,
        // and suppressing it hides a real pull request behind a coincidence
        // (AGE-20). New branches are repo-qualified, but attempts recorded
        // before that still hold the ambiguous name, so the check carries the
        // scope rather than trusting the string. A Linear task names no repo, so
        // its branch is honoured in whichever repo the PR turns up in.
        let attempt_branches = self.attempt_branches();

        if let Err(e) = self.link_pull_requests(&all_pulls) {
            self.log.error(format!("linking PRs: {e}"));
        }

        // Mergeability costs one call per open PR, so it rides the full sweep
        // rather than every poll. It is the fact that matters most about a PR
        // waiting on you when several branches are in flight at once.
        let check_mergeable = self.due_for_full_sweep();

        if self.cfg.github.pull_requests {
            for pr in &all_pulls {
                if attempt_branches.claims(pr) {
                    continue;
                }
                if let Err(e) = self.db.upsert_task(&pr.to_upsert()) {
                    self.log.error(format!("upsert {}: {e}", pr.task_id()));
                    continue;
                }
                seen.insert(pr.task_id());
                // Setting the PR fields is what makes derivation reach
                // `review` rather than `ready`.
                let _ = self.db.set_pr(
                    &pr.task_id(),
                    Some(&pr.url),
                    Some(pr.number),
                    pr.open,
                );
                let _ = self.db.set_pr_merged(&pr.task_id(), pr.merged);
                if check_mergeable && pr.open {
                    let state = gh.mergeable_state(&pr.repo, pr.number);
                    let _ = self.db.set_pr_mergeable(&pr.task_id(), state.as_deref());
                }
            }
        }
        // Only reap when every repo answered: a failed poll would otherwise look
        // like every issue in that repo had been deleted.
        if failed.is_none() {
            self.reap_missing(Source::Github, &seen);
        }

        match failed {
            None => {
                let _ = self.db.meta_set(meta::GITHUB_STATUS, "ok");
                let _ = self.db.meta_set(meta::GITHUB_LAST_OK, &crate::db::now());
                let _ = self.db.meta_set(meta::GITHUB_FAILURES, "0");
            }
            Some(e) => {
                let failures = self.bump_failures(meta::GITHUB_FAILURES);
                let _ = self.db.meta_set(meta::GITHUB_STATUS, &format!("error:{e}"));
                self.log
                    .warn(format!("github poll failed (attempt {failures}): {e}"));
            }
        }
        all_pulls
    }

    /// Every branch the board has dispatched onto, with the repository it was
    /// dispatched for when the task names one.
    fn attempt_branches(&self) -> AttemptBranches {
        let mut set = AttemptBranches::default();
        for task in self.db.load_tasks().unwrap_or_default() {
            for branch in task.attempts.iter().filter_map(|a| a.branch.clone()) {
                match crate::model::gh_repo(&task.id) {
                    Some(repo) => set.in_repo.entry(repo.to_string()).or_default().insert(branch),
                    None => set.anywhere.insert(branch),
                };
            }
        }
        set
    }

    /// Attach PRs to tasks by attempt branch (`board/<identifier>`), which is
    /// the link the dispatcher creates.
    pub fn link_pull_requests(&self, pulls: &[PullRequest]) -> Result<()> {
        if pulls.is_empty() {
            return Ok(());
        }
        let check_mergeable = self.due_for_full_sweep();
        let gh = self.github.as_ref();
        for task in self.db.load_tasks()? {
            let branches: Vec<String> = task
                .attempts
                .iter()
                .filter_map(|a| a.branch.clone())
                .collect();
            // A GitHub task owns a repo, and only that repo's pull requests can
            // be its own. Branch names are not unique across repos — `gh#2` in
            // two repos both branched to `board/gh-2` — so matching on the
            // branch alone attached another repo's merged PR to this task and
            // derived it straight to review (AGE-20). Branches carry their repo
            // now, but the scope stays: attempts recorded before that do not,
            // and `--branch` can name anything at all. Linear identifiers are
            // globally unique, so Linear rows need no such scoping.
            let own_repo = crate::model::gh_repo(&task.id);
            let Some(pr) = pulls.iter().find(|p| {
                own_repo.is_none_or(|r| p.repo == r)
                    && branches.iter().any(|b| pr_matches_branch(p, b))
            }) else {
                continue;
            };
            self.db
                .set_pr(&task.id, Some(&pr.url), Some(pr.number), pr.open)?;
            // Observing the merge is the same fact as performing it. Only `m`
            // used to say so, so a PR merged with `gh pr merge` or on the web
            // left its ticket in review forever — and not merely unadvanced:
            // nothing but a finished task ends `review`, so the state kept
            // standing and the review writeback kept asserting it, which is
            // what pulled a hand-closed AGE-17 back again (also AGE-18,
            // AGE-21). The idempotency key stops the next poll re-sending it.
            if pr.merged && !task.pr_merged && !task.state.is_terminal() {
                self.finish_on_merge(&task, &pr.repo, pr.number)?;
            }
            self.db.set_pr_merged(&task.id, pr.merged)?;
            if check_mergeable
                && pr.open
                && let Some(gh) = gh
            {
                let state = gh.mergeable_state(&pr.repo, pr.number);
                self.db.set_pr_mergeable(&task.id, state.as_deref())?;
            }
        }
        Ok(())
    }

    fn bump_failures(&self, key: &str) -> i64 {
        let n = self
            .db
            .meta_get(key)
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        let _ = self.db.meta_set(key, &n.to_string());
        n
    }

    /// Loop guard (impl spec §4.1): an update whose timestamp sits inside the
    /// window right after our own writeback is our own echo. We never dispatch
    /// from upstream events at all, so this only has to stop log churn.
    fn is_own_echo(&self, task_id: &str, updated_at: &str) -> bool {
        let Ok(Some(ours)) = self.db.meta_get(&meta::writeback_at(task_id)) else {
            return false;
        };
        let (Ok(ours), Ok(theirs)) = (
            chrono::DateTime::parse_from_rfc3339(&ours),
            chrono::DateTime::parse_from_rfc3339(updated_at),
        ) else {
            return false;
        };
        let delta = (theirs - ours).num_seconds();
        (0..=5).contains(&delta)
    }

    // ---- reconciliation -------------------------------------------------

    /// Has this attempt produced commits on its branch?
    ///
    /// A pull request is not the only evidence of finished work: an agent that
    /// commits locally and stops is done, and waiting for a PR that is never
    /// coming leaves the row `working` forever. Counting commits against the
    /// repo's default branch is the local equivalent of "explicit done
    /// detection".
    pub(crate) fn attempt_has_commits(
        &self,
        worktree: Option<&str>,
        base_sha: Option<&str>,
    ) -> bool {
        let Some(worktree) = worktree else {
            return false;
        };
        if !std::path::Path::new(worktree).exists() {
            return false;
        }
        // The attempt's own starting commit is the only correct base. Anything
        // else measures the operator's unpushed work as the agent's: a repo
        // whose default branch is one commit ahead of its remote made every
        // dispatch look finished the instant it started (AGE-19).
        if let Some(sha) = base_sha {
            let out = Command::new("git")
                .args(["-C", worktree, "rev-list", "--count", &format!("{sha}..HEAD")])
                .output();
            if let Ok(o) = out
                && o.status.success()
                && let Ok(n) = String::from_utf8_lossy(&o.stdout).trim().parse::<u32>()
            {
                return n > 0;
            }
            // The recorded base is gone (worktree rebuilt, history rewritten).
            // Fall through rather than guess — but the fallback below is the
            // very thing that was wrong, so say so.
            self.log.info(format!(
                "base {sha} unusable in {worktree}; falling back to remote-relative count"
            ));
        }
        // Attempts dispatched before base_sha existed have no starting point
        // recorded, so they keep the old, weaker measurement.
        let base = Command::new("git")
            .args(["-C", worktree, "rev-parse", "--abbrev-ref", "origin/HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "HEAD@{upstream}".to_string());

        let count = |range: &str| -> Option<u32> {
            let out = Command::new("git")
                .args(["-C", worktree, "rev-list", "--count", range])
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
                .flatten()
        };

        // Try the remote default branch, then the local one git reports for the
        // main checkout. Either way: commits on this branch that are not on the
        // base mean the agent produced something.
        for range in [format!("{base}..HEAD"), "master..HEAD".into(), "main..HEAD".into()] {
            if let Some(n) = count(&range) {
                return n > 0;
            }
        }
        false
    }

    /// Map live attempts onto herdr's current pane reality (impl spec §6).
    /// Tell the operator that released work has settled.
    ///
    /// Only when an attempt *ends* — not on every state change — because a
    /// notification that fires constantly is one nobody reads. This is the
    /// audience that can always be interrupted, and until AGE-25 it was the only
    /// one the board had: see [`SyncEngine::wake_dispatcher`] for the agent that
    /// released the work, which is told separately and only if asked for.
    fn notify_settled(&self, herdr: Option<&Herdr>, task: &Task, what: &str) {
        if !self.cfg.defaults.notify {
            return;
        }
        let Some(h) = herdr else { return };
        h.notify(
            &format!("{} {what}", task.identifier),
            &truncate_for_toast(&task.title),
        );
    }

    /// End an attempt, and tell everyone waiting on it.
    ///
    /// The three ends an attempt can come to all pass through here, so the two
    /// audiences — the operator's notification and the dispatching agent's
    /// prompt — cannot drift apart by somebody adding a fourth and remembering
    /// only one of them.
    ///
    /// Order matters. The close and the writeback are durable and instant; the
    /// wake takes seconds and talks to a pane that may refuse. Recording first
    /// means a crash inside the wake loses a notice, which is exactly what the
    /// board did before AGE-25; waking first would re-settle the attempt on the
    /// next tick and tell the dispatcher twice.
    fn settle(
        &self,
        herdr: Option<&Herdr>,
        task: &Task,
        attempt: &Attempt,
        settled: Settled,
        pr_url: Option<&str>,
    ) -> Result<()> {
        self.notify_settled(herdr, task, settled.toast());
        self.db.close_attempt(attempt.id, settled.outcome())?;
        self.enqueue_outcome(task, settled.outcome(), pr_url)?;
        self.wake_dispatcher(herdr, task, attempt, settled, pr_url);
        Ok(())
    }

    pub fn reconcile(&self, panes: &[PaneInfo]) -> Result<()> {
        self.reconcile_with(panes, None)
    }

    pub fn reconcile_with(&self, panes: &[PaneInfo], herdr: Option<&Herdr>) -> Result<()> {
        let by_id: HashMap<&str, &PaneInfo> =
            panes.iter().map(|p| (p.pane_id.as_str(), p)).collect();

        for attempt in self.db.live_attempts()? {
            let Some(pane_id) = attempt.pane_id.as_deref() else {
                // Dispatch is still in flight; nothing to reconcile yet.
                continue;
            };
            let Some(task) = self.db.get_task(&attempt.task_id)? else {
                continue;
            };

            match by_id.get(pane_id) {
                None => {
                    // Impl spec §7: treat an unknown pane as orphaned only after
                    // two consecutive ticks, so a live handoff does not flap
                    // every attempt into `failed`.
                    let ticks = attempt.missing_ticks + 1;
                    if ticks >= 2 {
                        self.log.warn(format!(
                            "{} pane {} gone for {} ticks — orphaned",
                            task.identifier, pane_id, ticks
                        ));
                        self.settle(herdr, &task, &attempt, Settled::PaneExited, None)?;
                    } else {
                        self.log.info(format!(
                            "{} pane {} missing (tick {}/2)",
                            task.identifier, pane_id, ticks
                        ));
                        self.db.set_missing_ticks(attempt.id, ticks)?;
                    }
                }
                Some(pane) if pane.agent.is_none() => {
                    // The pane outlived its agent. Same fact as a pane that
                    // exited, as far as this attempt is concerned, so it gets
                    // the same two-tick rule — and it has to be handled
                    // separately because the pane *is* still there, so the
                    // missing-pane branch above never fires for it.
                    //
                    // Found dispatching to opencode (AGE-26): opencode noticed
                    // a new release on launch, upgraded itself and exited,
                    // leaving the pane back at a shell prompt. herdr then had
                    // no agent to report a status for, `saw_working` never
                    // latched, and the AGE-19 guard correctly refused to settle
                    // on commits — so the row sat `working` with nothing left
                    // alive to change it. Codex can reach the same state by
                    // exiting on a usage-limit banner. Claude Code does neither,
                    // which is why two dozen attempts never hit this.
                    //
                    // Only an attempt that never worked is reaped here. An agent
                    // that got going, committed and was then quit is a
                    // *finished* attempt, not a failed start, so it takes the
                    // ordinary path and settles on its commits.
                    if attempt.saw_working {
                        self.reconcile_live_pane(&task, &attempt, pane, herdr)?;
                        continue;
                    }
                    let ticks = attempt.missing_ticks + 1;
                    if ticks >= 2 {
                        self.log.warn(format!(
                            "{} agent gone from pane {} for {} ticks without ever \
                             working — orphaned",
                            task.identifier, pane_id, ticks
                        ));
                        self.settle(herdr, &task, &attempt, Settled::NeverStarted, None)?;
                    } else {
                        self.log.info(format!(
                            "{} pane {} has no agent (tick {}/2)",
                            task.identifier, pane_id, ticks
                        ));
                        self.db.set_missing_ticks(attempt.id, ticks)?;
                    }
                }
                Some(pane) => self.reconcile_live_pane(&task, &attempt, pane, herdr)?,
            }
        }
        Ok(())
    }

    /// Reconcile one attempt whose pane herdr still reports an agent in.
    fn reconcile_live_pane(
        &self,
        task: &Task,
        attempt: &Attempt,
        pane: &PaneInfo,
        herdr: Option<&Herdr>,
    ) -> Result<()> {
        let pane_id = pane.pane_id.as_str();
        if attempt.missing_ticks != 0 {
            // It came back — a handoff, not a death.
            self.db.set_missing_ticks(attempt.id, 0)?;
        }
        let reported = pane.agent_status.unwrap_or(AgentStatus::Unknown);
        // gh#32: `working` is the one status herdr can hold long after the turn
        // it was reported for ended. The detection region reaches 20 non-empty
        // lines up the screen — it has to, or a todo list hides a live spinner —
        // and Claude Code leaves spinner-glyph lines in scrollback: a stale
        // `✳ Tinkering…` from a turn that died on an Anthropic 5xx, or a
        // past-tense `✻ Crunched for 3m 19s`. Three attempts sat `working` for
        // over an hour on lines like those, holding slots, unable to settle,
        // with nothing said to the operator or to the pane that released them.
        //
        // No manifest can fix it: it matches one screenshot, and the fact that
        // separates a live spinner from a frozen one is change over time. So
        // when herdr says `working`, ask the pane whether anything is moving.
        let vitals = self.vitals(herdr, attempt, pane, reported);
        // A frozen screen is not a running turn. Reading it as `idle` puts the
        // attempt back on the ordinary settle path — a pull request or commits
        // end it, exactly as they would for an agent that stopped cleanly — and
        // the dead ends below are where a stall gets named for what it is.
        let status = match vitals {
            Vitals::Frozen { .. } => AgentStatus::Idle,
            Vitals::Alive | Vitals::Watching => reported,
        };
        // An agent that has just started waiting on you is more urgent than one
        // that has finished: it is burning wall-clock right now, and nothing
        // else on screen says so unless you happen to be looking at the board.
        if status == AgentStatus::Blocked && attempt.agent_status != Some(AgentStatus::Blocked) {
            self.notify_settled(herdr, task, "needs you");
        }
        if status == AgentStatus::Unknown {
            // Worth a line: `unknown` is not proof of completion, and the agent
            // name says what herdr actually saw.
            self.log.info(format!(
                "{} pane {} agent {:?} is unclassified",
                task.identifier, pane_id, pane.agent
            ));
        }
        // Persist it so the TUI can render the dim `idle` marker without
        // shelling out to herdr on its own tick. A frozen pane is written `idle`
        // here and refined to `blocked` by `report_stalled` further down, inside
        // this same pass — nothing re-derives in between.
        self.db.set_attempt_status(attempt.id, status)?;
        // Latch that the agent actually got going, so a settled status can be
        // told apart from one that never started.
        if status == AgentStatus::Working && !attempt.saw_working {
            self.db.set_saw_working(attempt.id)?;
        }
        // An agent that has settled *and* produced an artifact is the only
        // explicit done detection we have. Without one the attempt stays live
        // and the row renders a dim `idle` marker.
        let settled = matches!(
            status,
            AgentStatus::Idle | AgentStatus::Done | AgentStatus::Unknown
        );
        if !settled {
            // Back at work: whatever the last samples said, they were not the
            // end of the attempt.
            return self.clear_settled_ticks(attempt);
        }
        // A PR is the agent's own declaration that it is finished, and it
        // cannot exist unless something ran. Nothing to second-guess: settle on
        // the first sample that sees one.
        if task.pr_open {
            return self.settle_now(herdr, task, attempt, status, "PR");
        }
        // gh#32: an `API Error: 5xx` on a frozen screen is the cause named
        // outright, and it outranks the commits below. Commits mean the agent
        // got somewhere, not that it finished — `agent-conventions.md` tells
        // dispatched agents to commit even without a PR — and settling one that
        // died mid-task as `finished` would tell the operator the opposite of
        // what happened. A human retrying is what unsticks a 529, so say so.
        if let Vitals::Frozen {
            api_error: Some(err),
        } = &vitals
        {
            return self.report_stalled(herdr, task, attempt, Some(err));
        }
        // Commits on the attempt branch are evidence too — an agent that
        // commits locally and stops would otherwise sit `working` forever —
        // but they are much weaker. `agent-conventions.md` tells dispatched
        // agents to "commit even when you are not opening a PR", so the
        // artifact is routinely there long before the work is done.
        //
        // Commits alone only count once the agent has been seen working. A
        // just-started Claude reports `idle` because it has not been handed its
        // prompt yet — several seconds pass between `agent start` and `agent
        // prompt` — and reaping it there ends the attempt before it begins.
        if !attempt.saw_working
            || !self.attempt_has_commits(attempt.worktree.as_deref(), attempt.base_sha.as_deref())
        {
            // Nothing to settle on. This is where an attempt used to disappear:
            // herdr said `working`, so it never got here at all, and the row sat
            // in the WORKING section with no agent behind it and no end in sight.
            // A frozen screen with nothing to show for it is not a finished
            // attempt either — it is one that needs a person (gh#32).
            if matches!(vitals, Vitals::Frozen { .. }) {
                return self.report_stalled(herdr, task, attempt, None);
            }
            return self.clear_settled_ticks(attempt);
        }
        // gh#18: and one settled sample is not "finished". `idle` is a screen
        // classification that flaps mid-turn — the bottom of the pane loses the
        // working line behind long bash output or a large diff — and
        // `pane.agent_status_changed` invokes this path at the moment of the
        // flap rather than waiting for the next 30s sweep. So the weak artifact
        // gets the debounce the vanished pane already has above: two
        // consecutive samples, and any sample that says `working` starts over.
        let ticks = attempt.settled_ticks + 1;
        if ticks < 2 {
            self.log.info(format!(
                "{} agent {} with commits (tick {}/2)",
                task.identifier,
                status.as_str(),
                ticks
            ));
            self.db.set_settled_ticks(attempt.id, ticks)?;
            return Ok(());
        }
        self.settle_now(herdr, task, attempt, status, "commits")
    }

    /// Ask a pane herdr calls `working` whether anything is actually moving.
    ///
    /// The rule itself is [`crate::screen::vitals`], which is pure. This is the
    /// clock and the storage around it: read the screen, compare it against the
    /// one recorded for this attempt, and record the new one when it differs so
    /// that `screen_at` keeps meaning *first seen*.
    ///
    /// Costs one `herdr pane read` per live `working` attempt per reconcile —
    /// bounded by `max_concurrent_per_workspace`, which is 3 by default, and only
    /// paid for attempts whose status is the one that can lie. Not rate-limited
    /// on purpose: `pane.agent_status_changed` fires reconciliation between
    /// daemon cycles, and skipping the read on those ticks would hand back
    /// [`Vitals::Watching`] for a pane already known to be frozen — which reads
    /// as `working`, and clears the settled-tick run gh#18 needs two of.
    fn vitals(
        &self,
        herdr: Option<&Herdr>,
        attempt: &Attempt,
        pane: &PaneInfo,
        reported: AgentStatus,
    ) -> Vitals {
        // Only `working` is in question. `blocked` is a dialog on screen, which
        // is a thing one sample can see and which is *supposed* to hold still;
        // the settled statuses already lead somewhere sensible.
        if reported != AgentStatus::Working {
            return Vitals::Watching;
        }
        // Somebody is reading this pane's scrollback, so the visible screen is
        // not the live one and would not move even for a working agent.
        if pane.scroll_offset > 0 {
            return Vitals::Watching;
        }
        let Some(h) = herdr else {
            return Vitals::Watching;
        };
        let Some(screen) = h.pane_read_visible(&pane.pane_id) else {
            // A screen we could not read is a question left open, exactly as it
            // is for delivery. Never an accusation.
            return Vitals::Watching;
        };
        // Both halves or neither. A print that cannot be aged cannot conclude
        // anything — and an attempt carrying one from before this check existed
        // would otherwise be compared forever against a timestamp it never had,
        // so it is treated as a first look and restamped below.
        let previous = match (attempt.screen_print.as_deref(), attempt.screen_at.as_deref()) {
            (Some(fingerprint), Some(at)) => chrono::DateTime::parse_from_rfc3339(at)
                .ok()
                .map(|t| screen::Sample {
                    fingerprint,
                    age_secs: (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds(),
                }),
            _ => None,
        };
        let vitals = screen::vitals(previous, &screen);
        if previous.is_none() || vitals == Vitals::Alive {
            // The screen moved (or this is the first look), so this is the
            // moment it started showing what it shows. A frozen one is left
            // alone deliberately: overwriting its timestamp would restart the
            // clock every tick and it could never be called frozen again.
            if let Err(e) = self
                .db
                .set_screen_sample(attempt.id, &screen::fingerprint(&screen))
            {
                self.log
                    .warn(format!("recording {}'s screen: {e}", pane.pane_id));
            }
        }
        vitals
    }

    /// Report an attempt whose pane has stopped moving, with the reason when the
    /// screen names one.
    ///
    /// `blocked`, not `failed` and not settled. The work is not finished and the
    /// attempt is not over: the pane is still there, holding its checkout and its
    /// history, and what unsticks a 529 is a person — retrying it, or cancelling
    /// the attempt back to `ready`. `blocked` is the state that says "needs you"
    /// and that keeps counting against `max_concurrent_per_workspace`, which is
    /// honest, because the pane really is still occupying a slot.
    fn report_stalled(
        &self,
        herdr: Option<&Herdr>,
        task: &Task,
        attempt: &Attempt,
        api_error: Option<&str>,
    ) -> Result<()> {
        let why = match api_error {
            Some(err) => format!("stalled — {err}"),
            None => format!(
                "stalled — its screen has not changed in {}s",
                screen::STALL_SECS
            ),
        };
        // Once, on the way in. A pane can sit frozen for hours and the operator
        // should not be told about it on every cycle.
        if attempt.agent_status != Some(AgentStatus::Blocked) {
            self.log.warn(format!(
                "{} pane {} reports working but nothing is running — {why}",
                task.identifier,
                attempt.pane_id.as_deref().unwrap_or("?")
            ));
            self.notify_settled(herdr, task, &truncate_for_toast(&why));
        }
        self.db
            .set_attempt_status(attempt.id, AgentStatus::Blocked)?;
        // Whatever the last samples were counting towards, it was not this.
        self.clear_settled_ticks(attempt)
    }

    /// Forget a run of settled-looking samples that did not end the attempt.
    fn clear_settled_ticks(&self, attempt: &Attempt) -> Result<()> {
        if attempt.settled_ticks != 0 {
            self.db.set_settled_ticks(attempt.id, 0)?;
        }
        Ok(())
    }

    /// Close an attempt whose evidence has cleared whatever bar it had to.
    fn settle_now(
        &self,
        herdr: Option<&Herdr>,
        task: &Task,
        attempt: &Attempt,
        status: AgentStatus,
        why: &str,
    ) -> Result<()> {
        self.log.info(format!(
            "{} agent {} with {why} — attempt done",
            task.identifier,
            status.as_str()
        ));
        self.settle(
            herdr,
            task,
            attempt,
            Settled::Finished,
            task.pr_url.as_deref(),
        )
    }

    /// Refresh what the board *displays* from herdr, and nothing else.
    ///
    /// Deliberately not [`SyncEngine::reconcile`]: that owns lifecycle
    /// decisions — orphaning a vanished pane, finalizing a finished attempt —
    /// and running it from two processes would double-count `missing_ticks` and
    /// orphan a pane that was merely handing over. This only writes the agent
    /// status a live attempt is showing, so a reader can pick up a `blocked`
    /// the moment it happens without racing the daemon.
    pub fn refresh_agent_status(&self, panes: &[PaneInfo]) -> Result<bool> {
        let by_id: HashMap<&str, &PaneInfo> =
            panes.iter().map(|p| (p.pane_id.as_str(), p)).collect();
        let mut changed = false;
        for attempt in self.db.live_attempts()? {
            let Some(pane_id) = attempt.pane_id.as_deref() else {
                continue;
            };
            let Some(pane) = by_id.get(pane_id) else {
                // Missing panes are the daemon's business, not ours.
                continue;
            };
            let status = pane.agent_status.unwrap_or(AgentStatus::Unknown);
            if attempt.agent_status != Some(status) {
                self.db.set_attempt_status(attempt.id, status)?;
                changed = true;
            }
        }
        if changed {
            self.rederive_all()?;
        }
        Ok(changed)
    }

    /// Close panes belonging to tasks that are finished for good.
    ///
    /// An attempt closing leaves its pane alone on purpose — that is the moment
    /// you want to read what the agent did. Once the *task* reaches `done`,
    /// though, the pane is an idle agent in a checkout nobody is going to look
    /// at again, and leaving it costs a slot in the layout for the rest of the
    /// session.
    pub fn dispose_finished_panes(&self, herdr: &Herdr) -> Result<usize> {
        let mut closed = 0;
        for task in self.db.load_tasks()? {
            if task.state != BoardState::Done {
                continue;
            }
            let live_panes: std::collections::HashSet<String> = herdr
                .pane_list()
                .unwrap_or_default()
                .into_iter()
                .map(|p| p.pane_id)
                .collect();
            for attempt in &task.attempts {
                // Only closed attempts: a live one is still running, and
                // `cancel` is the way to end that.
                let (Some(pane), Some(_)) = (attempt.pane_id.as_deref(), attempt.outcome)
                else {
                    continue;
                };
                if !live_panes.contains(pane) {
                    continue;
                }
                match herdr.pane_close(pane) {
                    Ok(()) => {
                        self.log.info(format!(
                            "{} is done — closed its pane {pane}",
                            task.identifier
                        ));
                        closed += 1;
                    }
                    Err(e) => self.log.warn(format!("closing {pane}: {e}")),
                }
            }
        }
        Ok(closed)
    }

    /// Recompute and persist every task's derived state, using the agent status
    /// reconciliation last stored on each live attempt.
    pub fn rederive_all(&self) -> Result<()> {
        self.rederive_with(&HashMap::new())
    }

    /// `override_status` lets a caller (and the tests) supply pane statuses
    /// directly; otherwise the value stored on the attempt is used.
    pub fn rederive_with(&self, override_status: &HashMap<String, AgentStatus>) -> Result<()> {
        for task in self.db.load_tasks()? {
            let state = derive_state(derivation_for(&task, override_status));
            if state != task.state {
                self.log
                    .info(format!("{}: {} → {}", task.identifier, task.state, state));
            }
            // A Linear row that has reached `review` is work that is finished and
            // waiting on a human — and Linear was never told. Dispatch moves the
            // issue to a started-type state and nothing moves it again until a
            // merge, so anyone reading Linear rather than the board sees
            // In Progress for the whole review window (AGE-21).
            //
            // Only when `[linear] review_state` names the state to move to:
            // Linear has no review *type* to resolve, so with nothing configured
            // there is no correct target and the ticket stays where it is.
            if state == BoardState::Review
                && task.source == Source::Linear
                && !task.upstream.is_final()
                && self.cfg.linear.review_state.is_some()
            {
                self.enqueue_review(&task)?;
            }
            // A GitHub row that has reached `done` while its issue is still open
            // upstream needs closing, or the next poll undoes it. `is_final`
            // rather than `!= Terminal`: an issue that is *gone* cannot be
            // closed, and asking would retry against a 404 forever.
            //
            // Per repo, not globally: closing an issue on a repo the board is
            // only reading is the write this setting exists to prevent.
            if state == BoardState::Done
                && task.source == Source::Github
                && !task.upstream.is_final()
                && split_gh_task_id(&task.id)
                    .is_some_and(|(repo, _)| self.cfg.github.writeback_for(&repo))
            {
                self.db.enqueue_writeback(&NewWriteback {
                    task_id: task.id.clone(),
                    kind: "close".into(),
                    payload: "{}".into(),
                    idem_key: format!("{}:close", task.id),
                })?;
            }
            self.db.store_derived_state(&task.id, state)?;
        }
        Ok(())
    }

    // ---- writeback ------------------------------------------------------

    /// `via` is the dispatcher already named for a reader — an issue identifier,
    /// or the pane an orchestrator is running in. `None` is the operator, and
    /// says nothing upstream.
    pub fn enqueue_dispatch(
        &self,
        task: &Task,
        runtime: &str,
        workspace: &str,
        attempt_no: usize,
        via: Option<&str>,
    ) -> Result<()> {
        // Name the parent upstream too: reading the Linear issue should tell you
        // an agent released this, not a person.
        self.db.enqueue_writeback(&NewWriteback {
            task_id: task.id.clone(),
            kind: "dispatch".into(),
            payload: json!({
                "runtime": runtime,
                "workspace": workspace,
                "attempt": attempt_no,
                "via": via,
            })
            .to_string(),
            idem_key: format!("{}:dispatch:{}", task.id, attempt_no),
        })?;
        Ok(())
    }

    /// Tell Linear the work is finished and waiting on a human.
    ///
    /// Keyed by attempt so a retry can move the ticket back out of review and in
    /// again: dispatch sends it to In Progress, and the attempt that follows has
    /// its own review transition to make.
    fn enqueue_review(&self, task: &Task) -> Result<()> {
        let Some(want) = self.cfg.linear.review_state.as_deref() else {
            return Ok(());
        };
        // Already there — nothing to say. Usually because we moved it on an
        // earlier tick, sometimes because the operator did it by hand; either
        // way a mutation that changes nothing is not worth sending.
        if task
            .source_state
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(want))
        {
            return Ok(());
        }
        self.db.enqueue_writeback(&NewWriteback {
            task_id: task.id.clone(),
            kind: "review".into(),
            payload: json!({ "state": want }).to_string(),
            idem_key: format!("{}:review:{}", task.id, task.attempt_count()),
        })?;
        Ok(())
    }

    pub fn enqueue_outcome(
        &self,
        task: &Task,
        outcome: Outcome,
        pr_url: Option<&str>,
    ) -> Result<()> {
        let attempt_no = task.attempt_count();
        self.db.enqueue_writeback(&NewWriteback {
            task_id: task.id.clone(),
            kind: "outcome".into(),
            payload: json!({
                "outcome": outcome.as_str(),
                "pr_url": pr_url,
                "log": self.paths.logfile().to_string_lossy(),
                "attempt": attempt_no,
            })
            .to_string(),
            idem_key: format!("{}:outcome:{}:{}", task.id, attempt_no, outcome.as_str()),
        })?;
        Ok(())
    }

    /// Drain the queue. Failures back off exponentially, capped at 5 minutes,
    /// and drain on their own once the source returns.
    ///
    /// A writeback leaves the queue two ways: delivered to the source, or
    /// dropped because there is no longer anything upstream to deliver it to
    /// (AGE-6). Both are final — the difference is only what the log says.
    pub fn drain_writebacks(&self) {
        let pending = match self.db.pending_writebacks(20) {
            Ok(p) => p,
            Err(e) => {
                self.log.error(format!("reading writeback queue: {e}"));
                return;
            }
        };
        for w in pending {
            match self.deliver(&w) {
                Ok(sent) => {
                    let _ = self.db.mark_writeback_done(w.id);
                    let _ = self
                        .db
                        .meta_set(&meta::writeback_at(&w.task_id), &crate::db::now());
                    // Both leave the queue; only one of them reached a source,
                    // and a log that called the other "delivered" would be a
                    // lie an operator has no way to check.
                    match sent {
                        Sent::Upstream => self
                            .log
                            .info(format!("writeback {} delivered ({})", w.idem_key, w.kind)),
                        Sent::Dropped(why) => self.log.info(format!(
                            "writeback {} ({}) dropped: {why}",
                            w.idem_key, w.kind
                        )),
                    }
                }
                Err(e) => {
                    self.log
                        .warn(format!("writeback {} failed: {e}", w.idem_key));
                    let _ = self.db.defer_writeback(w.id, w.attempts, &e.to_string());
                }
            }
        }
    }

    fn deliver(&self, w: &crate::db::Writeback) -> Result<Sent> {
        let Some(task) = self.db.get_task(&w.task_id)? else {
            // Reaping a never-dispatched task drops its queued writebacks in the
            // same transaction, so this is only reachable if the row went some
            // other way. Either way there is nothing left to say it to.
            return Ok(Sent::Dropped(format!("{} is no longer on the board", w.task_id)));
        };
        if task.upstream == UpstreamState::Gone {
            // The row is kept for its history, but the issue it points at is not
            // there any more. Retrying would only back off forever, so this
            // leaves the queue — as dropped, not as delivered.
            return Ok(Sent::Dropped(format!(
                "{} no longer exists upstream",
                task.identifier
            )));
        }
        let payload: Value = serde_json::from_str(&w.payload).unwrap_or(Value::Null);

        match task.source {
            Source::Linear => {
                let Some(linear) = &self.linear else {
                    anyhow::bail!("no Linear credentials; writeback stays queued");
                };
                match w.kind.as_str() {
                    "dispatch" => {
                        // Move the issue into the team's started-type state.
                        let team = task.identifier.split('-').next().unwrap_or_default();
                        if let Ok(Some(state_id)) = linear.started_state_id(team) {
                            linear.set_state(&task.source_id, &state_id)?;
                        } else {
                            self.log.warn(format!(
                                "no started-type state for team {team}; commenting only"
                            ));
                        }
                        let via = match payload["via"].as_str() {
                            Some(v) => format!(" · dispatched by {v}"),
                            None => String::new(),
                        };
                        linear.comment(
                            &task.source_id,
                            &format!(
                                "Dispatched to herdr · {} · ws:{} · attempt {}{}",
                                payload["runtime"].as_str().unwrap_or("?"),
                                payload["workspace"].as_str().unwrap_or("?"),
                                payload["attempt"].as_u64().unwrap_or(1),
                                via,
                            ),
                        )?;
                    }
                    "outcome" => {
                        let outcome = payload["outcome"].as_str().unwrap_or("done");
                        let pr = payload["pr_url"].as_str();
                        match (outcome, pr) {
                            ("done", Some(url)) => {
                                linear.attach_link(&task.source_id, url, "Pull request")?;
                                linear.comment(
                                    &task.source_id,
                                    &format!("herdr-board: attempt finished · {url}"),
                                )?;
                            }
                            ("done", None) => {
                                linear.comment(
                                    &task.source_id,
                                    "herdr-board: attempt finished with no pull request",
                                )?;
                            }
                            (other, _) => {
                                linear.comment(
                                    &task.source_id,
                                    &format!(
                                        "herdr-board: attempt {} · {} · log: {}",
                                        payload["attempt"].as_u64().unwrap_or(1),
                                        other,
                                        payload["log"].as_str().unwrap_or("(none)"),
                                    ),
                                )?;
                            }
                        }
                    }
                    // The attempt settled with work waiting on a human. Dispatch
                    // moved this issue to In Progress and, before this, nothing
                    // moved it again until a merge — so Linear read In Progress
                    // for the whole review window (AGE-21).
                    "review" => {
                        // Config decides, at delivery: turning the setting off
                        // must stop a transition still sitting in the queue.
                        let Some(want) = self.cfg.linear.review_state.as_deref() else {
                            return Ok(Sent::Dropped(format!(
                                "no [linear] review_state configured ({})",
                                task.identifier
                            )));
                        };
                        let team = task.identifier.split('-').next().unwrap_or_default();
                        match linear.state_id_named(team, want)? {
                            Ok(state_id) => linear.set_state(&task.source_id, &state_id)?,
                            // A named state that does not exist is a config
                            // mistake, not an outage: retrying it against Linear
                            // forever would only bury the reason. `doctor`
                            // checks this name for exactly this reason.
                            Err(have) => {
                                return Ok(Sent::Dropped(format!(
                                    "team {team} has no state named `{want}` (has: {})",
                                    have.join(", ")
                                )));
                            }
                        }
                    }
                    // Merging its pull request finished the work; the ticket is
                    // what is left.
                    "close" => {
                        let team = task.identifier.split('-').next().unwrap_or_default();
                        match linear.completed_state_id(team) {
                            Ok(Some(state_id)) => {
                                linear.set_state(&task.source_id, &state_id)?;
                                linear.comment(
                                    &task.source_id,
                                    "herdr-board: pull request merged",
                                )?;
                            }
                            _ => self.log.warn(format!(
                                "no completed-type state for team {team}; leaving it open"
                            )),
                        }
                    }
                    other => self.log.warn(format!("unknown writeback kind {other}")),
                }
            }
            Source::Github => {
                let Some(gh) = &self.github else {
                    anyhow::bail!("no GitHub client; writeback stays queued");
                };
                let Some((repo, number)) = split_gh_task_id(&task.id) else {
                    self.log
                        .warn(format!("cannot parse a repo out of {}", task.id));
                    return Ok(Sent::Dropped(format!("no repo in {}", task.id)));
                };
                // Config decides at delivery, and it decides per repo: the
                // queue holds effects aimed at several repos at once, and the
                // repo is what says whether this one may land. Turning it off
                // for a repo must also stop what is already queued for it.
                if !self.cfg.github.writeback_for(&repo) {
                    return Ok(Sent::Dropped(format!(
                        "writeback is off for {repo} in routing.toml ({})",
                        task.identifier
                    )));
                }
                match w.kind.as_str() {
                    "dispatch" => {
                        let via = match payload["via"].as_str() {
                            Some(v) => format!(" · dispatched by {v}"),
                            None => String::new(),
                        };
                        gh.comment(
                            &repo,
                            number,
                            &format!(
                                "Dispatched to herdr · {} · ws:{} · attempt {}{}",
                                payload["runtime"].as_str().unwrap_or("?"),
                                payload["workspace"].as_str().unwrap_or("?"),
                                payload["attempt"].as_u64().unwrap_or(1),
                                via,
                            ),
                        )?;
                    }
                    "outcome" => {
                        let outcome = payload["outcome"].as_str().unwrap_or("done");
                        let body = match payload["pr_url"].as_str() {
                            Some(url) => {
                                format!("herdr-board: attempt finished · {url}")
                            }
                            None => format!(
                                "herdr-board: attempt {} · {} · log: {}",
                                payload["attempt"].as_u64().unwrap_or(1),
                                outcome,
                                payload["log"].as_str().unwrap_or("(none)"),
                            ),
                        };
                        gh.comment(&repo, number, &body)?;
                    }
                    // Close on done. This is what makes `d mark done` mean the
                    // same thing on a GitHub row as on a Linear one.
                    "close" => gh.close_issue(&repo, number)?,
                    other => self.log.warn(format!("unknown writeback kind {other}")),
                }
            }
        }
        Ok(Sent::Upstream)
    }

    /// Merge the pull request on a task, if it has one.
    pub fn merge_pull_request(&self, task: &Task) -> Result<String> {
        let Some(number) = task.pr_number else {
            anyhow::bail!("{} has no pull request", task.identifier);
        };
        let Some(gh) = &self.github else {
            anyhow::bail!("no GitHub credentials");
        };
        // The repo comes from the task id for a PR row, or from the PR url for
        // a task whose PR was linked by branch.
        let repo = split_gh_task_id(&task.id)
            .map(|(r, _)| r)
            .or_else(|| {
                let url = task.pr_url.as_deref()?;
                let rest = url.split("github.com/").nth(1)?;
                let mut parts = rest.split('/');
                Some(format!("{}/{}", parts.next()?, parts.next()?))
            })
            .ok_or_else(|| anyhow::anyhow!("cannot tell which repo {} belongs to", task.identifier))?;

        gh.merge_pr(&repo, number)?;
        self.log
            .info(format!("merged {repo}#{number} for {}", task.identifier));

        // Reflect it immediately rather than waiting for a poll: the operator
        // just pressed the key and needs the row to move.
        self.db.set_pr(&task.id, task.pr_url.as_deref(), Some(number), false)?;
        self.db.set_pr_merged(&task.id, true)?;
        self.finish_on_merge(task, &repo, number)?;
        self.rederive_all()?;
        Ok(format!("{repo}#{number}"))
    }

    /// What a merged pull request means for the task that owns it.
    ///
    /// A merged PR is finished work. For a PR row that is the whole task; for
    /// an issue whose PR this was, the work is done and the ticket is what
    /// remains. Shared by `m` and by the poll that notices someone merged
    /// elsewhere, because otherwise the row's state depends on which route the
    /// merge took — which is the whole of AGE-22.
    fn finish_on_merge(&self, task: &Task, repo: &str, number: i64) -> Result<()> {
        self.db.set_local_done(&task.id, true)?;
        let queued = self.db.enqueue_writeback(&NewWriteback {
            task_id: task.id.clone(),
            kind: "close".into(),
            payload: json!({ "reason": "merged" }).to_string(),
            idem_key: format!("{}:close", task.id),
        })?;
        if queued {
            self.log.info(format!(
                "{}: {repo}#{number} merged; queued a close",
                task.identifier
            ));
        }
        Ok(())
    }

    // ---- health for the header -----------------------------------------

    pub fn health(&self, source: Source) -> SourceHealth {
        let (configured, status_key, fail_key) = match source {
            Source::Linear => (
                self.linear.is_some(),
                meta::LINEAR_STATUS,
                meta::LINEAR_FAILURES,
            ),
            Source::Github => (
                self.github.is_some(),
                meta::GITHUB_STATUS,
                meta::GITHUB_FAILURES,
            ),
        };
        // A recorded status means *something* polled this source, even if this
        // process built no client for it — the TUI is a reader, and its own
        // credentials say nothing about whether the daemon has any. Gating on
        // `configured` alone made the board omit `linear ✓` while the daemon
        // was happily polling Linear.
        match self.db.meta_get(status_key).ok().flatten() {
            None if !configured => SourceHealth::Absent,
            Some(s) if s == "ok" => SourceHealth::Ok,
            Some(s) if s.starts_with("error:") => {
                let failures = self
                    .db
                    .meta_get(fail_key)
                    .ok()
                    .flatten()
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(1);
                SourceHealth::Down {
                    error: s.trim_start_matches("error:").to_string(),
                    retry_in: crate::db::backoff_secs(failures),
                }
            }
            _ if configured => SourceHealth::Ok,
            _ => SourceHealth::Absent,
        }
    }
}

/// What `derive_state` gets to see for a task, read off its stored rows.
///
/// Pulled out of the sync cycle so anything that needs the current state without
/// writing it — `gc`, which must not act on a stale `state` column — asks the
/// same question the daemon does, rather than a second approximation of it.
/// `override_status` supplies pane statuses by pane id; empty means "use what
/// reconciliation last stored on the attempt".
pub fn derivation_for(task: &Task, override_status: &HashMap<String, AgentStatus>) -> Derivation {
    let live = task.live_attempt().map(|a| {
        a.pane_id
            .as_deref()
            .and_then(|p| override_status.get(p).copied())
            .or(a.agent_status)
            .unwrap_or(AgentStatus::Unknown)
    });
    let last_outcome = task.last_closed_attempt().and_then(|a| a.outcome);
    Derivation {
        upstream: task.upstream,
        live,
        last_outcome,
        open_pr: task.pr_open,
        local_done: task.local_done,
    }
}

/// herdr caps notification text at 80 characters.
fn truncate_for_toast(s: &str) -> String {
    crate::ui::render::truncate(s, 78)
}

/// The branches the board has dispatched onto, and where.
///
/// A branch name alone is not an identity: the board watches several repos and
/// `board/gh-2` can exist in all of them. A GitHub task's branch is claimed only
/// within its own repository; a Linear task names no repo, so its branch is
/// claimed wherever the pull request appears.
#[derive(Default)]
struct AttemptBranches {
    in_repo: std::collections::HashMap<String, std::collections::HashSet<String>>,
    anywhere: std::collections::HashSet<String>,
}

impl AttemptBranches {
    /// Is this pull request already some attempt's, rather than a row of its
    /// own?
    fn claims(&self, pr: &PullRequest) -> bool {
        self.anywhere.contains(&pr.head_ref)
            || self
                .in_repo
                .get(&pr.repo)
                .is_some_and(|branches| branches.contains(&pr.head_ref))
    }
}

/// `gh:owner/repo#87` → (`owner/repo`, 87). Also accepts the pull-request form
/// `gh:owner/repo!508` — GitHub's issues endpoints serve pull requests too, so
/// comments and closing work the same for both.
pub fn split_gh_task_id(id: &str) -> Option<(String, i64)> {
    let repo = crate::model::gh_repo(id)?;
    let (_, number) = id.rsplit_once(['#', '!'])?;
    Some((repo.to_string(), number.parse().ok()?))
}

/// The route context for a task, from its stored fields.
pub fn route_context(task: &Task) -> RouteContext {
    match task.source {
        Source::Linear => RouteContext {
            // Prefer what Linear told us; fall back to the identifier prefix
            // (`LIN-142` → team key `LIN`) for rows stored before the team was
            // recorded.
            linear_team: task
                .linear_team
                .clone()
                .or_else(|| task.identifier.split('-').next().map(str::to_string)),
            linear_project: task.linear_project.clone(),
            gh_repo: None,
            labels: task.labels.clone(),
        },
        Source::Github => RouteContext {
            linear_team: None,
            linear_project: None,
            gh_repo: crate::model::gh_repo(&task.id).map(str::to_string),
            labels: task.labels.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Defaults;
    use crate::db::UpsertTask;
    use crate::sources::linear::FixtureTransport;

    fn engine(linear: Option<Linear<Box<dyn GraphQl>>>) -> SyncEngine {
        engine_with(linear, None)
    }

    fn engine_with(
        linear: Option<Linear<Box<dyn GraphQl>>>,
        github: Option<Github<Box<dyn Rest>>>,
    ) -> SyncEngine {
        let mut e = engine_inner(linear);
        e.github = github;
        e
    }

    fn engine_inner(linear: Option<Linear<Box<dyn GraphQl>>>) -> SyncEngine {
        let tmp = std::env::temp_dir().join(format!(
            "herdr-board-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        SyncEngine {
            db: Db::open_in_memory().unwrap(),
            cfg: RoutingConfig {
                defaults: Defaults::default(),
                ..Default::default()
            },
            credentials: Default::default(),
            paths: Paths {
                config_dir: tmp.clone(),
                state_dir: tmp,
            },
            log: Arc::new(Logger::new("", false)),
            linear,
            github: None,
        }
    }

    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn seed(e: &SyncEngine, id: &str, identifier: &str, upstream: UpstreamState) {
        e.db.upsert_task(&UpsertTask {
            id: id.into(),
            source: Source::Linear,
            source_id: "uuid-1".into(),
            identifier: identifier.into(),
            title: "Add retry".into(),
            body: None,
            url: "https://linear.app/x".into(),
            labels: vec!["herd".into()],
            source_state: None,
            linear_team: identifier.split('-').next().map(str::to_string),
            linear_project: None,
            upstream,
            updated_at: crate::db::now(),
        })
        .unwrap();
    }

    fn pane(id: &str, status: AgentStatus) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            workspace_id: "w1".into(),
            tab_id: Some("w1:t2".into()),
            agent: Some("claude".into()),
            agent_status: Some(status),
            focused: false,
            label: None,
            cwd: None,
            scroll_offset: 0,
        }
    }

    fn dispatch(e: &SyncEngine, task: &str, pane_id: &str) -> i64 {
        dispatch_by(e, task, pane_id, None)
    }

    /// An attempt on a named branch, for the tests that care which one.
    fn dispatch_on(e: &SyncEngine, task: &str, branch: &str) -> i64 {
        e.db.insert_attempt(&crate::db::NewAttempt {
            task_id: task.into(),
            pane_id: None,
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: Some(branch.into()),
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
        })
        .unwrap()
    }

    /// `from` is the pane that released it — `None` is the operator.
    fn dispatch_by(e: &SyncEngine, task: &str, pane_id: &str, from: Option<&str>) -> i64 {
        let a = e
            .db
            .insert_attempt(&crate::db::NewAttempt {
                task_id: task.into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: None,
                branch: Some("board/lin-142".into()),
                dispatched_by: None,
                dispatched_by_pane: from.map(str::to_string),
                base_sha: None,
            })
            .unwrap();
        e.db.set_attempt_pane(a, pane_id).unwrap();
        a
    }

    #[test]
    fn a_missing_pane_needs_two_ticks_before_it_orphans() {
        // Impl spec §7: avoid flapping during a live handoff.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "w1:p9");

        e.reconcile(&[]).unwrap();
        assert!(
            e.db.attempts_for("linear:LIN-142").unwrap()[0]
                .outcome
                .is_none(),
            "one missing tick must not orphan the attempt"
        );

        e.reconcile(&[]).unwrap();
        assert_eq!(
            e.db.attempts_for("linear:LIN-142").unwrap()[0].outcome,
            Some(Outcome::Orphaned)
        );
        let _ = a;
    }

    #[test]
    fn a_pane_that_comes_back_resets_the_missing_counter() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "w1:p9");

        e.reconcile(&[]).unwrap();
        e.reconcile(&[pane("w1:p9", AgentStatus::Working)]).unwrap();
        assert_eq!(e.db.attempts_for("linear:LIN-142").unwrap()[0].missing_ticks, 0);

        // Having survived a handoff, it takes two fresh ticks again.
        e.reconcile(&[]).unwrap();
        assert!(
            e.db.attempts_for("linear:LIN-142").unwrap()[0]
                .outcome
                .is_none()
        );
    }

    #[test]
    fn a_settled_agent_without_a_pr_keeps_the_attempt_live() {
        // "only finalize on explicit done detection or user action"
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "w1:p9");
        e.reconcile(&[pane("w1:p9", AgentStatus::Idle)]).unwrap();
        assert!(
            e.db.attempts_for("linear:LIN-142").unwrap()[0]
                .outcome
                .is_none()
        );
    }

    /// An attempt in a real checkout that has committed since it started.
    ///
    /// This is the weak artifact, and the reason it is weak:
    /// `agent-conventions.md` tells dispatched agents to "commit even when you
    /// are not opening a PR", so it is routinely present while the agent is
    /// still mid-turn. Returns the worktree so the caller can clean it up.
    fn dispatch_with_commits(e: &SyncEngine, task: &str, pane_id: &str) -> std::path::PathBuf {
        let work = repo_ahead_of_its_remote();
        let wt = work.to_string_lossy().into_owned();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", &wt])
                .args(args)
                .output()
                .unwrap();
        };
        let base = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["-C", &wt, "rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        let a = e
            .db
            .insert_attempt(&crate::db::NewAttempt {
                task_id: task.into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: Some(wt.clone()),
                branch: Some("board/lin-142".into()),
                dispatched_by: None,
                dispatched_by_pane: None,
                base_sha: Some(base),
            })
            .unwrap();
        e.db.set_attempt_pane(a, pane_id).unwrap();
        // The mid-flight commit the conventions ask for.
        std::fs::write(work.join("wip"), "half a feature").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "wip"]);
        work
    }

    fn live(e: &SyncEngine) -> Attempt {
        e.db.attempts_for("linear:LIN-142").unwrap().remove(0)
    }

    #[test]
    fn commits_alone_need_two_settled_samples_before_the_attempt_closes() {
        // gh#18. `idle` is a screen classification that flaps while an agent
        // works, so one sample of it is not "finished" — and the weak artifact
        // is already there, because the agent was told to commit mid-flight.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let work = dispatch_with_commits(&e, "linear:LIN-142", "w1:p9");

        e.reconcile(&[pane("w1:p9", AgentStatus::Working)]).unwrap();
        e.reconcile(&[pane("w1:p9", AgentStatus::Idle)]).unwrap();
        let a = live(&e);
        assert!(
            a.outcome.is_none(),
            "one idle sample must not close an attempt that is still being worked"
        );
        assert_eq!(a.settled_ticks, 1);

        e.reconcile(&[pane("w1:p9", AgentStatus::Idle)]).unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn an_idle_flap_mid_turn_leaves_the_attempt_running() {
        // The failure this fixes: `pane.agent_status_changed` runs reconcile at
        // the moment of the flap, not on the next 30s sweep, so a working agent
        // that reads as `idle` for one sample used to be declared finished.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let work = dispatch_with_commits(&e, "linear:LIN-142", "w1:p9");

        for status in [
            AgentStatus::Working,
            AgentStatus::Idle,
            AgentStatus::Working,
            AgentStatus::Idle,
            AgentStatus::Working,
        ] {
            e.reconcile(&[pane("w1:p9", status)]).unwrap();
        }
        let a = live(&e);
        assert!(a.outcome.is_none(), "an agent still working must still be working");
        assert_eq!(a.settled_ticks, 0, "going back to work starts the count over");
        assert_eq!(
            e.db.pending_writeback_count().unwrap(),
            0,
            "and nothing was written back upstream"
        );
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_pull_request_settles_on_the_first_sample_that_sees_it() {
        // The artifacts are tiered. A PR is the agent's own declaration that it
        // is finished and cannot exist unless something ran, so it skips the
        // debounce the commit count has to clear.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let work = dispatch_with_commits(&e, "linear:LIN-142", "w1:p9");
        e.reconcile(&[pane("w1:p9", AgentStatus::Working)]).unwrap();
        e.db.set_pr(
            "linear:LIN-142",
            Some("https://github.com/o/r/pull/291"),
            Some(291),
            true,
        )
        .unwrap();

        e.reconcile(&[pane("w1:p9", AgentStatus::Idle)]).unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn notifications_are_opt_out_and_only_fire_when_work_ends() {
        // A toast on every state change is one nobody reads.
        let mut e = engine(None);
        assert!(e.cfg.defaults.notify, "on by default: somebody has to notice");
        e.cfg.defaults.notify = false;
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let t = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        // With no herdr and notify off, this is a no-op rather than a panic.
        e.notify_settled(None, &t, "finished");
    }

    #[test]
    fn a_settled_agent_with_a_pr_closes_the_attempt_as_done() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "w1:p9");
        e.db.set_pr(
            "linear:LIN-142",
            Some("https://github.com/o/r/pull/291"),
            Some(291),
            true,
        )
        .unwrap();
        e.reconcile(&[pane("w1:p9", AgentStatus::Done)]).unwrap();
        assert_eq!(
            e.db.attempts_for("linear:LIN-142").unwrap()[0].outcome,
            Some(Outcome::Done)
        );
        // And the outcome is queued for Linear.
        assert_eq!(e.db.pending_writeback_count().unwrap(), 1);
    }

    /// Telling the dispatcher is a second audience, not a second gate. Whatever
    /// it decides — off, nobody to tell, a pane that will not take a prompt —
    /// the attempt is still closed and the outcome still queued, because those
    /// are what the rest of the board reads.
    #[test]
    fn waking_the_dispatcher_cannot_hold_up_the_settle_itself() {
        let mut e = engine(None);
        assert!(
            !e.cfg.defaults.notify_dispatcher,
            "off by default: an agent woken by every child cannot hold a thought"
        );
        e.cfg.defaults.notify_dispatcher = true;
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_by(&e, "linear:LIN-142", "w1:p9", Some("w1:p4"));
        e.db.set_pr(
            "linear:LIN-142",
            Some("https://github.com/o/r/pull/291"),
            Some(291),
            true,
        )
        .unwrap();

        // There is no herdr here to carry the prompt — the same position the
        // daemon is in when herdr is not reachable.
        e.reconcile(&[pane("w1:p9", AgentStatus::Done)]).unwrap();

        assert_eq!(
            e.db.attempts_for("linear:LIN-142").unwrap()[0].outcome,
            Some(Outcome::Done)
        );
        assert_eq!(e.db.pending_writeback_count().unwrap(), 1);
    }

    #[test]
    fn orphaning_queues_exactly_one_writeback_even_across_ticks() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "w1:p9");
        for _ in 0..5 {
            e.reconcile(&[]).unwrap();
        }
        assert_eq!(e.db.pending_writeback_count().unwrap(), 1);
    }

    #[test]
    fn pull_requests_link_to_tasks_by_attempt_branch() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "w1:p9");
        seed(&e, "linear:LIN-999", "LIN-999", UpstreamState::Started);

        e.link_pull_requests(&[PullRequest {
            repo: "o/r".into(),
            number: 291,
            title: "Add retry".into(),
            body: None,
            url: "https://github.com/o/r/pull/291".into(),
            head_ref: "board/lin-142".into(),
            open: true,
            merged: false,
            draft: false,
            updated_at: crate::db::now(),
        }])
        .unwrap();

        assert!(e.db.get_task("linear:LIN-142").unwrap().unwrap().pr_open);
        // The unrelated task must not pick up the PR.
        assert!(!e.db.get_task("linear:LIN-999").unwrap().unwrap().pr_open);
    }

    #[test]
    fn a_pull_request_never_crosses_into_another_repo() {
        // AGE-20, seen live: gh#2 exists in two repos, both branch to
        // `board/gh-2`, and OIOS's *merged* PR attached itself to tripletex's
        // task — deriving it to review with no work done and sending `o` to the
        // wrong repo entirely.
        let e = engine(None);
        seed(&e, "gh:Florin-AS/tripletex-mcp#2", "gh#2", UpstreamState::Started);
        dispatch(&e, "gh:Florin-AS/tripletex-mcp#2", "w1:p9");

        e.link_pull_requests(&[PullRequest {
            repo: "bredebjorhovd/OIOS".into(),
            number: 10,
            title: "Row-capture sweeps".into(),
            body: None,
            url: "https://github.com/bredebjorhovd/OIOS/pull/10".into(),
            head_ref: "board/gh-2".into(),
            open: false,
            merged: true,
            draft: false,
            updated_at: crate::db::now(),
        }])
        .unwrap();

        let t = e.db.get_task("gh:Florin-AS/tripletex-mcp#2").unwrap().unwrap();
        assert_eq!(t.pr_url, None, "another repo's PR is not this task's PR");
        assert!(!t.pr_merged, "and must not mark it finished");
    }

    #[test]
    fn another_repos_attempt_branch_does_not_swallow_a_pull_request() {
        // The other side of the same ambiguity (AGE-20). A PR on a branch some
        // attempt owns is that attempt's, not a row of its own — but `board/gh-2`
        // in OIOS is not tripletex's attempt merely because the strings match,
        // and treating it as one drops a real pull request off the board with no
        // trace. New branches carry their repo; attempts recorded before this
        // still hold the ambiguous name, which is the case here.
        let mut e = engine_with(
            None,
            Some(Github::new(Box::new(crate::sources::github::FixtureRest::new(
                vec![
                    (
                        "/repos/Florin-AS/tripletex-mcp/issues".into(),
                        json!([{ "number": 2, "node_id": "n1", "title": "Ingest sweeps",
                                 "html_url": "u", "state": "open", "updated_at": "t",
                                 "labels": [] }]),
                    ),
                    ("/repos/Florin-AS/tripletex-mcp/pulls".into(), json!([])),
                    ("/repos/bredebjorhovd/OIOS/issues".into(), json!([])),
                    (
                        "/repos/bredebjorhovd/OIOS/pulls".into(),
                        json!([{ "number": 10, "title": "Row-capture sweeps",
                                 "html_url": "https://github.com/bredebjorhovd/OIOS/pull/10",
                                 "state": "closed", "merged_at": "t", "updated_at": "t",
                                 "head": { "ref": "board/gh-2" } }]),
                    ),
                ],
            )) as Box<dyn Rest>)),
        );
        e.cfg.github.repos = vec![
            "Florin-AS/tripletex-mcp".into(),
            "bredebjorhovd/OIOS".into(),
        ];
        seed(&e, "gh:Florin-AS/tripletex-mcp#2", "gh#2", UpstreamState::Started);
        dispatch_on(&e, "gh:Florin-AS/tripletex-mcp#2", "board/gh-2");

        e.poll_github();

        let pr = e.db.get_task("gh:bredebjorhovd/OIOS!10").unwrap();
        assert!(
            pr.is_some(),
            "OIOS's pull request is nobody's attempt and belongs on the board"
        );
        let t = e.db.get_task("gh:Florin-AS/tripletex-mcp#2").unwrap().unwrap();
        assert_eq!(t.pr_url, None, "and is still not tripletex's");
    }

    /// A merged pull request that the board is only told about by a poll.
    fn merged_pr() -> PullRequest {
        PullRequest {
            repo: "o/r".into(),
            number: 291,
            title: "Add retry".into(),
            body: None,
            url: "https://github.com/o/r/pull/291".into(),
            head_ref: "board/lin-142".into(),
            open: false,
            merged: true,
            draft: false,
            updated_at: crate::db::now(),
        }
    }

    #[test]
    fn a_pull_request_merged_outside_the_board_still_closes_the_ticket() {
        // AGE-22, seen live on AGE-17/18/21: `gh pr merge` and the web UI go
        // nowhere near `m`, so nothing told the tracker and all three sat at
        // In Review until someone closed them by hand.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "w1:p9");

        e.link_pull_requests(&[merged_pr()]).unwrap();

        let t = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        assert!(t.pr_merged);
        // Same row state as `m` produces — the route the merge took must not
        // decide where the row ends up.
        assert!(t.local_done, "observing a merge finishes the work");
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Done
        );
        assert!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .any(|w| w.kind == "close"),
            "and the ticket is queued to be closed"
        );
    }

    #[test]
    fn a_merge_seen_by_the_poll_lets_the_row_leave_review() {
        // The half that made this more than a missed advance. The attempt
        // settled, so derivation reaches `review` off its outcome and keeps
        // reaching it — the ticket is held in the wrong state rather than
        // merely not advanced, which is why closing AGE-17 by hand did not
        // stick. Only something that finishes the task can end that.
        let mut e = engine(None);
        e.cfg.linear.review_state = Some("In Review".into());
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "w1:p9");
        e.db.close_attempt(a, Outcome::Done).unwrap();
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Review
        );

        e.link_pull_requests(&[merged_pr()]).unwrap();
        for _ in 0..3 {
            e.rederive_all().unwrap();
        }
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Done,
            "and it stays out — no tick puts it back in review"
        );
    }

    #[test]
    fn a_merge_observed_on_every_poll_only_closes_the_ticket_once() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "w1:p9");
        for _ in 0..5 {
            e.link_pull_requests(&[merged_pr()]).unwrap();
            e.rederive_all().unwrap();
        }
        assert_eq!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .filter(|w| w.kind == "close")
                .count(),
            1,
        );
    }

    #[test]
    fn a_task_already_finished_upstream_is_not_reopened_by_a_merge() {
        // The issue was closed upstream; a merged PR turning up afterwards has
        // nothing left to say, and closing an already-closed ticket is noise.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Terminal);
        dispatch(&e, "linear:LIN-142", "w1:p9");
        e.rederive_all().unwrap();

        e.link_pull_requests(&[merged_pr()]).unwrap();

        assert!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .all(|w| w.kind != "close"),
        );
        // The PR fact itself is still recorded — only the writeback is skipped.
        assert!(e.db.get_task("linear:LIN-142").unwrap().unwrap().pr_merged);
    }

    #[test]
    fn a_reader_picks_up_a_status_change_without_touching_lifecycle() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "w1:p9");

        assert!(
            e.refresh_agent_status(&[pane("w1:p9", AgentStatus::Blocked)])
                .unwrap()
        );
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Blocked
        );
        // ...and unblocking is picked up the same way.
        assert!(
            e.refresh_agent_status(&[pane("w1:p9", AgentStatus::Working)])
                .unwrap()
        );
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Working
        );
        // An unchanged status is not a write.
        assert!(
            !e.refresh_agent_status(&[pane("w1:p9", AgentStatus::Working)])
                .unwrap()
        );
    }

    #[test]
    fn a_reader_never_orphans_a_missing_pane() {
        // Orphaning is the daemon's call; a reader running this every two
        // seconds would race `missing_ticks` and kill a pane mid-handoff.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "w1:p9");
        for _ in 0..10 {
            e.refresh_agent_status(&[]).unwrap();
        }
        let a = &e.db.attempts_for("linear:LIN-142").unwrap()[0];
        assert_eq!(a.missing_ticks, 0);
        assert!(a.outcome.is_none());
    }

    #[test]
    fn rederive_persists_the_matrix_result() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "w1:p9");
        let mut status = HashMap::new();
        status.insert("w1:p9".to_string(), AgentStatus::Blocked);
        e.rederive_with(&status).unwrap();
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Blocked
        );
    }

    #[test]
    fn a_linear_outage_serves_stale_data_and_marks_the_header() {
        struct Down;
        impl GraphQl for Down {
            fn query(&self, _: &Value) -> Result<Value> {
                anyhow::bail!("connection refused")
            }
        }
        let e = engine(Some(Linear::new(Box::new(Down) as Box<dyn GraphQl>)));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);

        e.poll_linear();

        // The row is still there — never blank the list because a poll failed.
        assert_eq!(e.db.load_tasks().unwrap().len(), 1);
        match e.health(Source::Linear) {
            SourceHealth::Down { error, retry_in } => {
                assert!(error.contains("connection refused"));
                assert!(retry_in > 0 && retry_in <= 300);
            }
            other => panic!("expected Down, got {other:?}"),
        }
    }

    #[test]
    fn backoff_grows_across_consecutive_failures_and_caps() {
        struct Down;
        impl GraphQl for Down {
            fn query(&self, _: &Value) -> Result<Value> {
                anyhow::bail!("nope")
            }
        }
        let e = engine(Some(Linear::new(Box::new(Down) as Box<dyn GraphQl>)));
        e.poll_linear();
        let first = match e.health(Source::Linear) {
            SourceHealth::Down { retry_in, .. } => retry_in,
            _ => unreachable!(),
        };
        for _ in 0..10 {
            e.poll_linear();
        }
        let later = match e.health(Source::Linear) {
            SourceHealth::Down { retry_in, .. } => retry_in,
            _ => unreachable!(),
        };
        assert!(later > first);
        assert_eq!(later, 300, "backoff must cap at 5 minutes");
    }

    #[test]
    fn recovery_clears_the_header_and_the_failure_count() {
        let page = json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } });
        let e = engine(Some(Linear::new(Box::new(FixtureTransport::new(vec![
            page.clone(),
            page,
        ])) as Box<dyn GraphQl>)));
        e.db.meta_set(meta::LINEAR_STATUS, "error:earlier").unwrap();
        e.db.meta_set(meta::LINEAR_FAILURES, "4").unwrap();
        e.poll_linear();
        assert_eq!(e.health(Source::Linear), SourceHealth::Ok);
    }

    #[test]
    fn an_unconfigured_source_is_absent_not_down() {
        // The header must not render `gh ✗` for a source nobody configured.
        let e = engine(None);
        assert_eq!(e.health(Source::Github), SourceHealth::Absent);
    }

    #[test]
    fn a_reader_reports_health_the_daemon_recorded() {
        // The board pane builds no Linear client of its own when it starts
        // before the key exists, but it must still show `linear ✓` once the
        // daemon is polling successfully.
        let e = engine(None);
        assert_eq!(e.health(Source::Linear), SourceHealth::Absent);
        e.db.meta_set(meta::LINEAR_STATUS, "ok").unwrap();
        assert_eq!(e.health(Source::Linear), SourceHealth::Ok);
    }

    #[test]
    fn a_full_sweep_removes_a_task_deleted_upstream() {
        let empty = json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } });
        let e = engine(Some(Linear::new(Box::new(FixtureTransport::new(vec![
            empty.clone(),
            empty,
        ])) as Box<dyn GraphQl>)));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        assert_eq!(e.db.load_tasks().unwrap().len(), 1);

        // No watermark recorded, so this poll is a full sweep.
        e.poll_linear();
        assert!(
            e.db.load_tasks().unwrap().is_empty(),
            "a task the source no longer returns must not linger forever"
        );
    }

    #[test]
    fn a_sweep_never_removes_a_task_with_a_running_agent() {
        let empty = json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } });
        let e = engine(Some(Linear::new(Box::new(FixtureTransport::new(vec![
            empty.clone(),
            empty,
        ])) as Box<dyn GraphQl>)));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "w1:p9");

        e.poll_linear();
        assert_eq!(
            e.db.load_tasks().unwrap().len(),
            1,
            "a row must not vanish from under a live pane"
        );
    }

    /// AGE-6, and the reason it is more than record-keeping: deleting the task
    /// deleted the only row that knew where the attempt's checkout was, and `gc`
    /// then refused to collect a directory it could not attribute. The row stays.
    #[test]
    fn a_sweep_keeps_the_attempts_of_a_task_that_was_worked_on() {
        let empty = json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } });
        let e = engine(Some(Linear::new(Box::new(FixtureTransport::new(vec![
            empty.clone(),
            empty,
        ])) as Box<dyn GraphQl>)));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "w1:p9");
        e.db.conn
            .execute(
                "UPDATE attempts SET worktree = '/wt/lin-142-1' WHERE id = ?1",
                rusqlite::params![a],
            )
            .unwrap();
        // Closed, so the task is reapable at all — a live attempt is protected
        // by a rule of its own.
        e.db.close_attempt(a, Outcome::Done).unwrap();

        e.poll_linear();

        let t = e.db.get_task("linear:LIN-142").unwrap().expect("row kept");
        assert_eq!(t.upstream, UpstreamState::Gone);
        assert_eq!(t.attempts.len(), 1, "the history is the point");
        assert_eq!(t.attempts[0].worktree.as_deref(), Some("/wt/lin-142-1"));
        // And it derives out of the queue, into `done`.
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Done
        );
    }

    #[test]
    fn a_sweep_still_forgets_a_task_nobody_ever_dispatched() {
        // The noise case the old behaviour was right about: created, mislabelled
        // or deleted again, never worked on. Nothing to keep.
        let empty = json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } });
        let e = engine(Some(Linear::new(Box::new(FixtureTransport::new(vec![
            empty.clone(),
            empty,
        ])) as Box<dyn GraphQl>)));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);

        e.poll_linear();
        assert!(e.db.load_tasks().unwrap().is_empty());
    }

    #[test]
    fn a_reaped_row_is_not_reported_again_on_the_next_sweep() {
        let empty = json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } });
        let e = engine(Some(Linear::new(Box::new(FixtureTransport::new(vec![
            empty.clone(),
            empty.clone(),
            empty.clone(),
            empty,
        ])) as Box<dyn GraphQl>)));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "w1:p9");
        e.db.close_attempt(a, Outcome::Done).unwrap();

        e.poll_linear();
        e.db.meta_set(meta::LAST_FULL_SWEEP, "2026-01-01T00:00:00Z")
            .unwrap();
        e.poll_linear();
        assert!(
            e.db.reapable_task_ids(Source::Linear).unwrap().is_empty(),
            "a gone row has nothing left to reap"
        );
    }

    #[test]
    fn an_incremental_poll_does_not_reap() {
        // An incremental response is not the whole set, so absence proves
        // nothing.
        let nodes = json!([{
            "id": "uuid-1", "identifier": "LIN-999", "title": "t", "url": "u",
            "updatedAt": "2026-07-25T18:00:00.000Z",
            "state": { "name": "Todo", "type": "unstarted" },
            "team": { "key": "LIN" }, "labels": { "nodes": [] },
            "attachments": { "nodes": [] }
        }]);
        let e = engine(Some(Linear::new(Box::new(FixtureTransport::new(vec![
            json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": nodes } }),
            json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } }),
        ])) as Box<dyn GraphQl>)));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        // Mark a sweep as just done, so this poll is incremental.
        e.db.meta_set(meta::LAST_FULL_SWEEP, &crate::db::now()).unwrap();
        e.db.meta_set(meta::LINEAR_WATERMARK, "2026-07-01T00:00:00Z")
            .unwrap();

        e.poll_linear();
        assert_eq!(e.db.load_tasks().unwrap().len(), 2, "nothing reaped");
    }

    #[test]
    fn watermark_advances_to_the_newest_issue() {
        let nodes = json!([{
            "id": "uuid-1", "identifier": "LIN-142", "title": "t",
            "url": "u", "updatedAt": "2026-07-25T18:00:00.000Z",
            "state": { "name": "Todo", "type": "unstarted" },
            "team": { "key": "LIN" }, "labels": { "nodes": [] },
            "attachments": { "nodes": [] }
        }]);
        let e = engine(Some(Linear::new(Box::new(FixtureTransport::new(vec![
            json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": nodes } }),
        ])) as Box<dyn GraphQl>)));
        e.poll_linear();
        assert_eq!(
            e.db.meta_get(meta::LINEAR_WATERMARK).unwrap().as_deref(),
            Some("2026-07-25T18:00:00.000Z")
        );
    }

    fn seed_gh(e: &SyncEngine) {
        seed_gh_in(e, "gh:o/r#87");
    }

    /// A GitHub row whose id names the repo it belongs to — which is what
    /// per-repo settings are looked up by, so a test about them needs to choose.
    fn seed_gh_in(e: &SyncEngine, id: &str) {
        let number = id.rsplit('#').next().unwrap_or("0").to_string();
        e.db.upsert_task(&UpsertTask {
            id: id.into(),
            source: Source::Github,
            source_id: format!("n{number}"),
            identifier: format!("gh#{number}"),
            title: "Bug".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: Some("open".into()),
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
    }

    /// An engine with GitHub writeback enabled — it is off by default now, so a
    /// test about writeback has to ask for it.
    fn engine_with_gh_writeback() -> SyncEngine {
        let mut e = engine_with(None, Some(gh_client()));
        e.cfg.github.writeback = true;
        e
    }

    fn gh_client() -> Github<Box<dyn Rest>> {
        Github::new(Box::new(crate::sources::github::FixtureRest::new(vec![]))
            as Box<dyn Rest>)
    }

    #[test]
    fn merging_moves_the_row_without_waiting_for_a_poll() {
        // The operator just pressed a key; the row has to move now, not in
        // thirty seconds.
        let e = engine_with(None, Some(gh_client()));
        seed_gh(&e);
        e.db.set_pr("gh:o/r#87", Some("https://github.com/o/r/pull/87"), Some(87), true)
            .unwrap();
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("gh:o/r#87").unwrap().unwrap().state,
            BoardState::Review
        );

        let task = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        e.merge_pull_request(&task).unwrap();

        let after = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        assert!(!after.pr_open, "the PR is no longer open");
        assert_eq!(after.state, BoardState::Done);
    }

    #[test]
    fn merging_queues_the_ticket_to_be_closed() {
        let e = engine_with(None, Some(gh_client()));
        seed_gh(&e);
        e.db.set_pr("gh:o/r#87", Some("https://github.com/o/r/pull/87"), Some(87), true)
            .unwrap();
        let task = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        e.merge_pull_request(&task).unwrap();
        assert!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .any(|w| w.kind == "close"),
            "merging finished the work; the ticket is what is left"
        );
    }

    #[test]
    fn a_github_outcome_leaves_the_same_trail_as_linear() {
        let e = engine_with(None, Some(gh_client()));
        seed_gh(&e);
        let task = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        e.enqueue_outcome(&task, Outcome::Failed, None).unwrap();
        e.drain_writebacks();
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
        // It was actually delivered, not dropped.
        assert!(
            e.github
                .as_ref()
                .is_some_and(|_| e.db.meta_get(&meta::writeback_at("gh:o/r#87")).unwrap().is_some())
        );
    }

    /// AGE-6. Reaping drops the queued writebacks it can see, but one enqueued
    /// between that sweep and the next drain would otherwise sit there failing
    /// against a deleted issue and backing off forever. It leaves the queue —
    /// and the log calls it dropped rather than delivered, because nothing was.
    #[test]
    fn a_writeback_against_a_gone_task_is_dropped_rather_than_retried_forever() {
        let e = engine_with(None, Some(gh_client()));
        seed_gh(&e);
        let task = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        e.enqueue_outcome(&task, Outcome::Done, None).unwrap();
        e.db.conn
            .execute(
                "UPDATE tasks SET upstream = 'gone' WHERE id = 'gh:o/r#87'",
                [],
            )
            .unwrap();

        let w = e.db.pending_writebacks(1).unwrap().remove(0);
        assert!(matches!(e.deliver(&w).unwrap(), Sent::Dropped(_)));

        e.drain_writebacks();
        assert_eq!(
            e.db.pending_writeback_count().unwrap(),
            0,
            "it must leave the queue, not back off against a 404"
        );
        assert!(
            e.db.meta_get(&meta::writeback_at("gh:o/r#87"))
                .unwrap()
                .is_some(),
            "and it is recorded as handled, so nothing re-queues it"
        );
    }

    #[test]
    fn a_github_row_that_reaches_done_queues_a_close() {
        // Otherwise the next poll recomputes `open` upstream and `d mark done`
        // undoes itself.
        let e = engine_with_gh_writeback();
        seed_gh(&e);
        e.db.set_local_done("gh:o/r#87", true).unwrap();
        e.rederive_all().unwrap();

        assert_eq!(
            e.db.get_task("gh:o/r#87").unwrap().unwrap().state,
            BoardState::Done
        );
        let pending = e.db.pending_writebacks(10).unwrap();
        assert!(
            pending.iter().any(|w| w.kind == "close"),
            "a close was not queued: {pending:?}"
        );
    }

    #[test]
    fn the_close_is_queued_once_however_many_times_we_rederive() {
        let e = engine_with_gh_writeback();
        seed_gh(&e);
        e.db.set_local_done("gh:o/r#87", true).unwrap();
        for _ in 0..5 {
            e.rederive_all().unwrap();
        }
        assert_eq!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .filter(|w| w.kind == "close")
                .count(),
            1
        );
    }

    #[test]
    fn an_already_closed_issue_is_not_closed_again() {
        let e = engine_with_gh_writeback();
        e.db.upsert_task(&UpsertTask {
            id: "gh:o/r#88".into(),
            source: Source::Github,
            source_id: "n2".into(),
            identifier: "gh#88".into(),
            title: "Bug".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: Some("closed".into()),
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Terminal,
            updated_at: crate::db::now(),
        })
        .unwrap();
        e.rederive_all().unwrap();
        assert!(
            !e.db
                .pending_writebacks(10)
                .unwrap()
                .iter()
                .any(|w| w.kind == "close")
        );
    }

    #[test]
    fn writeback_is_off_unless_asked_for() {
        // Pointing the board at a repo is not the same as asking it to write to
        // your issues.
        let e = engine_with(None, Some(gh_client()));
        assert!(!e.cfg.github.writeback, "writeback must default to off");
        seed_gh(&e);
        e.db.set_local_done("gh:o/r#87", true).unwrap();
        e.rederive_all().unwrap();
        // The row still moves locally; nothing is sent upstream.
        assert_eq!(
            e.db.get_task("gh:o/r#87").unwrap().unwrap().state,
            BoardState::Done
        );
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
    }

    // ---- AGE-23: one writeback flag cannot answer for every repo ---------

    /// Writeback on globally, off for the production repo — the config the
    /// ticket asks to be possible.
    fn engine_with_one_read_only_repo() -> SyncEngine {
        let mut e = engine_with(None, Some(gh_client()));
        e.cfg.github.repos = vec!["bredebjorhovd/OIOS".into(), "Florin-AS/Tally".into()];
        e.cfg.github.writeback = true;
        e.cfg.github.per_repo = vec![crate::config::RepoConfig {
            name: "Florin-AS/Tally".into(),
            labels: None,
            writeback: Some(false),
        }];
        e.cfg
            .check()
            .expect("the config the operator would write must validate");
        e
    }

    #[test]
    fn a_read_only_repo_queues_no_close_while_the_others_still_do() {
        // The bug: the flag was set by its riskiest repo, so wanting the trail
        // on OIOS meant accepting it on Tally, and refusing it on Tally meant
        // the board could close nothing anywhere.
        let e = engine_with_one_read_only_repo();
        seed_gh_in(&e, "gh:bredebjorhovd/OIOS#12");
        seed_gh_in(&e, "gh:Florin-AS/Tally#34");
        e.db.set_local_done("gh:bredebjorhovd/OIOS#12", true).unwrap();
        e.db.set_local_done("gh:Florin-AS/Tally#34", true).unwrap();
        e.rederive_all().unwrap();

        // Both rows still move locally: the board's own view is not what the
        // setting is about.
        for id in ["gh:bredebjorhovd/OIOS#12", "gh:Florin-AS/Tally#34"] {
            assert_eq!(
                e.db.get_task(id).unwrap().unwrap().state,
                BoardState::Done,
                "{id} did not reach done locally"
            );
        }
        let queued: Vec<String> = e
            .db
            .pending_writebacks(10)
            .unwrap()
            .into_iter()
            .filter(|w| w.kind == "close")
            .map(|w| w.task_id)
            .collect();
        assert_eq!(queued, ["gh:bredebjorhovd/OIOS#12"], "{queued:?}");
    }

    #[test]
    fn a_comment_aimed_at_a_read_only_repo_is_dropped_at_delivery() {
        // Config decides at delivery, and now it decides per repo: a dispatch
        // comment queued while the flag was global must not land on Tally once
        // the repo says otherwise.
        let e = engine_with_one_read_only_repo();
        seed_gh_in(&e, "gh:Florin-AS/Tally#34");
        let task = e.db.get_task("gh:Florin-AS/Tally#34").unwrap().unwrap();
        e.enqueue_outcome(&task, Outcome::Done, None).unwrap();

        let w = e.db.pending_writebacks(1).unwrap().remove(0);
        match e.deliver(&w).unwrap() {
            Sent::Dropped(why) => assert!(
                why.contains("Florin-AS/Tally"),
                "the reason has to name the repo that refused it: {why}"
            ),
            other => panic!("it was not dropped: {other:?}"),
        }

        e.drain_writebacks();
        assert_eq!(
            e.db.pending_writeback_count().unwrap(),
            0,
            "a dropped writeback leaves the queue rather than backing off"
        );
    }

    #[test]
    fn a_repo_can_be_written_to_while_the_global_flag_stays_off() {
        // The override goes both ways. Otherwise the safe global default can
        // only be escaped by turning every repo on at once — the same bug.
        let mut e = engine_with(None, Some(gh_client()));
        e.cfg.github.repos = vec!["bredebjorhovd/herdr-board".into()];
        e.cfg.github.per_repo = vec![crate::config::RepoConfig {
            name: "bredebjorhovd/herdr-board".into(),
            labels: None,
            writeback: Some(true),
        }];
        e.cfg.check().unwrap();
        assert!(!e.cfg.github.writeback, "the global flag is still off");

        seed_gh_in(&e, "gh:bredebjorhovd/herdr-board#7");
        e.db.set_local_done("gh:bredebjorhovd/herdr-board#7", true)
            .unwrap();
        e.rederive_all().unwrap();
        assert!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .any(|w| w.kind == "close"),
            "the repo asked for the trail and did not get it"
        );
    }

    // ---- AGE-28: one label filter cannot answer for every repo -----------

    /// Records the paths asked for, from outside the `Box<dyn Rest>` the engine
    /// holds — which is the only place the label filter is observable.
    struct Recorder(Arc<std::sync::Mutex<Vec<String>>>);

    impl Rest for Recorder {
        fn get(&self, path: &str) -> Result<Value> {
            self.0.lock().unwrap().push(path.to_string());
            Ok(json!([]))
        }
        fn post(&self, _: &str, _: &Value) -> Result<Value> {
            Ok(Value::Null)
        }
        fn patch(&self, _: &str, _: &Value) -> Result<Value> {
            Ok(Value::Null)
        }
        fn put(&self, _: &str, _: &Value) -> Result<Value> {
            Ok(Value::Null)
        }
    }

    #[test]
    fn each_repo_is_polled_for_its_own_labels() {
        // The bug: `[github] labels = []` is right for a curated tracker and a
        // backlog dump for the repo next to it, and there was no way to say so.
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut e = engine_with(
            None,
            Some(Github::new(
                Box::new(Recorder(asked.clone())) as Box<dyn Rest>
            )),
        );
        e.cfg.github.repos = vec!["Florin-AS/Tally".into(), "b/itsm-agent".into()];
        e.cfg.github.labels = vec![];
        e.cfg.github.per_repo = vec![crate::config::RepoConfig {
            name: "b/itsm-agent".into(),
            labels: Some(vec!["release-a".into()]),
            writeback: None,
        }];
        e.cfg
            .check()
            .expect("the config the operator would write must validate");

        e.poll_github();

        let asked = asked.lock().unwrap().clone();
        let queries: Vec<&String> = asked.iter().filter(|p| p.contains("/issues?")).collect();
        assert_eq!(queries.len(), 2, "{asked:?}");
        assert!(
            !queries[0].contains("labels="),
            "Tally asked for a filter it never configured: {}",
            queries[0]
        );
        assert!(
            queries[1].contains("labels=release-a"),
            "itsm-agent's whole backlog would arrive: {}",
            queries[1]
        );
    }

    // ---- AGE-21: Linear has to be told the work is waiting on a human ----

    /// One page of workflow states, as `state_id_named` reads them. `In Review`
    /// and `In Progress` are both `started`, which is the whole problem.
    fn states_page() -> Value {
        json!({
            "teams": { "nodes": [ { "id": "team-1", "states": { "nodes": [
                { "id": "s-rev",  "name": "In Review",   "type": "started",   "position": 2.0 },
                { "id": "s-prog", "name": "In Progress", "type": "started",   "position": 1.0 }
            ] } } ] }
        })
    }

    /// A fixture transport the test can still read after the engine has boxed
    /// it away, so what was actually sent to Linear can be asserted on.
    struct Shared(std::rc::Rc<FixtureTransport>);

    impl GraphQl for Shared {
        fn query(&self, body: &Value) -> Result<Value> {
            self.0.query(body)
        }
    }

    /// A Linear engine that has been told which state means review.
    fn engine_with_review_state(responses: Vec<Value>) -> SyncEngine {
        recording_engine(responses).0
    }

    fn recording_engine(responses: Vec<Value>) -> (SyncEngine, std::rc::Rc<FixtureTransport>) {
        let transport = std::rc::Rc::new(FixtureTransport::new(responses));
        let mut e = engine(Some(Linear::new(
            Box::new(Shared(transport.clone())) as Box<dyn GraphQl>
        )));
        e.cfg.linear.review_state = Some("In Review".into());
        (e, transport)
    }

    /// A Linear row with an open pull request — the board derives `review`.
    fn seed_in_review(e: &SyncEngine) {
        seed(e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        e.db.set_pr(
            "linear:LIN-142",
            Some("https://github.com/o/r/pull/291"),
            Some(291),
            true,
        )
        .unwrap();
    }

    #[test]
    fn a_row_that_reaches_review_queues_the_linear_transition() {
        // AGE-17 and AGE-18 both read In Progress in Linear for the whole time
        // their PRs sat waiting: dispatch moved them, and nothing moved them
        // again until a merge.
        let e = engine_with_review_state(vec![]);
        seed_in_review(&e);
        e.rederive_all().unwrap();

        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Review
        );
        assert!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .any(|w| w.kind == "review"),
            "reaching review must tell Linear, not only the board"
        );
    }

    #[test]
    fn with_no_review_state_configured_linear_is_left_where_it_was() {
        // The default. Linear has no review state *type*, so with nothing named
        // there is no correct target — and a workspace without such a state must
        // keep behaving exactly as it did.
        let e = engine(None);
        assert!(e.cfg.linear.review_state.is_none(), "unset by default");
        seed_in_review(&e);
        e.rederive_all().unwrap();

        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Review
        );
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
    }

    #[test]
    fn the_transition_is_queued_once_however_many_times_we_rederive() {
        let e = engine_with_review_state(vec![]);
        seed_in_review(&e);
        for _ in 0..5 {
            e.rederive_all().unwrap();
        }
        assert_eq!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .filter(|w| w.kind == "review")
                .count(),
            1
        );
    }

    #[test]
    fn a_retry_gets_a_transition_of_its_own() {
        // Dispatching again moves the ticket back to In Progress, so the attempt
        // that follows has its own review to announce.
        let e = engine_with_review_state(vec![]);
        seed_in_review(&e);
        e.rederive_all().unwrap();
        let first = e.db.pending_writebacks(10).unwrap();

        let a = dispatch(&e, "linear:LIN-142", "w1:p9");
        e.db.close_attempt(a, Outcome::Done).unwrap();
        e.rederive_all().unwrap();

        let after = e.db.pending_writebacks(10).unwrap();
        assert_eq!(
            after.iter().filter(|w| w.kind == "review").count(),
            2,
            "queued: {:?} then {:?}",
            first.iter().map(|w| &w.idem_key).collect::<Vec<_>>(),
            after.iter().map(|w| &w.idem_key).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_ticket_already_in_the_review_state_is_not_moved_again() {
        // The operator dragged it there themselves, or an earlier tick did. A
        // mutation that changes nothing is still a write to somebody's tracker.
        let e = engine_with_review_state(vec![]);
        seed_in_review(&e);
        e.db.conn
            .execute(
                "UPDATE tasks SET source_state = 'In Review' WHERE id = 'linear:LIN-142'",
                [],
            )
            .unwrap();
        e.rederive_all().unwrap();
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
    }

    #[test]
    fn a_closed_issue_is_never_dragged_back_into_review() {
        // `d mark done` derives Done, not Review, so the case that matters is an
        // issue closed upstream while its PR is still open.
        let e = engine_with_review_state(vec![]);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Terminal);
        e.db.set_pr("linear:LIN-142", Some("u"), Some(291), true)
            .unwrap();
        e.rederive_all().unwrap();
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
    }

    #[test]
    fn delivering_the_transition_sets_the_named_state() {
        let (e, transport) = recording_engine(vec![
            states_page(),
            json!({ "issueUpdate": { "success": true } }),
        ]);
        seed_in_review(&e);
        e.rederive_all().unwrap();
        e.drain_writebacks();

        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
        let sent = transport.sent.borrow();
        // Not the lowest-position `started` state, which is In Progress and is
        // where dispatch already put it.
        assert_eq!(sent[1]["variables"]["stateId"], json!("s-rev"));
        assert_eq!(sent[1]["variables"]["id"], json!("uuid-1"));
    }

    #[test]
    fn a_review_state_that_does_not_exist_is_dropped_rather_than_retried() {
        // A name that resolves to nothing is a config mistake, and doctor is
        // where it gets reported. Backing off against Linear forever would only
        // bury it.
        let e = engine_with_review_state(vec![json!({
            "teams": { "nodes": [ { "id": "team-1", "states": { "nodes": [
                { "id": "s-prog", "name": "Pågår", "type": "started", "position": 1.0 }
            ] } } ] }
        })]);
        seed_in_review(&e);
        e.rederive_all().unwrap();

        let w = e.db.pending_writebacks(10).unwrap();
        let w = w.iter().find(|w| w.kind == "review").unwrap();
        assert!(matches!(e.deliver(w).unwrap(), Sent::Dropped(_)));
    }

    #[test]
    fn turning_the_setting_off_stops_a_transition_still_in_the_queue() {
        let mut e = engine_with_review_state(vec![]);
        seed_in_review(&e);
        e.rederive_all().unwrap();
        e.cfg.linear.review_state = None;

        let w = e.db.pending_writebacks(10).unwrap();
        let w = w.iter().find(|w| w.kind == "review").unwrap();
        assert!(matches!(e.deliver(w).unwrap(), Sent::Dropped(_)));
    }

    /// GitHub has no equivalent gap to close. An issue there is open or closed —
    /// there is no state between the two to advance to — and the `outcome`
    /// writeback already comments the PR link on the issue when an attempt
    /// settles, which is the whole of what GitHub can be told. So a repo that
    /// turns writeback on gets the same information Linear now gets; it just
    /// arrives as a comment rather than a state.
    #[test]
    fn a_github_row_reaching_review_has_nothing_to_transition() {
        let mut e = engine_with_gh_writeback();
        e.cfg.linear.review_state = Some("In Review".into());
        seed_gh(&e);
        e.db.set_pr("gh:o/r#87", Some("https://github.com/o/r/pull/87"), Some(87), true)
            .unwrap();
        e.rederive_all().unwrap();

        assert_eq!(
            e.db.get_task("gh:o/r#87").unwrap().unwrap().state,
            BoardState::Review
        );
        assert_eq!(
            e.db.pending_writeback_count().unwrap(),
            0,
            "a Linear state name says nothing about a GitHub issue"
        );
    }

    #[test]
    fn github_task_ids_split_into_repo_and_number() {
        assert_eq!(
            split_gh_task_id("gh:offhand/tally#87"),
            Some(("offhand/tally".into(), 87))
        );
        // Pull requests too: GitHub's issues endpoints serve both.
        assert_eq!(
            split_gh_task_id("gh:offhand/tally!508"),
            Some(("offhand/tally".into(), 508))
        );
        assert_eq!(split_gh_task_id("linear:LIN-142"), None);
    }

    #[test]
    fn a_pull_request_row_routes_like_its_repo() {
        let e = engine(None);
        e.db.upsert_task(&UpsertTask {
            id: "gh:owner/repo!508".into(),
            source: Source::Github,
            source_id: "u".into(),
            identifier: "gh!508".into(),
            title: "t".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: Some("open".into()),
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let t = e.db.get_task("gh:owner/repo!508").unwrap().unwrap();
        assert_eq!(route_context(&t).gh_repo.as_deref(), Some("owner/repo"));
    }

    #[test]
    fn route_context_derives_team_from_a_linear_identifier() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let t = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        let ctx = route_context(&t);
        assert_eq!(ctx.linear_team.as_deref(), Some("LIN"));
        assert_eq!(ctx.labels, vec!["herd"]);
    }

    #[test]
    fn route_context_derives_repo_from_a_github_id() {
        let e = engine(None);
        e.db.upsert_task(&UpsertTask {
            id: "gh:owner/repo#87".into(),
            source: Source::Github,
            source_id: "n1".into(),
            identifier: "gh#87".into(),
            title: "Bug".into(),
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
        let t = e.db.get_task("gh:owner/repo#87").unwrap().unwrap();
        assert_eq!(route_context(&t).gh_repo.as_deref(), Some("owner/repo"));
    }

    /// A repo whose default branch is ahead of its remote, which is what made
    /// every dispatch into tripletex-mcp complete instantly (AGE-19).
    ///
    /// Builds a real repo with an "origin" it is one commit ahead of, then asks
    /// the same question two ways: against the remote, and against the commit
    /// the attempt actually started from.
    fn repo_ahead_of_its_remote() -> std::path::PathBuf {
        use std::process::Command;
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "hb-ahead-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let remote = root.join("remote.git");
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&remote)
            .output()
            .unwrap();
        git(&work, &["init", "-b", "main"]);
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "user.name", "t"]);
        std::fs::write(work.join("a"), "1").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "base"]);
        git(&work, &["remote", "add", "origin", &remote.to_string_lossy()]);
        git(&work, &["push", "-u", "origin", "main"]);
        // The operator's own unpushed commit — the whole point.
        std::fs::write(work.join("b"), "2").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "operator's unpushed work"]);
        work
    }

    #[test]
    fn reconciling_without_a_herdr_handle_cannot_wake_anyone() {
        // The event-driven reconcile passed `None` and therefore woke nobody,
        // for a day, silently — while the 30s poll, which does pass a handle,
        // almost never got there first. `pane.agent_status_changed` fires on
        // exactly the transition that means "an agent finished".
        //
        // Pins the shape rather than the plumbing: the no-handle path must not
        // be reachable from a caller that has one.
        let src = include_str!("cli.rs");
        let reconcile_once = src
            .split("pub fn reconcile_once")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("reconcile_once exists");
        assert!(
            reconcile_once.contains("reconcile_with(&panes, Some(&herdr))"),
            "reconcile_once has a herdr handle and must pass it, or a settle \
             noticed here wakes nobody"
        );
    }

    #[test]
    fn a_retry_does_not_inherit_the_cancelled_runs_commits() {
        // herdr-board#10, seen live on gh#71: cancelled after 10 minutes and
        // four commits, re-dispatched onto the same branch, marked `done` 62
        // seconds later while its agent was still working. The base recorded
        // before dispatch is the *repo* HEAD, and the reused branch was already
        // four commits ahead of it.
        let work = repo_ahead_of_its_remote();
        let e = engine(None);
        let wt = work.to_string_lossy().into_owned();
        let sha = |r: &str| {
            String::from_utf8_lossy(
                &std::process::Command::new("git")
                    .args(["-C", &wt, "rev-parse", r])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .trim()
            .to_string()
        };

        // What the first attempt started from, and what it left behind.
        let repo_head_at_first_dispatch = sha("HEAD");
        std::fs::write(work.join("first"), "1").unwrap();
        for args in [
            ["-C", &wt, "add", "."].as_slice(),
            ["-C", &wt, "commit", "-m", "the cancelled run's work"].as_slice(),
        ] {
            std::process::Command::new("git").args(args).output().unwrap();
        }

        // The retry: same checkout, and the branch tip is now the honest base.
        let base_for_retry = sha("HEAD");
        assert_ne!(base_for_retry, repo_head_at_first_dispatch);
        assert!(
            !e.attempt_has_commits(Some(&wt), Some(&base_for_retry)),
            "a retry that has committed nothing yet must not look finished"
        );
        assert!(
            e.attempt_has_commits(Some(&wt), Some(&repo_head_at_first_dispatch)),
            "measuring from the repo HEAD is what made it look finished — pinned              so the regression is recognisable if it returns"
        );
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn an_operators_unpushed_commit_is_not_the_agents_output() {
        let work = repo_ahead_of_its_remote();
        let e = engine(None);
        let wt = work.to_string_lossy().into_owned();

        // Where a dispatch would start from: the repo's HEAD right now.
        let base = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["-C", &wt, "rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        assert!(
            !e.attempt_has_commits(Some(&wt), Some(&base)),
            "an agent that has committed nothing since it started must not look finished"
        );

        // And it still notices real work.
        std::fs::write(work.join("c"), "3").unwrap();
        for args in [
            ["-C", &wt, "add", "."].as_slice(),
            ["-C", &wt, "commit", "-m", "the agent's work"].as_slice(),
        ] {
            std::process::Command::new("git").args(args).output().unwrap();
        }
        assert!(
            e.attempt_has_commits(Some(&wt), Some(&base)),
            "a commit made after the attempt started is the agent's output"
        );
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn without_a_base_the_old_remote_relative_count_is_what_misfires() {
        // Pins the reason base_sha exists: the fallback path reports "the agent
        // produced something" for a repo where nothing has been dispatched at
        // all. Attempts predating the column keep this weaker behaviour, so it
        // is documented rather than fixed.
        let work = repo_ahead_of_its_remote();
        let e = engine(None);
        assert!(
            e.attempt_has_commits(Some(&work.to_string_lossy()), None),
            "the remote-relative count is fooled by unpushed work — this is the bug"
        );
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }
}
