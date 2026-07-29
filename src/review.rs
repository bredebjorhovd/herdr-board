//! Deliver pull-request review comments back into the pane that wrote the PR
//! (gh#13).
//!
//! An orchestrator reviews a task and writes its feedback on the pull request.
//! The agent that wrote the PR never sees it: it is sitting idle in a live pane
//! with the entire task in context, and the review is a notification it has no
//! way to receive.
//!
//! The precondition holds by accident. [`SyncEngine::dispose_finished_panes`]
//! only closes a pane once its *task* reaches `done`; a task in `review` keeps
//! its pane, so the author is still alive with its context. And the delivery
//! mechanism is proven — `herdr agent prompt` is how every dispatch delivers its
//! brief, and waking an idle pane costs it nothing. So the missing piece is only
//! to notice the comment and decide it is worth delivering.
//!
//! ## The loop, and why author filtering cannot close it
//!
//! The orchestrator and the agent act as the same GitHub identity — both are the
//! operator's token — so "skip my own comments" has nothing to key on. A naive
//! implementation delivers a comment, the agent replies on the PR, the board
//! sees a new comment and delivers it back, forever.
//!
//! Three things close it, and they are requirements rather than details:
//!
//! 1. **A per-PR watermark**, per endpoint, of the last comment id consumed. An
//!    id below the watermark can never come back.
//! 2. **Deliver only while the agent is idle.** An agent that just replied is
//!    working, so its own reply is not delivered back to it — and a review that
//!    lands while it is busy is *kept*, not consumed, until it is free.
//! 3. **A latch on the wake.** Between waking an agent and seeing it settle
//!    again, whatever appears on the pull request is its own reply; it is
//!    consumed rather than delivered, and logged so that is visible. The cost
//!    is that a human comment landing inside that window is swallowed too —
//!    the honest limit of having no author to key on.

use crate::db::Db;
use crate::herdr::{Herdr, PaneInfo};
use crate::model::{AgentStatus, Attempt, BoardState, Task};
use crate::sources::github::{Feedback, FeedbackKind, Github, PullRequest, Rest};
use crate::sync::SyncEngine;
use crate::wake;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// How long to wait for a woken agent to visibly react before assuming it never
/// will. Only reached when [`wake`] did not see it start — normally the wake
/// itself latches `saw_working` and this never applies.
const REACTION_GRACE_SECS: i64 = 120;

/// What the board has already delivered for one pull request.
///
/// Stored as JSON in `meta` under [`crate::sync::meta::reviews_for`]. A row of
/// its own would be a schema change for a fact that is pure bookkeeping and
/// worthless once the pane is gone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
    /// Highest id consumed, per endpoint. The three sequences are unrelated, so
    /// one watermark cannot cover them.
    #[serde(default)]
    pub issue: i64,
    #[serde(default)]
    pub inline: i64,
    #[serde(default)]
    pub review: i64,
    /// The pull request's `updated_at` these watermarks were computed against.
    /// Unchanged means nothing has happened on the PR since, and none of the
    /// three comment endpoints is worth asking — which is the whole poll-cost
    /// story: one call per open PR per cycle would be wasteful, and the PR list
    /// already reports this.
    #[serde(default)]
    pub updated_at: String,
    /// Set while an agent is answering something we woke it with.
    #[serde(default)]
    pub woke_at: Option<String>,
    /// Have we seen the agent react since being woken?
    #[serde(default)]
    pub saw_working: bool,
    /// Whether a missing pane has already been logged, so a task whose pane the
    /// operator closed does not write the same line every 30 seconds.
    #[serde(default)]
    pub noted_gone: bool,
}

impl Delivered {
    fn watermark(&self, kind: FeedbackKind) -> i64 {
        match kind {
            FeedbackKind::Issue => self.issue,
            FeedbackKind::Inline => self.inline,
            FeedbackKind::Review => self.review,
        }
    }

    fn record(&mut self, f: &Feedback) {
        let slot = match f.kind {
            FeedbackKind::Issue => &mut self.issue,
            FeedbackKind::Inline => &mut self.inline,
            FeedbackKind::Review => &mut self.review,
        };
        *slot = (*slot).max(f.id);
    }

    /// Have we never looked at this pull request before?
    fn first_sight(&self) -> bool {
        self.updated_at.is_empty()
    }
}

/// What the pass decided about one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing has happened on the pull request since the last look. Costs no
    /// GitHub call at all.
    Unchanged,
    /// The agent is busy with something. The feedback keeps: consuming it here
    /// would throw away a review nobody has read.
    Busy,
    /// The agent is answering what we last delivered.
    Answering,
    /// Comments appeared while the agent was answering. They are its own reply
    /// — there is no author to prove it — so they are consumed, not delivered.
    ConsumedOwnReply(usize),
    /// Nothing above the watermark is worth waking anyone for.
    NothingNew,
    /// Wake the agent with these.
    Deliver(Vec<Feedback>),
}

/// Is this worth waking an agent for?
///
/// The board's own writeback comments are not: it posts a dispatch line and an
/// outcome line on every task it touches, and on a pull-request row those land
/// in the same conversation a reviewer writes in. They are the one thing
/// author-blind filtering *can* recognise, because the board wrote them.
pub fn is_actionable(f: &Feedback) -> bool {
    if is_the_boards_own(&f.body) {
        return false;
    }
    // A verdict of `changes requested` is actionable with or without prose. An
    // approval without prose is not: it says the agent has nothing left to do,
    // and waking it to hear so is the opposite of useful.
    f.requests_changes() || !f.body.trim().is_empty()
}

fn is_the_boards_own(body: &str) -> bool {
    let b = body.trim_start();
    b.starts_with("herdr-board:") || b.starts_with("Dispatched to herdr")
}

/// Decide what to do about one pull request, asking GitHub only if the decision
/// needs it.
///
/// Split from the effects so the loop guard — the part that has to converge —
/// is testable without a herdr, a network, or a pane. `state` is mutated in
/// place; the caller persists it unless the wake itself failed, in which case
/// dropping it is what makes the delivery retry.
///
/// `floor` is the authoring attempt's end (or start, if it is somehow still
/// live). It applies only on first sight, and only to stop the board delivering
/// a pull request's entire back catalogue the first time it looks at one:
/// nothing written before the agent finished can be a review of its work.
pub fn plan_delivery(
    state: &mut Delivered,
    pr_updated_at: &str,
    status: AgentStatus,
    floor: &str,
    now: &str,
    fetch: impl FnOnce() -> Result<Vec<Feedback>>,
) -> Result<Decision> {
    // `unknown` is not idle. herdr failing to classify a pane is not evidence
    // the agent in it is free, and typing into an agent mid-turn is exactly the
    // interruption this is supposed to avoid.
    let idle = matches!(status, AgentStatus::Idle | AgentStatus::Done);

    if state.woke_at.is_some() {
        if !idle {
            // Reacting to us. Latch it, so its return to idle can be told apart
            // from never having picked the prompt up.
            state.saw_working = true;
            return Ok(Decision::Answering);
        }
        if !state.saw_working && !reaction_grace_elapsed(state, now) {
            // Woken, but not yet moving. Deciding on one tick that nothing
            // happened would deliver its reply straight back to it.
            return Ok(Decision::Answering);
        }
    }

    if !idle {
        return Ok(Decision::Busy);
    }

    // From here the wake is over, one way or the other.
    let resolving = state.woke_at.take().is_some();
    state.saw_working = false;

    // The poll-cost gate: the pull request list already told us nothing has
    // happened, so none of the three comment endpoints is asked. An absent
    // timestamp is not a match — it would gate this off permanently.
    if !pr_updated_at.is_empty() && state.updated_at == pr_updated_at {
        return Ok(Decision::Unchanged);
    }

    let first_sight = state.first_sight();
    let feedback = fetch()?;
    let fresh: Vec<Feedback> = feedback
        .iter()
        .filter(|f| f.id > state.watermark(f.kind))
        .filter(|f| is_actionable(f))
        .filter(|f| !first_sight || f.created_at.as_str() > floor)
        .cloned()
        .collect();

    // Consume everything seen, delivered or not. An id below the watermark can
    // never come back, which is what stops the same comment arriving twice.
    for f in &feedback {
        state.record(f);
    }
    state.updated_at = pr_updated_at.to_string();

    if resolving {
        return Ok(Decision::ConsumedOwnReply(fresh.len()));
    }
    if fresh.is_empty() {
        return Ok(Decision::NothingNew);
    }
    // Latched here rather than by the caller, so a delivery that is planned and
    // then fails to reach the pane is retried rather than silently swallowed:
    // the caller drops this whole state instead of storing it.
    state.woke_at = Some(now.to_string());
    Ok(Decision::Deliver(fresh))
}

fn reaction_grace_elapsed(state: &Delivered, now: &str) -> bool {
    let (Some(woke), Ok(now)) = (
        state
            .woke_at
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()),
        chrono::DateTime::parse_from_rfc3339(now),
    ) else {
        // An unparseable timestamp must not wedge the latch shut forever.
        return true;
    };
    (now - woke).num_seconds() >= REACTION_GRACE_SECS
}

/// The pull request a task's PR fields point at, among the ones just polled.
///
/// Matched on the URL rather than the number: a number is only unique within a
/// repo, and the polled list spans every repo the board watches.
pub fn pull_request_for<'a>(task: &Task, pulls: &'a [PullRequest]) -> Option<&'a PullRequest> {
    let url = task.pr_url.as_deref()?;
    pulls.iter().find(|p| p.url == url)
}

/// The attempt that wrote this pull request.
///
/// The branch is the proof, and the only one there is: it is the link dispatch
/// creates, and a task can have several attempts with several panes. Without a
/// branch match we do not know which session produced the PR — and delivering
/// somebody else's review into an unrelated session is worse than not
/// delivering.
pub fn authoring_attempt<'a>(task: &'a Task, pr: &PullRequest) -> Option<&'a Attempt> {
    task.attempts
        .iter()
        .rev()
        .find(|a| a.pane_id.is_some() && a.branch.as_deref() == Some(pr.head_ref.as_str()))
}

/// Does this pane still hold the agent that wrote the pull request?
///
/// herdr pane ids are not reused, so the pane still existing is most of the
/// answer. The rest: something has to be running in it — a pane whose agent
/// exited is a shell prompt, and pasting a review into one types the review at
/// bash — and where it is running has to still be the attempt's own checkout.
pub fn still_holds_the_author(pane: &PaneInfo, attempt: &Attempt) -> bool {
    if pane.agent.is_none() {
        return false;
    }
    match (pane.cwd.as_deref(), attempt.worktree.as_deref()) {
        // `starts_with`, not equality: an agent that has cd'd into a
        // subdirectory of its own worktree is still in its own worktree.
        (Some(cwd), Some(worktree)) => {
            std::path::Path::new(cwd).starts_with(std::path::Path::new(worktree))
        }
        // herdr did not say where the pane is. The pane id and a live agent are
        // then all there is, and they are enough to not deliver blind.
        _ => true,
    }
}

/// The message an agent is woken with.
///
/// Enough to act on without a lookup: the body, the pull request URL, and for
/// an inline comment the file and line it hangs off.
pub fn compose(task: &Task, pr: &PullRequest, items: &[Feedback]) -> String {
    let mut s = format!(
        "herdr-board: your pull request has been reviewed.\n\n  \
         {} · {}\n  {}\n",
        task.identifier, task.title, pr.url
    );
    for f in items {
        s.push('\n');
        match (f.kind, f.state.as_deref()) {
            (FeedbackKind::Review, Some(state)) => {
                s.push_str(&format!("[review · {}]\n", state.replace('_', " ")));
            }
            (FeedbackKind::Inline, _) => {
                let where_ = match (f.path.as_deref(), f.line) {
                    (Some(p), Some(l)) => format!("{p}:{l}"),
                    (Some(p), None) => p.to_string(),
                    // An inline comment whose anchor GitHub no longer reports.
                    _ => "(inline)".to_string(),
                };
                s.push_str(&format!("[{where_}]\n"));
            }
            _ => {}
        }
        let body = f.body.trim();
        if body.is_empty() {
            // Only reachable for `changes requested` with no prose, which is
            // still the loudest thing on the list.
            s.push_str("(no comment body — see the pull request)\n");
        } else {
            s.push_str(body);
            s.push('\n');
        }
    }
    s.push_str(
        "\nThe pull request is still open and you are still in the worktree you wrote it \
         in. Address the feedback there, commit, and push to the same branch — do not open \
         a second pull request. If you disagree with a point, say so on the pull request \
         rather than silently skipping it.\n",
    );
    s
}

/// Load a task's delivery state. A missing or unreadable value starts over,
/// which costs one round of re-consumption and never a wrong delivery.
fn load(db: &Db, task_id: &str) -> Delivered {
    db.meta_get(&crate::sync::meta::reviews_for(task_id))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

fn store(db: &Db, task_id: &str, state: &Delivered) -> Result<()> {
    db.meta_set(
        &crate::sync::meta::reviews_for(task_id),
        &serde_json::to_string(state)?,
    )?;
    Ok(())
}

impl SyncEngine {
    /// Wake the agents whose pull requests have been reviewed.
    ///
    /// Runs after derivation, because `review` is the state that keeps a pane
    /// and it is only correct once this tick's reconciliation has landed. Takes
    /// the pane list the cycle already fetched rather than asking herdr again
    /// per task.
    ///
    /// A delivery holds the cycle for as long as it takes to confirm the prompt
    /// arrived — seconds, and only when there is something to deliver, which is
    /// rare. The alternative is a prompt that pasted without submitting, which
    /// is the failure that made dispatch fragile; the wait is the cheaper half
    /// of that trade.
    pub fn deliver_reviews(&self, herdr: &Herdr, panes: &[PaneInfo], pulls: &[PullRequest]) {
        if !self.cfg.github.deliver_reviews {
            return;
        }
        let Some(gh) = &self.github else {
            return;
        };
        if pulls.is_empty() {
            return;
        }
        let by_id: HashMap<&str, &PaneInfo> =
            panes.iter().map(|p| (p.pane_id.as_str(), p)).collect();
        let tasks = match self.db.load_tasks() {
            Ok(t) => t,
            Err(e) => {
                self.log.error(format!("reading tasks for review delivery: {e}"));
                return;
            }
        };
        for task in tasks {
            if let Err(e) = self.deliver_review_for(gh, herdr, &by_id, pulls, &task) {
                self.log
                    .warn(format!("delivering review for {}: {e}", task.identifier));
            }
        }
    }

    fn deliver_review_for(
        &self,
        gh: &Github<Box<dyn Rest>>,
        herdr: &Herdr,
        panes: &HashMap<&str, &PaneInfo>,
        pulls: &[PullRequest],
        task: &Task,
    ) -> Result<()> {
        // `review` is the whole precondition: finished work with an open pull
        // request, whose pane nothing has disposed of yet.
        if task.state != BoardState::Review || !task.pr_open {
            return Ok(());
        }
        let Some(pr) = pull_request_for(task, pulls) else {
            return Ok(());
        };
        let Some(attempt) = authoring_attempt(task, pr) else {
            return Ok(());
        };
        let Some(pane_id) = attempt.pane_id.as_deref() else {
            return Ok(());
        };
        let mut state = load(&self.db, &task.id);

        // A task whose pane is gone is skipped quietly. There is nothing to
        // deliver to and nothing to do about it — re-dispatching would be a
        // second agent on work that is already written.
        let gone = match panes.get(pane_id) {
            Some(pane) => !still_holds_the_author(pane, attempt),
            None => true,
        };
        if gone {
            if !state.noted_gone {
                self.log.info(format!(
                    "{}: pane {pane_id} no longer holds the agent that wrote {} — \
                     review comments will not be delivered",
                    task.identifier, pr.url
                ));
                state.noted_gone = true;
                store(&self.db, &task.id, &state)?;
            }
            return Ok(());
        }
        state.noted_gone = false;

        let pane = panes[pane_id];
        let status = pane.agent_status.unwrap_or(AgentStatus::Unknown);
        let now = crate::db::now();
        let floor = attempt
            .ended_at
            .clone()
            .unwrap_or_else(|| attempt.started_at.clone());

        let decision = plan_delivery(&mut state, &pr.updated_at, status, &floor, &now, || {
            gh.pr_feedback(&pr.repo, pr.number)
        })?;

        match &decision {
            Decision::Deliver(items) => {
                let text = compose(task, pr, items);
                let delivery = wake::wake(herdr, &self.log, pane_id, &text);
                if !delivery.reached_the_pane() {
                    // Nothing arrived, so nothing was consumed: dropping the
                    // state here is what makes the next cycle try again.
                    self.log.warn(format!(
                        "{}: could not wake {pane_id} with {} review comment(s)",
                        task.identifier,
                        items.len()
                    ));
                    return Ok(());
                }
                // Whether we *saw* it react decides how the next cycle reads
                // the pane: an agent we watched start is answering us, and its
                // next comment is its own.
                state.saw_working = delivery.saw_it_react();
                self.log.info(format!(
                    "{}: delivered {} review comment(s) on {} into pane {pane_id}",
                    task.identifier,
                    items.len(),
                    pr.url
                ));
            }
            Decision::ConsumedOwnReply(n) if *n > 0 => self.log.info(format!(
                "{}: {n} comment(s) appeared on {} while its agent was answering — \
                 taken as its own reply, not delivered back",
                task.identifier, pr.url
            )),
            _ => {}
        }
        store(&self.db, &task.id, &state)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Outcome;

    fn feedback(kind: FeedbackKind, id: i64, body: &str, created_at: &str) -> Feedback {
        Feedback {
            kind,
            id,
            body: body.into(),
            url: format!("https://github.com/o/r/pull/14#c{id}"),
            created_at: created_at.into(),
            author: "bredebjorhovd".into(),
            path: None,
            line: None,
            state: None,
        }
    }

    fn verdict(id: i64, state: &str, body: &str, created_at: &str) -> Feedback {
        Feedback {
            state: Some(state.into()),
            ..feedback(FeedbackKind::Review, id, body, created_at)
        }
    }

    fn inline(id: i64, path: &str, line: i64, body: &str, created_at: &str) -> Feedback {
        Feedback {
            path: Some(path.into()),
            line: Some(line),
            ..feedback(FeedbackKind::Inline, id, body, created_at)
        }
    }

    const FLOOR: &str = "2026-07-28T10:00:00Z";

    fn plan(
        state: &mut Delivered,
        updated_at: &str,
        status: AgentStatus,
        now: &str,
        items: Vec<Feedback>,
    ) -> Decision {
        plan_delivery(state, updated_at, status, FLOOR, now, || Ok(items)).unwrap()
    }

    /// The steady state, and the one that has to be free: the pull request has
    /// not moved, so none of the three comment endpoints is asked at all.
    #[test]
    fn an_unchanged_pull_request_costs_no_api_call() {
        let mut state = Delivered {
            updated_at: "2026-07-28T11:00:00Z".into(),
            ..Default::default()
        };
        let decision = plan_delivery(
            &mut state,
            "2026-07-28T11:00:00Z",
            AgentStatus::Idle,
            FLOOR,
            "2026-07-28T11:05:00Z",
            || panic!("nothing moved on the PR; there is nothing to ask about"),
        )
        .unwrap();
        assert_eq!(decision, Decision::Unchanged);
    }

    #[test]
    fn a_changes_requested_review_is_delivered_and_watermarked() {
        let mut state = Delivered {
            updated_at: "2026-07-28T11:00:00Z".into(),
            ..Default::default()
        };
        let d = plan(
            &mut state,
            "2026-07-28T11:30:00Z",
            AgentStatus::Idle,
            "2026-07-28T11:31:00Z",
            vec![verdict(
                900,
                "changes_requested",
                "Split the watermark per endpoint.",
                "2026-07-28T11:30:00Z",
            )],
        );
        let Decision::Deliver(items) = d else {
            panic!("expected a delivery, got {d:?}");
        };
        assert_eq!(items.len(), 1);
        assert!(items[0].requests_changes());
        assert_eq!(state.review, 900, "the watermark moved past it");
        assert!(state.woke_at.is_some(), "the wake is latched");
    }

    /// The loop the whole design exists to prevent: deliver, the agent replies
    /// on the PR, and the board must not deliver that reply back to it.
    #[test]
    fn an_agents_own_reply_is_never_delivered_back_to_it() {
        let mut state = Delivered {
            updated_at: "2026-07-28T11:00:00Z".into(),
            ..Default::default()
        };
        // The orchestrator asks for changes.
        let d = plan(
            &mut state,
            "2026-07-28T11:30:00Z",
            AgentStatus::Idle,
            "2026-07-28T11:31:00Z",
            vec![verdict(900, "changes_requested", "Fix the gate.", "2026-07-28T11:30:00Z")],
        );
        assert!(matches!(d, Decision::Deliver(_)));

        // It picks the prompt up and gets to work.
        let d = plan(
            &mut state,
            "2026-07-28T11:30:00Z",
            AgentStatus::Working,
            "2026-07-28T11:32:00Z",
            vec![],
        );
        assert_eq!(d, Decision::Answering);
        assert!(state.saw_working);

        // It replies on the pull request and settles. Same GitHub identity as
        // the orchestrator, so nothing about the comment says who wrote it.
        let reply = feedback(
            FeedbackKind::Issue,
            41,
            "Fixed in abc123.",
            "2026-07-28T11:40:00Z",
        );
        let d = plan(
            &mut state,
            "2026-07-28T11:40:00Z",
            AgentStatus::Idle,
            "2026-07-28T11:41:00Z",
            vec![reply],
        );
        assert_eq!(d, Decision::ConsumedOwnReply(1), "consumed, not delivered");
        assert_eq!(state.issue, 41, "and it can never come back");
        assert!(state.woke_at.is_none(), "the latch is released");

        // And the board is now listening again: the next real review lands.
        let d = plan(
            &mut state,
            "2026-07-28T11:50:00Z",
            AgentStatus::Idle,
            "2026-07-28T11:51:00Z",
            vec![feedback(
                FeedbackKind::Issue,
                42,
                "Still wrong on the retry path.",
                "2026-07-28T11:50:00Z",
            )],
        );
        assert!(matches!(d, Decision::Deliver(items) if items.len() == 1));
    }

    /// A review that lands while the agent is busy with something else is kept,
    /// not consumed. Consuming it would throw away feedback nobody has read.
    #[test]
    fn feedback_that_arrives_while_the_agent_is_busy_keeps() {
        let mut state = Delivered {
            updated_at: "2026-07-28T11:00:00Z".into(),
            ..Default::default()
        };
        let d = plan_delivery(
            &mut state,
            "2026-07-28T11:30:00Z",
            AgentStatus::Working,
            FLOOR,
            "2026-07-28T11:31:00Z",
            || panic!("a busy agent is not asked about"),
        )
        .unwrap();
        assert_eq!(d, Decision::Busy);
        assert_eq!(state.updated_at, "2026-07-28T11:00:00Z", "nothing consumed");

        // It frees up, and the review is still there to deliver.
        let d = plan(
            &mut state,
            "2026-07-28T11:30:00Z",
            AgentStatus::Idle,
            "2026-07-28T11:35:00Z",
            vec![verdict(900, "changes_requested", "Have another look.", "2026-07-28T11:30:00Z")],
        );
        assert!(matches!(d, Decision::Deliver(_)));
    }

    /// `unknown` is herdr not classifying the pane, which is not evidence the
    /// agent is free.
    #[test]
    fn an_unclassified_agent_is_not_treated_as_idle() {
        let mut state = Delivered {
            updated_at: "a".into(),
            ..Default::default()
        };
        let d = plan_delivery(
            &mut state,
            "b",
            AgentStatus::Unknown,
            FLOOR,
            "2026-07-28T11:00:00Z",
            || panic!("not asked"),
        )
        .unwrap();
        assert_eq!(d, Decision::Busy);
    }

    /// A wake that herdr never saw the agent react to must not latch forever —
    /// otherwise one missed reaction stops every future delivery for that PR.
    #[test]
    fn a_wake_nothing_reacted_to_releases_after_the_grace() {
        let mut state = Delivered {
            updated_at: "2026-07-28T11:00:00Z".into(),
            woke_at: Some("2026-07-28T11:00:00Z".into()),
            saw_working: false,
            ..Default::default()
        };
        // A tick later: still assumed to be picking the prompt up.
        let d = plan_delivery(
            &mut state,
            "2026-07-28T11:00:30Z",
            AgentStatus::Idle,
            FLOOR,
            "2026-07-28T11:00:30Z",
            || panic!("not asked while the wake is still settling"),
        )
        .unwrap();
        assert_eq!(d, Decision::Answering);

        // Well past the grace: it never moved, so the latch releases.
        let d = plan(
            &mut state,
            "2026-07-28T11:10:00Z",
            AgentStatus::Idle,
            "2026-07-28T11:10:00Z",
            vec![],
        );
        assert_eq!(d, Decision::ConsumedOwnReply(0));
        assert!(state.woke_at.is_none());
    }

    /// First sight must not hand an agent the pull request's whole back
    /// catalogue — including its own PR-opening chatter.
    #[test]
    fn the_first_look_at_a_pull_request_delivers_nothing_older_than_the_work() {
        let mut state = Delivered::default();
        assert!(state.first_sight());
        let d = plan(
            &mut state,
            "2026-07-28T12:00:00Z",
            AgentStatus::Idle,
            "2026-07-28T12:01:00Z",
            vec![
                // Written by the agent while it still had the pane.
                feedback(FeedbackKind::Issue, 10, "Opened the PR.", "2026-07-28T09:30:00Z"),
                // Written after it finished: a genuine review.
                verdict(900, "changes_requested", "Not quite.", "2026-07-28T11:00:00Z"),
            ],
        );
        let Decision::Deliver(items) = d else {
            panic!("expected the review to be delivered, got {d:?}");
        };
        assert_eq!(items.len(), 1, "only what came after the work");
        assert!(items[0].requests_changes());
        assert_eq!(state.issue, 10, "the older one is consumed, not delivered");
    }

    #[test]
    fn the_boards_own_writeback_comments_are_not_review_feedback() {
        // On a pull-request row these land in the same conversation a reviewer
        // writes in, and waking the agent with its own dispatch line is noise.
        assert!(!is_actionable(&feedback(
            FeedbackKind::Issue,
            1,
            "Dispatched to herdr · claude-code · ws:offhand · attempt 1",
            "t"
        )));
        assert!(!is_actionable(&feedback(
            FeedbackKind::Issue,
            2,
            "herdr-board: attempt finished · https://github.com/o/r/pull/14",
            "t"
        )));
        assert!(is_actionable(&feedback(
            FeedbackKind::Issue,
            3,
            "The retry path is still wrong.",
            "t"
        )));
    }

    #[test]
    fn an_approval_with_nothing_to_say_wakes_nobody() {
        assert!(!is_actionable(&verdict(1, "approved", "", "t")));
        // With prose it is worth hearing.
        assert!(is_actionable(&verdict(2, "approved", "Nice — rename `x` first.", "t")));
        // And a verdict of changes requested is actionable with or without it.
        assert!(is_actionable(&verdict(3, "changes_requested", "", "t")));
        // An empty comment is nothing at all.
        assert!(!is_actionable(&feedback(FeedbackKind::Issue, 4, "   ", "t")));
    }

    #[test]
    fn the_message_says_where_an_inline_comment_points() {
        let task = task_row();
        let pr = pull("board/gh-13");
        let text = compose(
            &task,
            &pr,
            &[
                verdict(900, "changes_requested", "Two things.", "t"),
                inline(41, "src/review.rs", 88, "This consumes a human comment.", "t"),
            ],
        );
        assert!(text.contains("https://github.com/o/r/pull/14"), "{text}");
        assert!(text.contains("gh#13"), "{text}");
        assert!(text.contains("[review · changes requested]"), "{text}");
        assert!(text.contains("[src/review.rs:88]"), "{text}");
        assert!(text.contains("This consumes a human comment."), "{text}");
        // Enough to act on without a lookup, and told where to put the fix.
        assert!(text.contains("push to the same branch"), "{text}");
    }

    // ---- pane verification ----------------------------------------------

    fn task_row() -> Task {
        Task {
            id: "gh:o/r#13".into(),
            source: crate::model::Source::Github,
            source_id: "n".into(),
            identifier: "gh#13".into(),
            title: "Deliver PR review comments back into the agent's pane".into(),
            body: None,
            url: "https://github.com/o/r/issues/13".into(),
            labels: vec![],
            state: BoardState::Review,
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream: crate::model::UpstreamState::Unstarted,
            local_done: false,
            pr_url: Some("https://github.com/o/r/pull/14".into()),
            pr_number: Some(14),
            pr_open: true,
            pr_merged: false,
            pr_mergeable: None,
            updated_at: "t".into(),
            synced_at: "t".into(),
            attempts: vec![],
        }
    }

    fn pull(head_ref: &str) -> PullRequest {
        PullRequest {
            repo: "o/r".into(),
            number: 14,
            title: "Deliver review comments".into(),
            body: None,
            url: "https://github.com/o/r/pull/14".into(),
            head_ref: head_ref.into(),
            open: true,
            merged: false,
            draft: false,
            updated_at: "2026-07-28T11:30:00Z".into(),
        }
    }

    fn attempt(branch: &str, pane: &str, worktree: Option<&str>) -> Attempt {
        Attempt {
            id: 1,
            task_id: "gh:o/r#13".into(),
            pane_id: Some(pane.into()),
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: worktree.map(str::to_string),
            branch: Some(branch.into()),
            started_at: "2026-07-28T09:00:00Z".into(),
            ended_at: Some("2026-07-28T10:00:00Z".into()),
            outcome: Some(Outcome::Done),
            missing_ticks: 0,
            settled_ticks: 0,
            agent_status: Some(AgentStatus::Idle),
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
            saw_working: true,
            screen_print: None,
            screen_at: None,
        }
    }

    fn pane_info(id: &str, agent: Option<&str>, cwd: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            workspace_id: "w1".into(),
            tab_id: Some("w1:t1".into()),
            agent: agent.map(str::to_string),
            agent_status: Some(AgentStatus::Idle),
            focused: false,
            label: None,
            cwd: cwd.map(str::to_string),
            scroll_offset: 0,
        }
    }

    /// The branch is the only proof of which session wrote the PR. A task can
    /// hold several attempts in several panes — the picker can override the
    /// branch per dispatch — and a review must not go to whichever is newest.
    #[test]
    fn only_the_attempt_whose_branch_matches_owns_the_pull_request() {
        let mut task = task_row();
        task.attempts = vec![
            attempt("board/gh-13", "w1:p1", None),
            Attempt {
                id: 2,
                ..attempt("spike/another-idea", "w1:p9", None)
            },
        ];
        let pr = pull("board/gh-13");
        assert_eq!(
            authoring_attempt(&task, &pr).unwrap().pane_id.as_deref(),
            Some("w1:p1")
        );
        // A pull request nothing on this task branched to belongs to nobody
        // here, and nobody here gets woken for it.
        assert!(authoring_attempt(&task, &pull("somebody/else")).is_none());
    }

    #[test]
    fn a_pane_whose_agent_exited_is_not_delivered_into() {
        let a = attempt("board/gh-13", "w1:p1", None);
        // A pane back at a shell prompt would take the review as a command.
        assert!(!still_holds_the_author(&pane_info("w1:p1", None, None), &a));
        assert!(still_holds_the_author(
            &pane_info("w1:p1", Some("claude"), None),
            &a
        ));
    }

    #[test]
    fn a_pane_that_moved_to_another_checkout_is_not_the_author() {
        let a = attempt("board/gh-13", "w1:p1", Some("/wt/gh-13-1"));
        assert!(still_holds_the_author(
            &pane_info("w1:p1", Some("claude"), Some("/wt/gh-13-1")),
            &a
        ));
        // Deeper inside its own checkout is still its own checkout.
        assert!(still_holds_the_author(
            &pane_info("w1:p1", Some("claude"), Some("/wt/gh-13-1/src")),
            &a
        ));
        // Somebody else's session entirely.
        assert!(!still_holds_the_author(
            &pane_info("w1:p1", Some("claude"), Some("/wt/lin-140-1")),
            &a
        ));
    }

    // ---- the pass, driven through the engine ----------------------------

    /// Shares one fixture between the engine, which owns its transport, and the
    /// test, which needs to read back what was asked.
    struct Shared(std::rc::Rc<crate::sources::github::FixtureRest>);

    impl Rest for Shared {
        fn get(&self, path: &str) -> Result<serde_json::Value> {
            self.0.get(path)
        }
        fn post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
            self.0.post(path, body)
        }
        fn patch(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
            self.0.patch(path, body)
        }
        fn put(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
            self.0.put(path, body)
        }
    }

    /// An engine over an in-memory board and a recorded GitHub.
    fn engine(rest: std::rc::Rc<crate::sources::github::FixtureRest>) -> SyncEngine {
        let dir = std::env::temp_dir().join(format!("hb-review-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        SyncEngine {
            db: Db::open_in_memory().unwrap(),
            cfg: crate::config::RoutingConfig::default(),
            credentials: Default::default(),
            paths: crate::config::Paths {
                config_dir: dir.clone(),
                state_dir: dir,
            },
            log: std::sync::Arc::new(crate::log::Logger::new("", false)),
            linear: None,
            github: Some(Github::new(Box::new(Shared(rest)) as Box<dyn Rest>)),
        }
    }

    /// A finished attempt on gh#13, with an open pull request — the row this
    /// whole module is about.
    fn seed_reviewed_task(e: &SyncEngine, branch: &str, pane: &str) {
        e.db.upsert_task(&crate::db::UpsertTask {
            id: "gh:o/r#13".into(),
            source: crate::model::Source::Github,
            source_id: "n".into(),
            identifier: "gh#13".into(),
            title: "Deliver PR review comments".into(),
            body: None,
            url: "https://github.com/o/r/issues/13".into(),
            labels: vec![],
            source_state: Some("open".into()),
            linear_team: None,
            linear_project: None,
            upstream: crate::model::UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let a = e
            .db
            .insert_attempt(&crate::db::NewAttempt {
                task_id: "gh:o/r#13".into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: Some("/wt/gh-13-1".into()),
                branch: Some(branch.into()),
                dispatched_by: None,
                dispatched_by_pane: None,
                base_sha: None,
            })
            .unwrap();
        e.db.set_attempt_pane(a, pane).unwrap();
        e.db.close_attempt(a, Outcome::Done).unwrap();
        e.db.set_pr(
            "gh:o/r#13",
            Some("https://github.com/o/r/pull/14"),
            Some(14),
            true,
        )
        .unwrap();
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("gh:o/r#13").unwrap().unwrap().state,
            BoardState::Review
        );
    }

    fn state_of(e: &SyncEngine) -> Delivered {
        load(&e.db, "gh:o/r#13")
    }

    /// The pane the agent was in is gone — the operator closed it, or the whole
    /// session ended. Nothing to deliver to, and nothing to do about it: a
    /// re-dispatch would be a second agent on work that is already written.
    #[test]
    fn a_task_whose_pane_is_gone_is_skipped_quietly() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(vec![]));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "w1:p1");
        let h = Herdr::discover(e.log.clone());

        // herdr knows a pane, just not that one.
        e.deliver_reviews(&h, &[pane_info("w9:p4", Some("claude"), None)], &[pull(
            "board/gh-13",
        )]);

        assert!(
            rest.asked.borrow().is_empty(),
            "a pane that is gone costs no GitHub call: {:?}",
            rest.asked.borrow()
        );
        assert!(state_of(&e).noted_gone, "and it is said once, not every tick");
    }

    /// The candidate path, right up to the point where a wake would happen:
    /// task selected, pane verified, all three endpoints asked, watermarks
    /// recorded. Nothing here is new enough to wake anybody for.
    #[test]
    fn a_verified_pane_with_nothing_new_records_the_watermark_and_stops() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(vec![
            (
                "/repos/o/r/issues/14/comments".into(),
                serde_json::json!([{ "id": 41, "body": "Opened the PR.",
                                     "created_at": "2026-07-28T09:30:00Z",
                                     "user": { "login": "b" } }]),
            ),
            ("/repos/o/r/pulls/14/comments".into(), serde_json::json!([])),
            ("/repos/o/r/pulls/14/reviews".into(), serde_json::json!([])),
        ]));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "w1:p1");
        let h = Herdr::discover(e.log.clone());

        e.deliver_reviews(
            &h,
            &[pane_info("w1:p1", Some("claude"), Some("/wt/gh-13-1"))],
            &[pull("board/gh-13")],
        );

        assert_eq!(rest.asked.borrow().len(), 3, "all three sources are read");
        let state = state_of(&e);
        assert_eq!(state.issue, 41, "consumed, so it can never come back");
        assert_eq!(state.updated_at, "2026-07-28T11:30:00Z");
        assert!(state.woke_at.is_none(), "nobody was woken");

        // And a second cycle over an unchanged pull request asks nothing at all.
        e.deliver_reviews(
            &h,
            &[pane_info("w1:p1", Some("claude"), Some("/wt/gh-13-1"))],
            &[pull("board/gh-13")],
        );
        assert_eq!(rest.asked.borrow().len(), 3, "the updated_at gate held");
    }

    /// A pull request nobody dispatched has no author sitting in a pane. It is
    /// a `review` row like any other, and there is no one to tell.
    #[test]
    fn a_pull_request_row_the_board_never_dispatched_wakes_nobody() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(vec![]));
        let e = engine(rest.clone());
        let pr = pull("someone/elses-branch");
        e.db.upsert_task(&pr.to_upsert()).unwrap();
        e.db.set_pr(&pr.task_id(), Some(&pr.url), Some(pr.number), true)
            .unwrap();
        e.rederive_all().unwrap();
        let h = Herdr::discover(e.log.clone());

        e.deliver_reviews(&h, &[pane_info("w1:p1", Some("claude"), None)], &[pr]);
        assert!(rest.asked.borrow().is_empty());
    }

    #[test]
    fn turning_delivery_off_stops_it_asking_anything() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(vec![]));
        let mut e = engine(rest.clone());
        e.cfg.github.deliver_reviews = false;
        seed_reviewed_task(&e, "board/gh-13", "w1:p1");
        let h = Herdr::discover(e.log.clone());
        e.deliver_reviews(
            &h,
            &[pane_info("w1:p1", Some("claude"), Some("/wt/gh-13-1"))],
            &[pull("board/gh-13")],
        );
        assert!(rest.asked.borrow().is_empty());
    }

    #[test]
    fn a_pull_request_is_matched_to_its_task_by_url_not_number() {
        let task = task_row();
        let mut other = pull("board/gh-13");
        other.repo = "someone/else".into();
        other.url = "https://github.com/someone/else/pull/14".into();
        // Same number, different repo: not this task's pull request.
        assert!(pull_request_for(&task, std::slice::from_ref(&other)).is_none());
        assert_eq!(
            pull_request_for(&task, &[other, pull("board/gh-13")])
                .unwrap()
                .repo,
            "o/r"
        );
    }
}
