//! The sync cycle: poll sources, reconcile panes, derive state, drain
//! writebacks (impl spec §4, §6, §7).

use crate::config::{Paths, RouteContext, RoutingConfig};
use crate::db::{Db, NewWriteback};
use crate::herdr::{Herdr, PaneInfo};
use crate::log::Logger;
use crate::model::*;
use crate::sources::github::{Github, PullRequest, Rest, pr_matches_branch};
use crate::sources::linear::{GraphQl, Linear};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;
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
}

/// Health of one upstream source, rendered in the board header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceHealth {
    /// Not configured — the header omits it entirely.
    Absent,
    Ok,
    Down { error: String, retry_in: u64 },
}

pub struct SyncEngine {
    pub db: Db,
    pub cfg: RoutingConfig,
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
}

impl SyncEngine {
    /// One full cycle. Never returns `Err` for a source outage — a poll failure
    /// marks the header and serves stale data (impl spec §7).
    pub fn sync_once(&self, herdr: Option<&Herdr>) -> Result<()> {
        self.poll_linear();
        self.poll_github();

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
            self.reconcile(panes)?;
        }

        self.rederive_all()?;
        self.drain_writebacks();
        self.db.meta_set(meta::LAST_SYNC, &crate::db::now())?;
        Ok(())
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

    /// Forget tasks the source no longer returns.
    ///
    /// A task with a live attempt is left alone: an agent is working on it, and
    /// the row vanishing underneath a running pane would be worse than a stale
    /// row. Reconciliation will orphan it if the pane dies.
    fn reap_missing(&self, source: Source, seen: &std::collections::HashSet<String>) {
        let Ok(known) = self.db.task_ids_for_source(source) else {
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
            match self.db.delete_task(&id) {
                Ok(()) => self
                    .log
                    .info(format!("{id} no longer exists upstream — removed")),
                Err(e) => self.log.error(format!("removing {id}: {e}")),
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

        let result = linear
            .fetch_board_issues(&self.cfg.sync.labels, watermark.as_deref())
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

    fn poll_github(&self) {
        let Some(gh) = &self.github else {
            return;
        };
        if self.cfg.github.repos.is_empty() {
            return;
        }
        let mut all_pulls: Vec<PullRequest> = Vec::new();
        let mut failed: Option<String> = None;
        // GitHub is always polled in full, so every cycle can reap.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for repo in &self.cfg.github.repos {
            match gh.issues(repo, &self.cfg.github.labels) {
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
        let attempt_branches: std::collections::HashSet<String> = self
            .db
            .load_tasks()
            .unwrap_or_default()
            .iter()
            .flat_map(|t| t.attempts.iter().filter_map(|a| a.branch.clone()))
            .collect();

        if let Err(e) = self.link_pull_requests(&all_pulls) {
            self.log.error(format!("linking PRs: {e}"));
        }

        if self.cfg.github.pull_requests {
            for pr in &all_pulls {
                if attempt_branches.contains(&pr.head_ref) {
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
    }

    /// Attach PRs to tasks by attempt branch (`board/<identifier>`), which is
    /// the link the dispatcher creates.
    pub fn link_pull_requests(&self, pulls: &[PullRequest]) -> Result<()> {
        if pulls.is_empty() {
            return Ok(());
        }
        for task in self.db.load_tasks()? {
            let branches: Vec<String> = task
                .attempts
                .iter()
                .filter_map(|a| a.branch.clone())
                .collect();
            let Some(pr) = pulls
                .iter()
                .find(|p| branches.iter().any(|b| pr_matches_branch(p, b)))
            else {
                continue;
            };
            self.db
                .set_pr(&task.id, Some(&pr.url), Some(pr.number), pr.open)?;
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

    /// Map live attempts onto herdr's current pane reality (impl spec §6).
    pub fn reconcile(&self, panes: &[PaneInfo]) -> Result<()> {
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
                        self.db.close_attempt(attempt.id, Outcome::Orphaned)?;
                        self.enqueue_outcome(&task, Outcome::Orphaned, None)?;
                    } else {
                        self.log.info(format!(
                            "{} pane {} missing (tick {}/2)",
                            task.identifier, pane_id, ticks
                        ));
                        self.db.set_missing_ticks(attempt.id, ticks)?;
                    }
                }
                Some(pane) => {
                    if attempt.missing_ticks != 0 {
                        // It came back — a handoff, not a death.
                        self.db.set_missing_ticks(attempt.id, 0)?;
                    }
                    let status = pane.agent_status.unwrap_or(AgentStatus::Unknown);
                    if status == AgentStatus::Unknown {
                        // Worth a line: `unknown` is not proof of completion,
                        // and the agent name says what herdr actually saw.
                        self.log.info(format!(
                            "{} pane {} agent {:?} is unclassified",
                            task.identifier, pane_id, pane.agent
                        ));
                    }
                    // Persist it so the TUI can render the dim `idle` marker
                    // without shelling out to herdr on its own tick.
                    self.db.set_attempt_status(attempt.id, status)?;
                    // An agent that has settled *and* produced a PR is the only
                    // explicit done detection we have. Without a PR the attempt
                    // stays live and the row renders a dim `idle` marker.
                    let settled = matches!(
                        status,
                        AgentStatus::Idle | AgentStatus::Done | AgentStatus::Unknown
                    );
                    if settled && task.pr_open {
                        self.log.info(format!(
                            "{} agent {} with PR — attempt done",
                            task.identifier, status.as_str()
                        ));
                        self.db.close_attempt(attempt.id, Outcome::Done)?;
                        self.enqueue_outcome(
                            &task,
                            Outcome::Done,
                            task.pr_url.as_deref(),
                        )?;
                    }
                }
            }
        }
        Ok(())
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

    /// Recompute and persist every task's derived state, using the agent status
    /// reconciliation last stored on each live attempt.
    pub fn rederive_all(&self) -> Result<()> {
        self.rederive_with(&HashMap::new())
    }

    /// `override_status` lets a caller (and the tests) supply pane statuses
    /// directly; otherwise the value stored on the attempt is used.
    pub fn rederive_with(&self, override_status: &HashMap<String, AgentStatus>) -> Result<()> {
        for task in self.db.load_tasks()? {
            let live = task.live_attempt().map(|a| {
                a.pane_id
                    .as_deref()
                    .and_then(|p| override_status.get(p).copied())
                    .or(a.agent_status)
                    .unwrap_or(AgentStatus::Unknown)
            });
            let last_outcome = task
                .attempts
                .iter()
                .rev()
                .find(|a| a.outcome.is_some())
                .and_then(|a| a.outcome);
            let state = derive_state(Derivation {
                upstream: task.upstream,
                live,
                last_outcome,
                open_pr: task.pr_open,
                local_done: task.local_done,
            });
            if state != task.state {
                self.log
                    .info(format!("{}: {} → {}", task.identifier, task.state, state));
            }
            // A GitHub row that has reached `done` while its issue is still open
            // upstream needs closing, or the next poll undoes it.
            if state == BoardState::Done
                && task.source == Source::Github
                && task.upstream != UpstreamState::Terminal
                && self.cfg.github.writeback
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

    pub fn enqueue_dispatch(
        &self,
        task: &Task,
        runtime: &str,
        workspace: &str,
        attempt_no: usize,
        dispatched_by: Option<&str>,
    ) -> Result<()> {
        // Name the parent upstream too: reading the Linear issue should tell you
        // an agent released this, not a person.
        let by = dispatched_by.and_then(|id| {
            self.db
                .get_task(id)
                .ok()
                .flatten()
                .map(|t| t.identifier)
                .or_else(|| Some(id.to_string()))
        });
        self.db.enqueue_writeback(&NewWriteback {
            task_id: task.id.clone(),
            kind: "dispatch".into(),
            payload: json!({
                "runtime": runtime,
                "workspace": workspace,
                "attempt": attempt_no,
                "via": by,
            })
            .to_string(),
            idem_key: format!("{}:dispatch:{}", task.id, attempt_no),
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
                Ok(()) => {
                    let _ = self.db.mark_writeback_done(w.id);
                    let _ = self
                        .db
                        .meta_set(&meta::writeback_at(&w.task_id), &crate::db::now());
                    self.log
                        .info(format!("writeback {} delivered ({})", w.idem_key, w.kind));
                }
                Err(e) => {
                    self.log
                        .warn(format!("writeback {} failed: {e}", w.idem_key));
                    let _ = self.db.defer_writeback(w.id, w.attempts, &e.to_string());
                }
            }
        }
    }

    fn deliver(&self, w: &crate::db::Writeback) -> Result<()> {
        let Some(task) = self.db.get_task(&w.task_id)? else {
            // The task is gone; nothing to say upstream.
            return Ok(());
        };
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
                    other => self.log.warn(format!("unknown writeback kind {other}")),
                }
            }
            Source::Github => {
                let Some(gh) = &self.github else {
                    anyhow::bail!("no GitHub client; writeback stays queued");
                };
                if !self.cfg.github.writeback {
                    self.log.info(format!(
                        "github writeback disabled in routing.toml: {} {}",
                        w.kind, task.identifier
                    ));
                    return Ok(());
                }
                let Some((repo, number)) = split_gh_task_id(&task.id) else {
                    self.log
                        .warn(format!("cannot parse a repo out of {}", task.id));
                    return Ok(());
                };
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

/// `gh:owner/repo#87` → (`owner/repo`, 87). Also accepts the pull-request form
/// `gh:owner/repo!508` — GitHub's issues endpoints serve pull requests too, so
/// comments and closing work the same for both.
pub fn split_gh_task_id(id: &str) -> Option<(String, i64)> {
    let rest = id.strip_prefix("gh:")?;
    let (repo, number) = rest.rsplit_once(['#', '!'])?;
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
            // `gh:owner/repo#87` and `gh:owner/repo!508` → `owner/repo`.
            gh_repo: task
                .id
                .strip_prefix("gh:")
                .and_then(|r| r.split(['#', '!']).next())
                .map(str::to_string),
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
                sync: Default::default(),
                routes: vec![],
                defaults: Defaults::default(),
                github: Default::default(),
            },
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
        }
    }

    fn dispatch(e: &SyncEngine, task: &str, pane_id: &str) -> i64 {
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
            draft: false,
            updated_at: crate::db::now(),
        }])
        .unwrap();

        assert!(e.db.get_task("linear:LIN-142").unwrap().unwrap().pr_open);
        // The unrelated task must not pick up the PR.
        assert!(!e.db.get_task("linear:LIN-999").unwrap().unwrap().pr_open);
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
        e.db.upsert_task(&UpsertTask {
            id: "gh:o/r#87".into(),
            source: Source::Github,
            source_id: "n1".into(),
            identifier: "gh#87".into(),
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

    fn gh_client() -> Github<Box<dyn Rest>> {
        Github::new(Box::new(crate::sources::github::FixtureRest::new(vec![]))
            as Box<dyn Rest>)
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

    #[test]
    fn a_github_row_that_reaches_done_queues_a_close() {
        // Otherwise the next poll recomputes `open` upstream and `d mark done`
        // undoes itself.
        let e = engine_with(None, Some(gh_client()));
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
        let e = engine_with(None, Some(gh_client()));
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
        let e = engine_with(None, Some(gh_client()));
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
    fn writeback_can_be_turned_off_for_github() {
        let mut e = engine_with(None, Some(gh_client()));
        e.cfg.github.writeback = false;
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
}
