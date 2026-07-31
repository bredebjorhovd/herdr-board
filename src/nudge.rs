//! Prompting an agent that an Anthropic 5xx parked, instead of leaving it parked
//! (gh#40).
//!
//! gh#32 made the stall visible: a `working` pane whose screen has not moved for
//! [`crate::screen::STALL_SECS`] is not running a turn, and when
//! [`crate::screen::api_error`] finds a 5xx on it the cause is named outright.
//! Nothing acted on it. Three agents sat parked for 30, 62 and 65 minutes holding
//! concurrency slots until the operator prompted each by hand — and prompting by
//! hand is all it took.
//!
//! So the board can do that itself. Three decisions shape what this is:
//!
//! **A nudge, not a re-dispatch.** The agent's whole context is intact; a 5xx
//! killed one turn, not the session. Re-dispatching would throw away half an
//! hour of context to recover from a transient server error, and it would put a
//! second agent on work the first one is most of the way through. So it goes
//! through [`crate::wake::wake`] — the same delivery review comments use, into
//! the pane the agent is already sitting in.
//!
//! **Capped, with a wait between tries.** Not because the load matters to
//! Anthropic; at three panes it does not. Because an agent that does not resume
//! after a few nudges is not suffering from a 5xx any more, and nudging it
//! forever would hide that behind a board that looks busy. After
//! [`MAX_NUDGES`] the attempt stays `blocked` with the reason, which is the
//! honest end state.
//!
//! **Off by default.** A board that types into panes unprompted is a different
//! thing from one that watches them, and that is opted into — `[defaults]
//! nudge_stalled = true`. It is also what keeps the board out of a fight with
//! another watcher: anything else with a retry loop of its own over the same
//! panes should have exactly one of the two turned on.
//!
//! ## Why the wait is also the proof
//!
//! The counter has to reset when the pane comes back to life, or a nudge that
//! worked would cost the agent a try the next time it needed one. But *our own
//! nudge moves the screen*: the text lands, Claude Code echoes it, and one tick
//! later the pane looks alive whether or not the agent actually resumed. Reset on
//! any movement and the counter clears itself every round, so the cap never
//! arrives and the board nudges a dead pane forever.
//!
//! What separates the two is how long the movement lasts. An agent that resumed
//! is still drawing minutes later; an echo is one frame and then stillness again.
//! So the backoff is read in both directions, and [`recovered`] is the same
//! window as [`plan`]: a pane still moving at the moment its next nudge would be
//! due is a pane the last nudge fixed.

/// How many times one stall is worth nudging.
///
/// Three, because the first is the one that almost always works — every stall in
/// gh#32 came back on a single hand-typed prompt — and the rest exist to cover a
/// pane that needed a moment rather than to grind at one that is gone.
pub const MAX_NUDGES: i64 = 3;

/// How long to leave a pane alone once it has been nudged `nudges` times.
///
/// Zero for the first: the stall has already been established by two screen
/// samples [`crate::screen::STALL_SECS`] apart, and no request is in flight —
/// [`crate::screen::api_error`] excludes the `Retrying in Ns…` line precisely
/// because a countdown is a turn that has not finished failing yet. There is
/// nothing left to wait for.
///
/// Then two minutes and five. Both are several daemon cycles, so a nudge that
/// took a while to land is not counted as one that failed; together they give an
/// agent about seven minutes to come back before the board stops asking, against
/// the half-hour-plus the panes in gh#32 sat for.
fn backoff_secs(nudges: i64) -> i64 {
    match nudges {
        0 => 0,
        1 => 120,
        _ => 300,
    }
}

/// What to do about a pane frozen on a 5xx.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Type into it. `nth` is which try this is, counting from one.
    Nudge { nth: i64 },
    /// Frozen, but nudged too recently for the stillness to mean anything yet.
    Hold { secs_left: i64 },
    /// It has had [`MAX_NUDGES`] and never came back. Nothing more to try.
    Capped,
    /// Not ours: the flag is off.
    Off,
}

/// Decide what a stalled attempt gets, given what it has already had.
///
/// Pure, so the rule is testable without a herdr, a pane or a clock. The caller
/// owns the storage and the clock and the one precondition this does not see —
/// that the screen is frozen *with a 5xx on it*, which is the only stall shape
/// worth typing into.
///
/// `since_last_secs` is how long ago the last nudge was sent, `None` when there
/// has not been one. A stamp that cannot be read lands there too, and reads as
/// "no wait outstanding": the cap is what bounds this, not the clock, and a
/// missing timestamp must not be able to freeze a sequence part-way.
pub fn plan(enabled: bool, nudges: i64, since_last_secs: Option<i64>) -> Plan {
    if !enabled {
        return Plan::Off;
    }
    if nudges >= MAX_NUDGES {
        return Plan::Capped;
    }
    let wait = backoff_secs(nudges);
    match since_last_secs {
        Some(since) if since < wait => Plan::Hold {
            secs_left: wait - since,
        },
        _ => Plan::Nudge { nth: nudges + 1 },
    }
}

/// Did the last nudge work — is this movement the agent, or our own echo?
///
/// Called when the screen has been seen to *change*, which on its own settles
/// nothing: a nudge lands as text in the pane and Claude Code echoes it, so the
/// screen moves once for a nudge that achieved nothing at all. Reset the counter
/// on that and the cap never arrives.
///
/// So the answer is the same window [`plan`] waits: a pane still moving when its
/// next nudge would have been due has been moving for minutes, which an echo is
/// not. Everything before that stays counted, and a pane that goes still again
/// inside the window simply reaches its next nudge on schedule.
pub fn recovered(nudges: i64, since_last_secs: Option<i64>) -> bool {
    nudges > 0 && since_last_secs.is_none_or(|since| since >= backoff_secs(nudges))
}

/// Should the operator hear about this stall now?
///
/// Twice at most, and never in between: once when the pane first stops moving,
/// and once if nudging gave up on it. The gap is the point. A nudge moves the
/// screen, so the attempt flips back to `working` for a tick and then to
/// `blocked` again when it re-freezes — and every one of those transitions would
/// otherwise raise a fresh notification about a stall the board is already
/// working on.
///
/// `already_blocked` is the status the attempt carried into this reconcile.
pub fn worth_saying(already_blocked: bool, nudges: i64) -> bool {
    !already_blocked && (nudges == 0 || nudges >= MAX_NUDGES)
}

/// What the pane is sent.
///
/// Short on purpose, and not only for the agent's sake. The message is echoed
/// into the pane, and everything echoed pushes the screen up — including the
/// `API Error` line this whole decision reads. A long prompt could scroll the
/// evidence off the visible screen and leave the next reconcile unable to tell
/// why the pane is frozen.
///
/// It says what happened, because the agent cannot see it: from inside the
/// session the last thing that happened is a turn that produced nothing.
pub fn message(api_error: &str) -> String {
    format!(
        "herdr-board: your last turn ended on a server error and nothing has run since \
         — {api_error}\n\n\
         Nothing else changed: the worktree, the branch and everything you had in context \
         are as you left them. Carry on from where you stopped.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_is_the_first_question_asked() {
        // Off by default, and off means nothing is typed into anything — not
        // even the first nudge, which is the one that would almost always work.
        assert_eq!(plan(false, 0, None), Plan::Off);
        assert_eq!(plan(false, 2, Some(9_000)), Plan::Off);
    }

    #[test]
    fn the_first_nudge_does_not_wait_for_anything() {
        // The stall is already two samples ten seconds apart, and `api_error`
        // has already excluded the one case where a request is still in flight.
        assert_eq!(plan(true, 0, None), Plan::Nudge { nth: 1 });
    }

    #[test]
    fn a_second_nudge_waits_out_the_first() {
        assert_eq!(plan(true, 1, Some(0)), Plan::Hold { secs_left: 120 });
        assert_eq!(plan(true, 1, Some(90)), Plan::Hold { secs_left: 30 });
        assert_eq!(plan(true, 1, Some(120)), Plan::Nudge { nth: 2 });
        // And the third waits longer than the second.
        assert_eq!(plan(true, 2, Some(120)), Plan::Hold { secs_left: 180 });
        assert_eq!(plan(true, 2, Some(300)), Plan::Nudge { nth: 3 });
    }

    #[test]
    fn the_cap_is_the_end_of_it() {
        // However long it has been. A pane that ignored three nudges is not
        // suffering from a 5xx any more, and going on hides that.
        assert_eq!(plan(true, MAX_NUDGES, Some(10_000)), Plan::Capped);
        assert_eq!(plan(true, MAX_NUDGES + 1, None), Plan::Capped);
    }

    #[test]
    fn an_unreadable_stamp_does_not_strand_a_sequence() {
        // The cap bounds this, not the clock. A timestamp that cannot be parsed
        // must not be able to hold an attempt part-way through forever.
        assert_eq!(plan(true, 1, None), Plan::Nudge { nth: 2 });
    }

    #[test]
    fn our_own_echo_does_not_clear_the_counter() {
        // The whole reason `recovered` is not "the screen moved". A nudge lands
        // as text, Claude Code echoes it, and one tick later the pane looks
        // alive whether or not the agent resumed. Clearing here is what would
        // make the cap unreachable and nudge a dead pane forever.
        assert!(!recovered(1, Some(4)));
        assert!(!recovered(1, Some(119)));
    }

    #[test]
    fn a_pane_still_moving_when_its_next_nudge_was_due_is_a_pane_that_came_back() {
        assert!(recovered(1, Some(120)));
        assert!(recovered(2, Some(300)));
        // Even a capped one: three nudges that failed do not sentence an agent
        // that later revived to a permanently empty allowance.
        assert!(recovered(MAX_NUDGES, Some(600)));
    }

    #[test]
    fn an_attempt_that_was_never_nudged_has_nothing_to_reset() {
        assert!(!recovered(0, None));
        assert!(!recovered(0, Some(10_000)));
    }

    #[test]
    fn the_operator_hears_the_start_and_the_end_and_not_the_middle() {
        // First detection: nothing has been tried yet.
        assert!(worth_saying(false, 0));
        // Mid-sequence. The board is on it, and the pane flipping between
        // `working` and `blocked` as each nudge echoes is not news.
        assert!(!worth_saying(false, 1));
        assert!(!worth_saying(false, 2));
        // Gave up. This is the state that needs a person.
        assert!(worth_saying(false, MAX_NUDGES));
        // And never twice for the same reading: a pane can sit frozen for hours.
        assert!(!worth_saying(true, 0));
        assert!(!worth_saying(true, MAX_NUDGES));
    }

    #[test]
    fn the_message_carries_the_error_and_stays_short() {
        let m = message("API Error: 529 Overloaded. This is a server-side issue, usually temporary");
        // On the first line, so it survives anything that reads the prompt a
        // line at a time — and because it is the fact the agent cannot see.
        assert!(m.lines().next().is_some_and(|l| l.contains("529 Overloaded")), "{m}");
        // Echoed into the pane, above the `API Error` line the next reconcile
        // has to read. Long enough to scroll it off and the stall loses its name.
        assert!(m.lines().count() <= 4, "{m}");
    }
}
