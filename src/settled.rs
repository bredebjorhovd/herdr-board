//! Tell the agent that released a task when that task settles (AGE-25).
//!
//! An agent that finishes does not tell whoever released it. The operator
//! notices a toast, walks over, and tells the orchestrator to go and review —
//! every time. Neither of the two things the board already had closes that gap:
//! [`SyncEngine::notify_settled`] raises a desktop notification aimed at the
//! operator and sends nothing to any agent, and `herdr-board wait` blocks the
//! orchestrator until work settles, which is worse than prodding it — a blocked
//! orchestrator cannot answer the operator, start anything else, or be
//! interrupted.
//!
//! ## Why this is no longer the thing AGE-3 rejected
//!
//! Pushing a prompt into an orchestrator's pane was considered and rejected
//! once, on three grounds. Each has since stopped being true:
//!
//! - *Delivery into a running agent is unreliable by construction.* It was, and
//!   [`crate::wake`] is what fixed it: nudge-and-verify, proven by every
//!   dispatch and by review delivery (gh#13).
//! - *The pane is often gone by then.* It may well be — so this verifies the
//!   pane still exists and still holds the agent that dispatched, and drops the
//!   notice rather than misdelivering into somebody else's session.
//! - *An agent talking unprompted is action at a distance.* Which is why it is
//!   off by default. `[defaults] notify_dispatcher = true` is the operator
//!   saying they want it.
//!
//! It also could not have worked before AGE-24. Keyed on `dispatched_by` this
//! would never fire: that column is NULL on every attempt, because an
//! orchestrator pane owns no attempt and so has no task id. `dispatched_by_pane`
//! is the column that answers "who released this", and the pane it names is the
//! delivery address.
//!
//! ## What is a settle, and what is not
//!
//! Only the three ends of an attempt: it finished, its pane exited, or its agent
//! never started. A `blocked` agent is not one of them. Blocked means the work
//! is waiting on a *human* — a permission prompt, a question — and there is
//! nothing the dispatcher can do about it, so it stays a toast.

use crate::herdr::{Herdr, PaneInfo};
use crate::model::{Attempt, Outcome, Task};
use crate::sync::SyncEngine;
use crate::wake;

/// How an attempt ended.
///
/// One type for the two audiences, so a settle that reaches the operator cannot
/// silently stop reaching the dispatcher: adding an end to this list forces both
/// wordings to be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    /// The agent settled with a pull request or commits to show for it.
    Finished,
    /// The pane went away with the work unfinished.
    PaneExited,
    /// The agent left its pane without ever being seen working.
    NeverStarted,
}

impl Settled {
    /// How the operator's notification says it, after the identifier:
    /// "AGE-25 finished".
    pub fn toast(self) -> &'static str {
        match self {
            Settled::Finished => "finished",
            Settled::PaneExited => "failed — its pane exited",
            Settled::NeverStarted => "failed — its agent never started",
        }
    }

    pub fn outcome(self) -> Outcome {
        match self {
            Settled::Finished => Outcome::Done,
            Settled::PaneExited | Settled::NeverStarted => Outcome::Orphaned,
        }
    }

    /// The outcome line the dispatcher reads. Says more than the toast, which
    /// has a width limit and a person reading it; this has neither.
    fn line(self, has_pr: bool) -> &'static str {
        match (self, has_pr) {
            (Settled::Finished, true) => "finished — pull request open",
            (Settled::Finished, false) => "finished — committed, but opened no pull request",
            (Settled::PaneExited, _) => "failed — its pane exited before the work was finished",
            (Settled::NeverStarted, _) => "failed — its agent never started",
        }
    }

    /// What the dispatcher can do about it, which is the only reason to wake it.
    fn what_now(self, has_pr: bool) -> &'static str {
        match (self, has_pr) {
            (Settled::Finished, true) => {
                "Review it when you are ready. The pane that wrote it stays open until the \
                 task leaves review, so comments you write on the pull request reach the \
                 agent that wrote it."
            }
            (Settled::Finished, false) => {
                "It committed without opening a pull request, so nothing is up for review \
                 yet. The worktree above is where the work is."
            }
            _ => {
                "Nothing was delivered and its pane is gone. Re-dispatch it if the work \
                 still matters; there is no agent left to prompt."
            }
        }
    }
}

/// What a settled attempt asks for, beyond the operator's notification.
///
/// Pure, so the routing rule is testable without a herdr, a pane, or a database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Turned off. The default.
    Disabled,
    /// Nobody released this but the operator, who has the notification already.
    /// Not merely "we do not know who" — see [`crate::model::Dispatcher`].
    NobodyToWake,
    /// Wake this pane, if it still holds the agent that dispatched.
    Wake(String),
}

/// Decide where a settle notice goes.
pub fn plan_notice(attempt: &Attempt, enabled: bool) -> Notice {
    if !enabled {
        return Notice::Disabled;
    }
    match attempt.dispatcher().pane() {
        Some(pane) => Notice::Wake(pane.to_string()),
        None => Notice::NobodyToWake,
    }
}

/// Is this pane still the one that dispatched?
///
/// The pane may be gone, or holding somebody else by now, and typing a settle
/// notice into an unrelated session is worse than not sending it.
///
/// How much can be checked depends on what the board knows about the dispatcher.
/// When it dispatched the parent too there is an attempt with a checkout to
/// compare against, and the verification is the one review delivery already
/// does. The usual case is an orchestrator pane the operator started themselves:
/// there is no worktree to compare, so a live agent in a pane id that herdr does
/// not reuse is the whole of what can be established — and it is enough to rule
/// out the failure that actually bites, a pane whose agent exited leaving a
/// shell prompt to type the notice at.
pub fn still_holds_the_dispatcher(pane: &PaneInfo, dispatcher: Option<&Attempt>) -> bool {
    match dispatcher {
        Some(a) => crate::review::still_holds_the_author(pane, a),
        None => pane.agent.is_some(),
    }
}

/// The message the dispatcher is woken with.
///
/// Enough to act on without a lookup: which task, how it ended, and where the
/// result is. An orchestrator that has to run `herdr-board list` to find out
/// what it was just told is barely better off than one that was never told.
pub fn compose(task: &Task, attempt: &Attempt, settled: Settled, pr_url: Option<&str>) -> String {
    let has_pr = pr_url.is_some();
    let mut s = format!(
        "herdr-board: work you released has settled.\n\n  \
         {} · {}\n  {}\n",
        task.identifier,
        task.title,
        settled.line(has_pr)
    );
    if let Some(url) = pr_url {
        s.push_str(&format!("  {url}\n"));
    }
    if let Some(branch) = attempt.branch.as_deref() {
        s.push_str(&format!("  branch {branch}\n"));
    }
    // Only when there is no pull request to point at. With one, the URL is the
    // thing to act on and a path beside it is noise.
    if !has_pr && let Some(worktree) = attempt.worktree.as_deref() {
        s.push_str(&format!("  worktree {worktree}\n"));
    }
    s.push('\n');
    s.push_str(settled.what_now(has_pr));
    s.push_str(
        "\n\nYou are being told because you released this task. \
         `[defaults] notify_dispatcher = false` stops it.\n",
    );
    s
}

impl SyncEngine {
    /// Wake the agent that released this attempt, now that it has settled.
    ///
    /// Called after the attempt is closed, deliberately: the wake takes seconds
    /// to confirm, and a crash inside it should lose the notice rather than
    /// deliver it twice on the next tick. Never returns an error — a settle that
    /// is already recorded must not be undone because a pane would not take a
    /// prompt.
    ///
    /// Only the daemon's cycle reaches this, because it is the only caller that
    /// reconciles *with* a herdr. That falls out right: an orchestrator that
    /// chose to block on `wait` is told by what `wait` returns, and being
    /// prompted about it as well would be the same news twice.
    pub(crate) fn wake_dispatcher(
        &self,
        herdr: Option<&Herdr>,
        task: &Task,
        attempt: &Attempt,
        settled: Settled,
        pr_url: Option<&str>,
    ) {
        let pane_id = match plan_notice(attempt, self.cfg.defaults.notify_dispatcher) {
            Notice::Wake(p) => p,
            // The operator's notification already covers both of these.
            Notice::Disabled | Notice::NobodyToWake => return,
        };
        // A planned wake with no handle is a bug in the caller, not a decision
        // — it is how the event-driven reconcile silently woke nobody for a day.
        let Some(h) = herdr else {
            self.log.warn(format!(
                "{}: wanted to wake {pane_id} but this path has no herdr handle",
                task.identifier
            ));
            return;
        };

        // Asked fresh rather than read off the cycle's pane list: that list was
        // taken before reconciliation ran, and this is about to type into
        // whatever the pane holds *now*.
        let pane = match h.pane_get(&pane_id) {
            Ok(Some(p)) => p,
            Ok(None) => {
                self.log.info(format!(
                    "{}: pane {pane_id} that released it is gone — settle notice dropped",
                    task.identifier
                ));
                return;
            }
            Err(e) => {
                self.log
                    .warn(format!("{}: reading pane {pane_id}: {e}", task.identifier));
                return;
            }
        };
        let parent = self.dispatcher_attempt(attempt, &pane_id);
        if !still_holds_the_dispatcher(&pane, parent.as_ref()) {
            self.log.info(format!(
                "{}: pane {pane_id} no longer holds the agent that released it — \
                 settle notice dropped rather than delivered to whoever is there now",
                task.identifier
            ));
            return;
        }

        let text = compose(task, attempt, settled, pr_url);
        let delivery = wake::wake(h, &self.log, &pane_id, &text);
        if delivery.reached_the_pane() {
            self.log.info(format!(
                "{}: told pane {pane_id} that it {}",
                task.identifier,
                settled.toast()
            ));
        } else {
            // Nothing to retry against: the attempt is closed, so this tick was
            // the only one that would ever have carried the notice.
            self.log.warn(format!(
                "{}: could not tell pane {pane_id} that it {} — {:?}",
                task.identifier,
                settled.toast(),
                delivery
            ));
        }
    }

    /// The dispatcher's own attempt, when the board dispatched it too.
    ///
    /// `None` is the ordinary answer, not an error: most dispatching agents sit
    /// in a pane the board never started and so have no attempt anywhere.
    fn dispatcher_attempt(&self, attempt: &Attempt, pane_id: &str) -> Option<Attempt> {
        let parent = self.db.get_task(attempt.dispatched_by.as_deref()?).ok()??;
        parent
            .attempts
            .into_iter()
            .rev()
            .find(|a| a.pane_id.as_deref() == Some(pane_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentStatus;

    fn task() -> Task {
        Task {
            id: "linear:AGE-25".into(),
            source: crate::model::Source::Linear,
            source_id: "n".into(),
            identifier: "AGE-25".into(),
            title: "Tell the dispatcher when its work settles".into(),
            body: None,
            url: "https://linear.app/x/issue/AGE-25".into(),
            labels: vec![],
            state: crate::model::BoardState::Review,
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream: crate::model::UpstreamState::Unstarted,
            local_done: false,
            pr_url: None,
            pr_number: None,
            pr_open: false,
            pr_merged: false,
            pr_mergeable: None,
            updated_at: "t".into(),
            synced_at: "t".into(),
            attempts: vec![],
        }
    }

    fn attempt(dispatched_by: Option<&str>, pane: Option<&str>) -> Attempt {
        Attempt {
            id: 1,
            task_id: "linear:AGE-25".into(),
            pane_id: Some("w1:p9".into()),
            workspace: "w1".into(),
            runtime: "claude".into(),
            worktree: Some("/wt/age-25-1".into()),
            branch: Some("board/age-25".into()),
            started_at: "2026-07-29T09:00:00Z".into(),
            ended_at: None,
            outcome: None,
            missing_ticks: 0,
            agent_status: None,
            dispatched_by: dispatched_by.map(str::to_string),
            dispatched_by_pane: pane.map(str::to_string),
            base_sha: None,
            saw_working: true,
        }
    }

    fn pane_info(id: &str, agent: Option<&str>, cwd: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            workspace_id: "w1".into(),
            tab_id: None,
            agent: agent.map(str::to_string),
            agent_status: Some(AgentStatus::Idle),
            focused: false,
            label: None,
            cwd: cwd.map(str::to_string),
        }
    }

    // ---- routing ---------------------------------------------------------

    /// The guardrail the ticket asks for by name: an agent woken by every child
    /// is one that cannot hold a train of thought, so the operator opts in.
    #[test]
    fn nothing_is_woken_until_it_is_turned_on() {
        assert_eq!(
            plan_notice(&attempt(None, Some("w1:p4")), false),
            Notice::Disabled
        );
    }

    /// Operator-released work has no dispatcher. The notification is already the
    /// right answer for it, and there is no pane to wake.
    #[test]
    fn operator_released_work_wakes_nobody() {
        assert_eq!(plan_notice(&attempt(None, None), true), Notice::NobodyToWake);
    }

    /// The common topology, and the one AGE-24 made expressible: a long-lived
    /// orchestrator pane the board never dispatched, so it owns no attempt and
    /// `dispatched_by` is NULL. The pane is the whole address.
    #[test]
    fn an_orchestrator_pane_without_a_task_is_still_woken() {
        assert_eq!(
            plan_notice(&attempt(None, Some("w1:p4")), true),
            Notice::Wake("w1:p4".into())
        );
    }

    #[test]
    fn a_board_dispatched_parent_is_woken_in_its_own_pane() {
        assert_eq!(
            plan_notice(&attempt(Some("linear:AGE-24"), Some("w1:p4")), true),
            Notice::Wake("w1:p4".into())
        );
    }

    // ---- pane verification ----------------------------------------------

    /// A pane back at a shell prompt would take the notice as a command.
    #[test]
    fn a_pane_whose_agent_exited_is_not_delivered_into() {
        assert!(!still_holds_the_dispatcher(
            &pane_info("w1:p4", None, None),
            None
        ));
        assert!(still_holds_the_dispatcher(
            &pane_info("w1:p4", Some("claude"), None),
            None
        ));
    }

    /// When the board dispatched the dispatcher too, its checkout is known and
    /// a pane that has moved to another one is holding somebody else.
    #[test]
    fn a_dispatcher_pane_that_moved_checkouts_is_not_the_dispatcher() {
        let parent = attempt(None, None);
        assert!(still_holds_the_dispatcher(
            &pane_info("w1:p4", Some("claude"), Some("/wt/age-25-1")),
            Some(&parent)
        ));
        assert!(!still_holds_the_dispatcher(
            &pane_info("w1:p4", Some("claude"), Some("/wt/age-30-1")),
            Some(&parent)
        ));
    }

    // ---- the message -----------------------------------------------------

    #[test]
    fn a_finished_attempt_says_what_it_is_and_where_the_pull_request_is() {
        let m = compose(
            &task(),
            &attempt(None, Some("w1:p4")),
            Settled::Finished,
            Some("https://github.com/o/r/pull/18"),
        );
        // Identifier, outcome and PR url: everything needed to act without a
        // lookup, which is the whole point of waking anybody.
        assert!(m.contains("AGE-25"), "{m}");
        assert!(m.contains("finished — pull request open"), "{m}");
        assert!(m.contains("https://github.com/o/r/pull/18"), "{m}");
        assert!(m.contains("board/age-25"), "{m}");
        // The URL is the thing to act on; a worktree path beside it is noise.
        assert!(!m.contains("/wt/age-25-1"), "{m}");
    }

    /// The board settles on commits too, so "finished" does not imply a PR —
    /// and a dispatcher told to go and review one that does not exist would go
    /// looking for it.
    #[test]
    fn finishing_without_a_pull_request_says_so_and_points_at_the_worktree() {
        let m = compose(&task(), &attempt(None, Some("w1:p4")), Settled::Finished, None);
        assert!(m.contains("opened no pull request"), "{m}");
        assert!(m.contains("/wt/age-25-1"), "{m}");
    }

    #[test]
    fn a_failed_attempt_does_not_send_anybody_to_look_for_a_review() {
        let m = compose(
            &task(),
            &attempt(None, Some("w1:p4")),
            Settled::PaneExited,
            None,
        );
        assert!(m.contains("its pane exited"), "{m}");
        assert!(m.contains("Re-dispatch"), "{m}");
        assert!(!m.contains("Review it"), "{m}");
    }

    /// Every message says how to stop it. An agent prompted out of nowhere by
    /// something it did not ask for should not have to guess what did it.
    #[test]
    fn the_message_says_what_turned_it_on() {
        for settled in [Settled::Finished, Settled::PaneExited, Settled::NeverStarted] {
            let m = compose(&task(), &attempt(None, None), settled, None);
            assert!(m.contains("notify_dispatcher"), "{settled:?}: {m}");
        }
    }

    // ---- the two audiences stay in step ----------------------------------

    #[test]
    fn every_settle_maps_to_the_outcome_the_board_already_recorded() {
        assert_eq!(Settled::Finished.outcome(), Outcome::Done);
        assert_eq!(Settled::PaneExited.outcome(), Outcome::Orphaned);
        assert_eq!(Settled::NeverStarted.outcome(), Outcome::Orphaned);
    }

    /// The toast wordings are what the operator has been reading since before
    /// this existed; routing them through a type must not have changed them.
    #[test]
    fn the_operators_wordings_are_unchanged() {
        assert_eq!(Settled::Finished.toast(), "finished");
        assert_eq!(Settled::PaneExited.toast(), "failed — its pane exited");
        assert_eq!(
            Settled::NeverStarted.toast(),
            "failed — its agent never started"
        );
    }
}

