//! Task/attempt types and the state-derivation matrix.
//!
//! Board state is **derived**, never trusted from one side alone: upstream state
//! (Linear/GitHub) plus the live attempt (a herdr pane) produce the board state.
//! `derive_state` is the single place that decision is made; it is pure so the
//! matrix can be tested exhaustively.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Board-level task state. Note the deliberate divergence from herdr's
/// vocabulary: herdr's `done` means "agent finished, you haven't looked", which
/// is our `review`. Our `done` means the issue is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoardState {
    Blocked,
    Working,
    Ready,
    Review,
    Failed,
    Done,
}

impl BoardState {
    /// Fixed section order on the board: blocked → working → ready → review →
    /// failed → done.
    pub const SECTION_ORDER: [BoardState; 6] = [
        BoardState::Blocked,
        BoardState::Working,
        BoardState::Ready,
        BoardState::Review,
        BoardState::Failed,
        BoardState::Done,
    ];

    /// Shape-distinct glyph per state. Three shape families on purpose —
    /// pointed (`▲ ▸`), round (`● ·`), crossed (`✓ ✕`) — so every state
    /// survives color being stripped.
    pub fn glyph(self) -> &'static str {
        match self {
            BoardState::Blocked => "▲",
            BoardState::Working => "●",
            BoardState::Ready => "▸",
            BoardState::Review => "✓",
            BoardState::Failed => "✕",
            BoardState::Done => "·",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BoardState::Blocked => "BLOCKED",
            BoardState::Working => "WORKING",
            BoardState::Ready => "READY",
            BoardState::Review => "REVIEW",
            BoardState::Failed => "FAILED",
            BoardState::Done => "DONE",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BoardState::Blocked => "blocked",
            BoardState::Working => "working",
            BoardState::Ready => "ready",
            BoardState::Review => "review",
            BoardState::Failed => "failed",
            BoardState::Done => "done",
        }
    }

    pub fn parse(s: &str) -> Option<BoardState> {
        Some(match s {
            "blocked" => BoardState::Blocked,
            "working" => BoardState::Working,
            "ready" => BoardState::Ready,
            "review" => BoardState::Review,
            "failed" => BoardState::Failed,
            "done" => BoardState::Done,
            _ => return None,
        })
    }

    /// A task holding a pane. `blocked` counts — it still occupies a pane, so it
    /// counts against `max_concurrent_per_workspace`.
    pub fn holds_pane(self) -> bool {
        matches!(self, BoardState::Working | BoardState::Blocked)
    }

    /// Finished for good, with no retry left to come.
    ///
    /// Only `done` qualifies: `review` is waiting for you and `failed` is
    /// waiting for a retry, and both of those still have a use for the attempt's
    /// worktree — a retry reuses the checkout already holding the branch. This
    /// is the filter `gc` prunes by.
    pub fn is_terminal(self) -> bool {
        matches!(self, BoardState::Done)
    }
}

impl fmt::Display for BoardState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Upstream state, normalized across sources. Linear workflow states are mapped
/// by **type**, not name; GitHub has only open/closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpstreamState {
    /// Linear triage/backlog/unstarted, GitHub open with no started marker.
    Unstarted,
    /// Linear `started`-type state.
    Started,
    /// Linear completed/canceled, GitHub closed.
    Terminal,
    /// The issue itself is no longer there: a full sweep of the source stopped
    /// returning it. Never set by a poll — only by reaping, and only on a task
    /// that has attempts worth keeping (impl spec §4, AGE-6).
    Gone,
}

impl UpstreamState {
    pub fn as_str(self) -> &'static str {
        match self {
            UpstreamState::Unstarted => "unstarted",
            UpstreamState::Started => "started",
            UpstreamState::Terminal => "terminal",
            UpstreamState::Gone => "gone",
        }
    }

    pub fn parse(s: &str) -> Option<UpstreamState> {
        Some(match s {
            "unstarted" => UpstreamState::Unstarted,
            "started" => UpstreamState::Started,
            "terminal" => UpstreamState::Terminal,
            "gone" => UpstreamState::Gone,
            _ => return None,
        })
    }

    /// Nothing more is coming from upstream: the issue is closed, or the issue
    /// is not there at all. Both end the task; neither can be written back to.
    pub fn is_final(self) -> bool {
        matches!(self, UpstreamState::Terminal | UpstreamState::Gone)
    }

    /// Map a Linear workflow state *type* onto our normalized upstream state.
    pub fn from_linear_type(t: &str) -> UpstreamState {
        match t {
            "completed" | "canceled" | "cancelled" => UpstreamState::Terminal,
            "started" => UpstreamState::Started,
            // triage, backlog, unstarted, and anything unrecognized
            _ => UpstreamState::Unstarted,
        }
    }
}

/// Live agent status as reported by herdr, plus `Missing` for a pane herdr no
/// longer knows about. Mirrors herdr's `agent_status` values exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Working,
    Blocked,
    /// herdr: ready for input, tab already seen.
    Idle,
    /// herdr: same underlying idle state after unseen background work finished.
    Done,
    /// An agent is present but herdr cannot classify it. Explicitly NOT proof of
    /// successful completion.
    Unknown,
    /// The pane is gone, or herdr does not know this pane id.
    Missing,
}

impl AgentStatus {
    pub fn parse(s: &str) -> AgentStatus {
        match s {
            "working" => AgentStatus::Working,
            "blocked" => AgentStatus::Blocked,
            "idle" => AgentStatus::Idle,
            "done" => AgentStatus::Done,
            "missing" => AgentStatus::Missing,
            _ => AgentStatus::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Working => "working",
            AgentStatus::Blocked => "blocked",
            AgentStatus::Idle => "idle",
            AgentStatus::Done => "done",
            AgentStatus::Unknown => "unknown",
            AgentStatus::Missing => "missing",
        }
    }
}

/// Terminal outcome of an attempt. `None` in the DB means the attempt is live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Done,
    Failed,
    Cancelled,
    /// The pane vanished without completing.
    Orphaned,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Failed => "failed",
            Outcome::Cancelled => "cancelled",
            Outcome::Orphaned => "orphaned",
        }
    }

    pub fn parse(s: &str) -> Option<Outcome> {
        Some(match s {
            "done" => Outcome::Done,
            "failed" => Outcome::Failed,
            "cancelled" | "canceled" => Outcome::Cancelled,
            "orphaned" => Outcome::Orphaned,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Linear,
    Github,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Linear => "linear",
            Source::Github => "github",
        }
    }

    pub fn parse(s: &str) -> Option<Source> {
        Some(match s {
            "linear" => Source::Linear,
            "github" => Source::Github,
            _ => return None,
        })
    }
}

/// Everything `derive_state` is allowed to look at. Keeping this a plain struct
/// (rather than reaching into a `Task`) is what makes the matrix testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Derivation {
    pub upstream: UpstreamState,
    /// Status of the live attempt's agent, if an attempt is open.
    pub live: Option<AgentStatus>,
    /// Outcome of the most recent *closed* attempt, if any.
    pub last_outcome: Option<Outcome>,
    /// A pull request is linked to this task and still open.
    pub open_pr: bool,
    /// Operator pressed `d mark done`. Survives re-derivation until the task is
    /// re-dispatched; without it, a derived state would silently undo the key.
    pub local_done: bool,
}

impl Default for Derivation {
    fn default() -> Self {
        Derivation {
            upstream: UpstreamState::Unstarted,
            live: None,
            last_outcome: None,
            open_pr: false,
            local_done: false,
        }
    }
}

/// The state-derivation matrix (impl spec §3/§6).
///
/// Rule order is load-bearing; each arm documents why it outranks the next.
pub fn derive_state(d: Derivation) -> BoardState {
    // 1. An explicit `d mark done` is the operator overruling derivation. It has
    //    to outrank everything, otherwise the next poll recomputes the upstream
    //    state and the key undoes itself.
    if d.local_done {
        return BoardState::Done;
    }

    // 2. Closed upstream is the end of the story, and so is deleted upstream —
    //    `gone` is a task whose issue the source stopped returning. This
    //    outranks a live attempt: if the issue was closed while an agent worked,
    //    the work is moot and the operator should see it leave the queue.
    if d.upstream.is_final() {
        return BoardState::Done;
    }

    // 3. A live attempt outranks any non-terminal upstream state — in particular
    //    it outranks upstream `ready`, which is the spec's explicit rule.
    if let Some(agent) = d.live {
        return match agent {
            AgentStatus::Blocked => BoardState::Blocked,
            AgentStatus::Working => BoardState::Working,
            // The pane is gone but the attempt was never closed.
            AgentStatus::Missing => BoardState::Failed,
            // Agent settled. A PR is the only *explicit* done detection we have;
            // without one the agent may simply be between turns, so the task
            // stays `working` and the UI renders a dim `idle` marker instead of
            // inventing a state.
            AgentStatus::Idle | AgentStatus::Done | AgentStatus::Unknown => {
                if d.open_pr {
                    BoardState::Review
                } else {
                    BoardState::Working
                }
            }
        };
    }

    // 4. No live attempt. A PR outranks the closed attempt's outcome — the work
    //    landed somewhere reviewable regardless of how the pane ended.
    if d.open_pr {
        return BoardState::Review;
    }

    match d.last_outcome {
        // Both mean "needs you". Separated from `blocked` by glyph and section.
        Some(Outcome::Failed) | Some(Outcome::Orphaned) => BoardState::Failed,
        // The agent finished and nobody has looked. herdr would call this
        // `done`; on the board that is `review`.
        Some(Outcome::Done) => BoardState::Review,
        // Cancelling ends the attempt, not the issue. This is design-spec gap
        // #5: upstream sits in a `started`-type state with no live attempt and
        // no PR, and it must derive to `ready` — the issue is still owed.
        Some(Outcome::Cancelled) | None => BoardState::Ready,
    }
}

#[derive(Debug, Clone)]
// `source_state`, `updated_at` and `synced_at` are stored columns required by
// impl spec §3 and are read by `sqlite3 board.db` when debugging; the UI derives
// what it shows rather than reading them.
#[allow(dead_code)]
pub struct Task {
    pub id: String,
    pub source: Source,
    pub source_id: String,
    pub identifier: String,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    pub labels: Vec<String>,
    pub state: BoardState,
    pub source_state: Option<String>,
    pub linear_team: Option<String>,
    pub linear_project: Option<String>,
    pub upstream: UpstreamState,
    pub local_done: bool,
    pub pr_url: Option<String>,
    pub pr_number: Option<i64>,
    pub pr_open: bool,
    /// Merged, rather than closed without merging.
    pub pr_merged: bool,
    /// GitHub's `mergeable_state` — `behind` and `dirty` are the ones that
    /// matter when several branches are in flight at once.
    pub pr_mergeable: Option<String>,
    pub updated_at: String,
    pub synced_at: String,
    /// Populated by the read path, not stored on the row.
    pub attempts: Vec<Attempt>,
}

impl Task {
    /// The live attempt, if one is open.
    pub fn live_attempt(&self) -> Option<&Attempt> {
        self.attempts.iter().find(|a| a.outcome.is_none())
    }

    /// The most recent attempt that has ended, if any.
    ///
    /// The single definition of "how did the last go", shared by `derive_state`
    /// and by `list --json`. A parent agent polling for its child reads the same
    /// fact the board derives from, so the two can never disagree about whether
    /// a `ready` row was cancelled or never ran.
    pub fn last_closed_attempt(&self) -> Option<&Attempt> {
        self.attempts.iter().rev().find(|a| a.outcome.is_some())
    }

    /// Attempt count, used for the `attempt <n>` writeback comment.
    pub fn attempt_count(&self) -> usize {
        self.attempts.len()
    }
}

/// The `owner/repo` a GitHub task id names — `gh:Florin-AS/tripletex-mcp#2` →
/// `Florin-AS/tripletex-mcp`. `None` for a Linear id, which names no repo.
///
/// GitHub numbers issues per repository, so the repo is the half of a task id
/// that makes it unique. Anything keyed on the *identifier* alone — `gh#2`, and
/// the `board/gh-2` branch that used to come from it — is keyed on something
/// two repos can both answer to.
pub fn gh_repo(task_id: &str) -> Option<&str> {
    // `!` is the pull-request form of the id: `gh:owner/repo!508`.
    task_id.strip_prefix("gh:")?.split(['#', '!']).next()
}

/// Just the repository's name — `Florin-AS/tripletex-mcp` → `tripletex-mcp`.
///
/// The owner is noise when you work with a handful of repos; the name is the
/// part you read, and so the part that names branches and panes.
pub fn gh_repo_name(task_id: &str) -> Option<&str> {
    gh_repo(task_id)?.rsplit('/').next()
}

#[derive(Debug, Clone)]
pub struct Attempt {
    pub id: i64,
    pub task_id: String,
    pub pane_id: Option<String>,
    pub workspace: String,
    pub runtime: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub outcome: Option<Outcome>,
    /// Consecutive reconciliation ticks where herdr did not know this pane.
    /// Orphaning waits for 2 to avoid flapping during a live handoff.
    pub missing_ticks: i64,
    /// Live agent status, filled in by reconciliation; not a stored column.
    pub agent_status: Option<AgentStatus>,
    /// Task id of the parent that released this one, when the board dispatched
    /// that parent too. `None` on its own does **not** mean the operator: most
    /// dispatching agents sit in a pane the board never dispatched and so have
    /// no task id at all — see `dispatched_by_pane` and [`Dispatcher`].
    pub dispatched_by: Option<String>,
    /// The pane the dispatch ran from, when an agent was in it. This is the
    /// column that answers "released by" for the common case: every command
    /// carries `HERDR_PANE_ID`, whether or not the board started the pane.
    pub dispatched_by_pane: Option<String>,
    /// The commit this attempt's branch was cut from. Commits *after* it are
    /// the agent's output; commits before it are the operator's own unpushed
    /// work, which must not be mistaken for the agent having finished.
    pub base_sha: Option<String>,
    /// Whether herdr has ever reported this attempt's pane as `working`. An
    /// agent that was never seen working cannot have finished, which is what
    /// stops a freshly-started `idle` agent from being reaped before its
    /// prompt has even been delivered.
    pub saw_working: bool,
    /// Consecutive samples that looked finished on *commits alone*. Settling
    /// waits for 2, because `idle` is a screen classification that flaps while
    /// an agent works, not a stable fact — one badly-timed sample would close
    /// an attempt mid-turn (gh#18). The mirror image of `missing_ticks`; a PR
    /// bypasses it entirely.
    pub settled_ticks: i64,
    /// Digest of this pane's screen the last time it changed, and when that was
    /// — see [`crate::screen::fingerprint`]. Together they answer the one
    /// question a detection manifest cannot: whether the spinner it matched is
    /// live, or a line left in scrollback by a turn that died (gh#32).
    pub screen_print: Option<String>,
    pub screen_at: Option<String>,
}

impl Attempt {
    /// Who released this attempt, as recorded at dispatch.
    pub fn dispatcher(&self) -> Dispatcher {
        Dispatcher::agent(self.dispatched_by.clone(), self.dispatched_by_pane.clone())
    }
}

/// Who released a task.
///
/// The board dispatches *into* fresh panes, but the pane that does the
/// dispatching is usually not one of them: the common topology is one
/// long-lived orchestrator pane the operator started and keeps around, which
/// releases many children. That pane is a session, not an attempt — so asking
/// "does a live attempt own this pane" answers `None` for exactly the case
/// provenance most needs to see, and every dispatch gets recorded as the
/// operator's.
///
/// So an agent is identified by its **pane**, which every command carries in
/// `HERDR_PANE_ID` whether or not the board started it, and named by its
/// **task** as well when the board did dispatch it — a board-dispatched chain
/// keeps the richer `via LIN-138` label rather than dropping to a pane id.
///
/// `Operator` is the narrow case it sounds like: a keypress on the board, or a
/// CLI run from a pane with no agent in it. Not merely "not an attempt".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatcher {
    Operator,
    Agent {
        /// The agent's own task, when the board dispatched it. Absent for an
        /// orchestrator pane, which is the usual case.
        task: Option<String>,
        /// The pane it ran from — a delivery address, and the only identifier
        /// that is always available.
        pane: Option<String>,
    },
}

impl Dispatcher {
    /// An agent known by a task, a pane, or both. Knowing neither is the
    /// operator, so that case cannot be constructed by accident.
    pub fn agent(task: Option<String>, pane: Option<String>) -> Dispatcher {
        match (task, pane) {
            (None, None) => Dispatcher::Operator,
            (task, pane) => Dispatcher::Agent { task, pane },
        }
    }

    pub fn is_agent(&self) -> bool {
        matches!(self, Dispatcher::Agent { .. })
    }

    /// The parent's task id, when the board dispatched the parent too.
    pub fn task(&self) -> Option<&str> {
        match self {
            Dispatcher::Agent { task, .. } => task.as_deref(),
            Dispatcher::Operator => None,
        }
    }

    /// The pane the dispatch came from — where to reach the dispatcher.
    pub fn pane(&self) -> Option<&str> {
        match self {
            Dispatcher::Agent { pane, .. } => pane.as_deref(),
            Dispatcher::Operator => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d() -> Derivation {
        Derivation::default()
    }

    #[test]
    fn a_task_id_says_which_repo_a_github_number_belongs_to() {
        assert_eq!(
            gh_repo("gh:Florin-AS/tripletex-mcp#2"),
            Some("Florin-AS/tripletex-mcp")
        );
        // The pull-request form of the id, which carries `!` instead.
        assert_eq!(gh_repo("gh:bredebjorhovd/OIOS!10"), Some("bredebjorhovd/OIOS"));
        assert_eq!(gh_repo_name("gh:bredebjorhovd/OIOS!10"), Some("OIOS"));
        // A Linear id names no repo, and its identifier needs none: `LIN-142`
        // is unique across every project the board watches.
        assert_eq!(gh_repo("linear:LIN-142"), None);
        assert_eq!(gh_repo_name("linear:LIN-142"), None);
    }

    #[test]
    fn no_attempt_unstarted_is_ready() {
        assert_eq!(derive_state(d()), BoardState::Ready);
    }

    #[test]
    fn live_attempt_outranks_upstream_ready() {
        // The spec's explicit rule: an open live attempt always outranks
        // upstream `ready`.
        let s = derive_state(Derivation {
            upstream: UpstreamState::Unstarted,
            live: Some(AgentStatus::Working),
            ..d()
        });
        assert_eq!(s, BoardState::Working);
    }

    #[test]
    fn blocked_agent_is_blocked() {
        assert_eq!(
            derive_state(Derivation {
                live: Some(AgentStatus::Blocked),
                ..d()
            }),
            BoardState::Blocked
        );
    }

    #[test]
    fn idle_agent_without_pr_stays_working() {
        // "only finalize on explicit done detection or user action" — an idle
        // agent may just be between turns.
        for st in [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Unknown] {
            assert_eq!(
                derive_state(Derivation {
                    live: Some(st),
                    open_pr: false,
                    ..d()
                }),
                BoardState::Working,
                "{st:?} without a PR must stay working"
            );
        }
    }

    #[test]
    fn idle_agent_with_pr_is_review() {
        assert_eq!(
            derive_state(Derivation {
                live: Some(AgentStatus::Done),
                open_pr: true,
                ..d()
            }),
            BoardState::Review
        );
    }

    #[test]
    fn missing_pane_is_failed() {
        assert_eq!(
            derive_state(Derivation {
                live: Some(AgentStatus::Missing),
                ..d()
            }),
            BoardState::Failed
        );
    }

    #[test]
    fn terminal_upstream_is_done_even_with_live_attempt() {
        assert_eq!(
            derive_state(Derivation {
                upstream: UpstreamState::Terminal,
                live: Some(AgentStatus::Working),
                ..d()
            }),
            BoardState::Done
        );
    }

    /// A reaped task keeps its attempts, so it must not derive back into a state
    /// that invites a retry: there is no issue left to retry against. `gone` is
    /// terminal exactly like `terminal`, which is also what lets `gc` collect its
    /// checkout instead of stranding it.
    #[test]
    fn a_task_gone_from_upstream_is_done_whatever_its_last_attempt_did() {
        for outcome in [
            None,
            Some(Outcome::Cancelled),
            Some(Outcome::Failed),
            Some(Outcome::Done),
        ] {
            let s = derive_state(Derivation {
                upstream: UpstreamState::Gone,
                last_outcome: outcome,
                ..d()
            });
            assert_eq!(s, BoardState::Done, "gone + {outcome:?}");
        }
        assert!(BoardState::Done.is_terminal(), "so gc can collect it");
    }

    #[test]
    fn gone_and_terminal_are_both_final_upstream() {
        assert!(UpstreamState::Gone.is_final());
        assert!(UpstreamState::Terminal.is_final());
        assert!(!UpstreamState::Started.is_final());
        assert!(!UpstreamState::Unstarted.is_final());
    }

    #[test]
    fn upstream_states_round_trip() {
        for s in [
            UpstreamState::Unstarted,
            UpstreamState::Started,
            UpstreamState::Terminal,
            UpstreamState::Gone,
        ] {
            assert_eq!(UpstreamState::parse(s.as_str()), Some(s));
        }
    }

    /// Design-spec gap #5, resolved here: cancelling ends the attempt, not the
    /// issue. The row returns to `ready` with its history intact.
    #[test]
    fn cancelled_attempt_returns_to_ready() {
        assert_eq!(
            derive_state(Derivation {
                upstream: UpstreamState::Started,
                live: None,
                last_outcome: Some(Outcome::Cancelled),
                open_pr: false,
                local_done: false,
            }),
            BoardState::Ready
        );
    }

    #[test]
    fn started_upstream_with_nothing_live_is_ready() {
        assert_eq!(
            derive_state(Derivation {
                upstream: UpstreamState::Started,
                ..d()
            }),
            BoardState::Ready
        );
    }

    #[test]
    fn failed_and_orphaned_attempts_are_failed() {
        for o in [Outcome::Failed, Outcome::Orphaned] {
            assert_eq!(
                derive_state(Derivation {
                    last_outcome: Some(o),
                    ..d()
                }),
                BoardState::Failed
            );
        }
    }

    #[test]
    fn done_attempt_is_review_not_done() {
        // herdr's `done` is our `review`; our `done` means the issue is closed.
        assert_eq!(
            derive_state(Derivation {
                last_outcome: Some(Outcome::Done),
                ..d()
            }),
            BoardState::Review
        );
    }

    #[test]
    fn open_pr_outranks_a_failed_attempt() {
        assert_eq!(
            derive_state(Derivation {
                last_outcome: Some(Outcome::Failed),
                open_pr: true,
                ..d()
            }),
            BoardState::Review
        );
    }

    #[test]
    fn local_done_outranks_everything() {
        // Otherwise `d mark done` undoes itself on the next poll.
        assert_eq!(
            derive_state(Derivation {
                upstream: UpstreamState::Started,
                live: Some(AgentStatus::Working),
                last_outcome: Some(Outcome::Failed),
                open_pr: true,
                local_done: true,
            }),
            BoardState::Done
        );
    }

    #[test]
    fn blocked_holds_a_pane() {
        // It counts against max_concurrent_per_workspace.
        assert!(BoardState::Blocked.holds_pane());
        assert!(BoardState::Working.holds_pane());
        assert!(!BoardState::Ready.holds_pane());
        assert!(!BoardState::Review.holds_pane());
    }

    #[test]
    fn glyphs_are_shape_distinct() {
        // Rule 3: every state must survive color being stripped.
        let g: Vec<_> = BoardState::SECTION_ORDER.iter().map(|s| s.glyph()).collect();
        let mut seen = g.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), g.len(), "two states share a glyph: {g:?}");
    }

    #[test]
    fn linear_state_types_map_by_type_not_name() {
        assert_eq!(
            UpstreamState::from_linear_type("started"),
            UpstreamState::Started
        );
        assert_eq!(
            UpstreamState::from_linear_type("completed"),
            UpstreamState::Terminal
        );
        assert_eq!(
            UpstreamState::from_linear_type("canceled"),
            UpstreamState::Terminal
        );
        for t in ["triage", "backlog", "unstarted"] {
            assert_eq!(UpstreamState::from_linear_type(t), UpstreamState::Unstarted);
        }
    }
}
