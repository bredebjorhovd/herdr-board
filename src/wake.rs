//! Putting text into a pane that already holds an agent, and making sure it
//! arrived.
//!
//! Two callers want this and they want the same thing: dispatch hands a new
//! agent its opening brief, and review delivery wakes a finished one with the
//! comments on its pull request. The failure they share is the one that made
//! dispatch fragile — `agent prompt` is documented to submit text plus Enter
//! atomically, and against a full-screen agent that is still settling it can
//! leave the text sitting unsent in the input box, or drop it entirely. herdr
//! reports the send as successful either way.
//!
//! The evidence used to be the agent leaving its idle state, and that alone was
//! not enough (gh#9). Claude Code reports `idle` for the first seconds of a
//! turn — see `crate::screen` — so a delivered prompt looked undelivered, three
//! Enters went into a running turn, and dispatch logged that it may be sitting
//! on an unsent prompt whatever had actually happened. Worse, the one case that
//! really needed help — the paste that never arrived — got the same three
//! Enters pressed into an empty box, which does nothing at all.
//!
//! So delivery reads the screen too, and answers each of the three shapes with
//! the key that fits it: a running turn is done, a draft in the box wants
//! Enter, and an untouched pane wants the prompt again.

use crate::herdr::Herdr;
use crate::log::Logger;
use crate::model::AgentStatus;
use crate::screen::{self, Screen};

/// What became of a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// herdr took it and the agent started reacting.
    Started,
    /// The text pasted without submitting; an explicit enter got it moving.
    NeededEnter,
    /// The paste was swallowed — the pane was still on its welcome screen with
    /// an empty box — and a second send got it moving.
    Resent,
    /// Sent, and the agent never visibly started. It may be sitting on unsent
    /// text — or it may have answered faster than we looked.
    Unconfirmed,
    /// herdr refused it. Nothing reached the pane.
    Refused(String),
}

impl Delivery {
    /// Did anything reach the pane?
    ///
    /// Everything but a refusal did, and a caller that must not send twice —
    /// which is all of them, because an agent handed the same text twice reads
    /// it twice and says so — has to treat `Unconfirmed` as delivered.
    pub fn reached_the_pane(&self) -> bool {
        !matches!(self, Delivery::Refused(_))
    }

    /// Did we *see* the agent react? The caller that woke an idle agent uses
    /// this to know its next comment on the pull request is its own reply
    /// rather than somebody else's review.
    pub fn saw_it_react(&self) -> bool {
        matches!(
            self,
            Delivery::Started | Delivery::NeededEnter | Delivery::Resent
        )
    }
}

/// Wake an agent that is already up, and idle.
///
/// `target` is anything herdr resolves to an agent — a pane id is the right one
/// here, because the pane is what the caller verified still holds the agent it
/// means to talk to.
pub fn wake(herdr: &Herdr, log: &Logger, target: &str, text: &str) -> Delivery {
    deliver(herdr, log, target, text, false)
}

/// Hand a just-started agent its opening brief.
///
/// Waits for the agent's UI to settle first: `agent start` returns once herdr
/// *detects* the agent, but a full-screen agent is often still painting its
/// welcome screen and silently swallows a paste that arrives too early.
pub fn first_prompt(herdr: &Herdr, log: &Logger, target: &str, text: &str) -> Delivery {
    deliver(herdr, log, target, text, true)
}

fn deliver(herdr: &Herdr, log: &Logger, target: &str, text: &str, settle: bool) -> Delivery {
    if settle {
        for _ in 0..20 {
            if herdr.agent_status(target).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        // herdr knowing the agent is there is not the same as the agent being
        // able to take text: it answers before Claude Code has drawn its input
        // box, and a paste that lands first is swallowed. Wait for the box, and
        // keep a moment's grace after it appears for the paint to finish.
        for _ in 0..20 {
            if herdr
                .agent_read_visible(target)
                .is_some_and(|pane| screen::ready(&pane))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }

    if let Err(e) = herdr.agent_prompt(target, text) {
        log.warn(format!("prompt delivery failed for {target}: {e}"));
        return Delivery::Refused(e.to_string());
    }
    if started(herdr, target, text, 16) {
        return Delivery::Started;
    }

    // Three rounds of asking the screen what went wrong and answering it. Each
    // round re-reads, because the answer to one round changes the next: an
    // Enter that dismisses a menu leaves a pane that then needs the prompt.
    let mut outcome = Delivery::Unconfirmed;
    for round in 1..=3 {
        match herdr
            .agent_read_visible(target)
            .map_or(Screen::Unclear, |pane| screen::classify(&pane, text))
        {
            // herdr has not caught up, but the spinner is on screen. This is
            // the ordinary case for the first seconds of a turn, and treating
            // it as a failure is what gh#9 was.
            Screen::Working => {
                log.info(format!(
                    "{target} is working — its screen says so before herdr does"
                ));
                return if outcome == Delivery::Unconfirmed {
                    Delivery::Started
                } else {
                    outcome
                };
            }
            // Nothing ever reached this pane: an empty box, no tool markers, no
            // trace of our text. Enter has nothing to submit here, and sending
            // again cannot be read twice.
            Screen::Untouched => {
                log.info(format!(
                    "{target} never received the prompt — its box is empty; sending it again ({round})"
                ));
                if herdr.agent_prompt(target, text).is_err() {
                    break;
                }
                outcome = Delivery::Resent;
            }
            // Text sitting in the box, or a menu waiting on a key. Nudge it —
            // and only ever nudge: sending the prompt a second time on top of
            // itself leaves the agent reading the same instructions twice,
            // which it notices and comments on.
            Screen::Draft | Screen::Unclear => {
                log.info(format!("{target} has not started; sending enter ({round})"));
                if herdr.agent_send_keys(target, &["enter"]).is_err() {
                    break;
                }
                if outcome == Delivery::Unconfirmed {
                    outcome = Delivery::NeededEnter;
                }
            }
        }
        if started(herdr, target, text, 20) {
            log.info(format!(
                "prompt for {target} landed after {} ({round})",
                match outcome {
                    Delivery::Resent => "a second send",
                    _ => "an explicit enter",
                }
            ));
            return outcome;
        }
    }
    log.error(format!(
        "{target} never started work — it may be sitting on an unsent prompt"
    ));
    Delivery::Unconfirmed
}

/// Did the agent start reacting within `ticks` quarter-seconds?
///
/// Two witnesses, because neither is enough on its own. herdr's status is the
/// one that knows about a blocked agent and a pane that has gone away; the
/// screen is the one that knows about the opening seconds of a turn, which
/// herdr spends reporting `idle` (gh#9). The screen costs a subprocess, so it
/// is read once a second rather than on every tick.
fn started(herdr: &Herdr, target: &str, text: &str, ticks: u32) -> bool {
    for tick in 0..ticks {
        std::thread::sleep(std::time::Duration::from_millis(250));
        match herdr.agent_status(target) {
            Some(AgentStatus::Working) | Some(AgentStatus::Blocked) => return true,
            // Gone: nothing left to wait for.
            None => return true,
            _ => {}
        }
        if tick % 4 == 3
            && herdr
                .agent_read_visible(target)
                .is_some_and(|pane| screen::classify(&pane, text) == Screen::Working)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_refusal_means_nothing_reached_the_pane() {
        assert!(Delivery::Started.reached_the_pane());
        assert!(Delivery::NeededEnter.reached_the_pane());
        assert!(Delivery::Resent.reached_the_pane());
        // The text is in the box even if the agent has not moved. Sending it
        // again would leave the agent reading it twice.
        assert!(Delivery::Unconfirmed.reached_the_pane());
        assert!(!Delivery::Refused("no such agent".into()).reached_the_pane());
    }

    #[test]
    fn only_an_observed_reaction_counts_as_having_seen_it_react() {
        assert!(Delivery::Started.saw_it_react());
        assert!(Delivery::NeededEnter.saw_it_react());
        // A resend that took is still a reaction we watched happen.
        assert!(Delivery::Resent.saw_it_react());
        // Unconfirmed is precisely "we did not see it react", and review
        // delivery must not treat it as proof the agent is answering.
        assert!(!Delivery::Unconfirmed.saw_it_react());
        assert!(!Delivery::Refused("x".into()).saw_it_react());
    }

    #[test]
    fn the_screen_decides_which_key_a_stuck_pane_gets() {
        // The three shapes delivery answers, and the reason each answer
        // differs. Getting these two backwards is gh#9: an Enter into a running
        // turn, and a swallowed prompt left for dead.
        let prompt = "You are working on: gh#9 — detection reads working as idle";
        let welcome = "\n\n──────────\n\u{276f}\n──────────\n  \u{23f5}\u{23f5} auto mode on";
        let running = "\u{276f} You are working on: gh#9\n\u{2736} Sublimating…\n──────────\n\u{276f}\n──────────";
        let draft = format!("──────────\n\u{276f} {prompt}\n──────────");

        assert_eq!(screen::classify(welcome, prompt), Screen::Untouched);
        assert_eq!(screen::classify(running, prompt), Screen::Working);
        assert_eq!(screen::classify(&draft, prompt), Screen::Draft);
    }
}
