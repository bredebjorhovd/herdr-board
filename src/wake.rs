//! Putting text into a pane that already holds an agent, and making sure it
//! arrived.
//!
//! Two callers want this and they want the same thing: dispatch hands a new
//! agent its opening brief, and review delivery wakes a finished one with the
//! comments on its pull request. The failure they share is the one that made
//! dispatch fragile — `agent prompt` is documented to submit text plus Enter
//! atomically, and against a full-screen agent that is still settling it can
//! leave the text sitting unsent in the input box. herdr reports the send as
//! successful either way, so the only evidence of delivery is the agent leaving
//! its idle state.

use crate::herdr::Herdr;
use crate::log::Logger;
use crate::model::AgentStatus;

/// What became of a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// herdr took it and the agent started reacting.
    Started,
    /// The text pasted without submitting; an explicit enter got it moving.
    NeededEnter,
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
        matches!(self, Delivery::Started | Delivery::NeededEnter)
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
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }

    // Delivered is not the same as sent. The pane changes either way, so
    // watching the screen cannot tell the two apart; only the agent actually
    // starting work can.
    if let Err(e) = herdr.agent_prompt(target, text) {
        log.warn(format!("prompt delivery failed for {target}: {e}"));
        return Delivery::Refused(e.to_string());
    }
    if started(herdr, target, 16) {
        return Delivery::Started;
    }

    // Text pasted but never submitted. Nudge it — and only ever nudge: sending
    // the prompt a second time leaves the agent reading the same instructions
    // twice, which it notices and comments on.
    for nudge in 1..=3 {
        log.info(format!("{target} has not started; sending enter ({nudge})"));
        if herdr.agent_send_keys(target, &["enter"]).is_err() {
            break;
        }
        if started(herdr, target, 20) {
            log.info(format!("prompt for {target} needed an explicit enter"));
            return Delivery::NeededEnter;
        }
    }
    log.error(format!(
        "{target} never started work — it may be sitting on an unsent prompt"
    ));
    Delivery::Unconfirmed
}

/// Did the agent start reacting within `ticks` quarter-seconds?
fn started(herdr: &Herdr, target: &str, ticks: u32) -> bool {
    for _ in 0..ticks {
        std::thread::sleep(std::time::Duration::from_millis(250));
        match herdr.agent_status(target) {
            Some(AgentStatus::Working) | Some(AgentStatus::Blocked) => return true,
            // Gone: nothing left to wait for.
            None => return true,
            _ => {}
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
        // The text is in the box even if the agent has not moved. Sending it
        // again would leave the agent reading it twice.
        assert!(Delivery::Unconfirmed.reached_the_pane());
        assert!(!Delivery::Refused("no such agent".into()).reached_the_pane());
    }

    #[test]
    fn only_an_observed_reaction_counts_as_having_seen_it_react() {
        assert!(Delivery::Started.saw_it_react());
        assert!(Delivery::NeededEnter.saw_it_react());
        // Unconfirmed is precisely "we did not see it react", and review
        // delivery must not treat it as proof the agent is answering.
        assert!(!Delivery::Unconfirmed.saw_it_react());
        assert!(!Delivery::Refused("x".into()).saw_it_react());
    }
}
