//! Reading a Claude Code pane ourselves, for the moments herdr's state is wrong.
//!
//! herdr classifies a pane by matching a manifest against the bottom of the
//! screen, and Claude Code gives it very little to match: the prompt box stays
//! live while a turn runs, so `live_prompt_box` reports `idle` unless something
//! outranks it. `crate::integration` ships the rule that does — but a manifest
//! is a heuristic sampled on a tick, and the *first seconds* of a turn are the
//! part that matters most to us and the part with the least on screen.
//!
//! Sampled every 300ms through a real turn on Claude Code 2.1.220
//! (herdr-board gh#9), a pane that has just been handed a prompt goes:
//!
//! ```text
//! t+0.3s  ❯                                        state=idle   rule=live_prompt_box
//! t+0.6s  ✶ Sublimating…                           state=idle   rule=live_prompt_box
//! t+2.4s  ✻ Sublimating…                           state=idle   rule=live_prompt_box
//! t+2.7s  ✽ Composing… (2s · ↓ 2 tokens)           state=idle   rule=board_working_…
//! t+3.0s  ✶ Composing… (2s · ↓ 7 tokens)           state=working
//! ```
//!
//! Two full seconds of visibly-working agent reported `idle`, because the
//! spinner carries no token counter until the first token arrives. The screen
//! said `Sublimating…` the whole time.
//!
//! So this module reads the screen directly and answers the two questions
//! delivery actually has: *is a turn running?* and *did anything reach this
//! pane at all?* It is deliberately conservative — the expensive mistake is
//! calling a pane untouched when it is holding our prompt, because that leads
//! to sending the prompt twice, and an agent handed the same brief twice reads
//! it twice and says so.

/// The spinner's animation frames. Six of them, cycling roughly every 300ms:
/// `·` `✢` `✳` `✶` `✻` `✽`.
///
/// The glyph alone proves nothing — a *finished* turn leaves `✻ Cogitated for
/// 4s` on screen, same glyph class. The ellipsis is what separates the present
/// participle from the past tense, and it is what we match on.
const SPINNER_FRAMES: [char; 6] = [
    '\u{b7}', '\u{2722}', '\u{2733}', '\u{2736}', '\u{273b}', '\u{273d}',
];

/// Claude Code's prompt marker, which starts both the live input box and every
/// echoed user message above it.
const CHEVRON: char = '\u{276f}';

/// Tool-call and tool-result markers. Their presence means somebody has used
/// this pane; their absence in a Claude pane means nobody ever has.
const TOOL_CALL: char = '\u{23fa}';
const TOOL_RESULT: char = '\u{23bf}';

/// How much of the prompt to look for on screen. Short enough to survive
/// Claude Code truncating a long first line, long enough not to match some
/// other prompt by accident.
const FRAGMENT: usize = 32;

/// What a Claude Code pane says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// A turn is in flight — the spinner is on screen. True whatever herdr
    /// says, and true two seconds before herdr says it.
    Working,
    /// Text is sitting in the input box, unsent. Enter submits it.
    Draft,
    /// Nobody has ever prompted this agent: an empty box, no tool markers, no
    /// trace of our text. A prompt sent here was swallowed and can be sent
    /// again without the agent reading anything twice.
    Untouched,
    /// The screen does not settle the question.
    Unclear,
}

/// Classify a pane snapshot against the prompt we tried to deliver into it.
pub fn classify(pane: &str, prompt: &str) -> Screen {
    if working(pane) {
        return Screen::Working;
    }
    match prompt_box(pane) {
        // A live box with something in it. That covers our own unsent paste and
        // Claude Code's own menus (`❯ 1. Yes, I trust this folder`), and Enter
        // is the right key for both.
        Some(rest) if !rest.is_empty() => Screen::Draft,
        Some(_) if untouched(pane, prompt) => Screen::Untouched,
        _ => Screen::Unclear,
    }
}

/// Is a turn running right now?
///
/// The spinner line is `<frame> <Gerund>…`, optionally followed by a timer once
/// there is anything to report: `✽ Composing… (2s · ↓ 2 tokens)`. Matching the
/// ellipsis rather than the timer is what covers the opening seconds.
pub fn working(pane: &str) -> bool {
    pane.lines().any(is_spinner)
}

fn is_spinner(line: &str) -> bool {
    let line = line.trim_end();
    let mut chars = line.trim_start().chars();
    let Some(frame) = chars.next() else {
        return false;
    };
    if !SPINNER_FRAMES.contains(&frame) {
        return false;
    }
    // A frame, then a space, then a word: `✽ Composing…`. Without the gap this
    // would match a middot-led bullet.
    let rest = chars.as_str();
    if !rest.starts_with(' ') {
        return false;
    }
    let rest = rest.trim_start();
    if rest.is_empty() {
        return false;
    }
    // The ellipsis is the tense marker: `Composing…` is now, `Cogitated for 4s`
    // is over. Ending the line or opening a timer, and nothing else — the
    // ellipsis Claude Code uses to elide a long line sits mid-sentence.
    rest.match_indices('\u{2026}').any(|(at, e)| {
        let tail = rest[at + e.len()..].trim_start();
        tail.is_empty() || tail.starts_with('(')
    })
}

/// Has the TUI finished painting enough to accept text?
///
/// `agent start` returns when herdr *detects* the agent, which is well before
/// Claude Code has drawn its input box — and a paste that lands in between is
/// swallowed without trace. The box on screen is the readiness signal herdr
/// does not offer.
pub fn ready(pane: &str) -> bool {
    prompt_box(pane).is_some()
}

/// The live input box's contents, or `None` if no box is on screen.
///
/// The box is the *last* chevron line: every submitted message is echoed with
/// the same marker further up, so the first one on screen is history.
fn prompt_box(pane: &str) -> Option<&str> {
    pane.lines()
        .filter_map(|l| l.trim_start().strip_prefix(CHEVRON))
        .next_back()
        .map(str::trim)
}

/// Has this pane ever been used?
///
/// Three independent signs of use, because sending a prompt twice is the one
/// mistake worth spending caution on: the tool markers Claude Code prints for
/// every call it makes, and our own text echoed somewhere on screen.
fn untouched(pane: &str, prompt: &str) -> bool {
    if pane.contains(TOOL_CALL) || pane.contains(TOOL_RESULT) {
        return false;
    }
    match fragment(prompt) {
        // Nothing distinctive to look for; the markers are the whole answer.
        None => true,
        Some(f) => !squash(pane).contains(&f),
    }
}

/// A distinctive opening of the prompt, normalised the way the screen will have
/// mangled it — Claude Code wraps a long line and indents the continuation.
fn fragment(prompt: &str) -> Option<String> {
    let line = prompt.lines().map(str::trim).find(|l| !l.is_empty())?;
    let f = squash(line);
    if f.len() < 8 {
        return None;
    }
    Some(f.chars().take(FRAGMENT).collect())
}

/// Collapse every run of whitespace to one space, so wrapped text compares
/// equal to the text it was wrapped from.
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a live pane 600ms after `herdr agent prompt` (gh#9). The
    /// spinner has no timer yet — this is the state that reported `idle` and
    /// made dispatch send three blind Enters into a running turn.
    const WORKING_EARLY: &str = "\
❯ Count the files in /tmp with ls, then say the number and nothing else.
✶ Sublimating…
──────────────────────────────────────────
❯
──────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents";

    /// The same pane two seconds later, once the token counter appears.
    const WORKING_COUNTER: &str = "\
  ⎿  $ ls /tmp | wc -l
✽ Composing… (2s · ↓ 2 tokens)
──────────────────────────────────────────
❯
──────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents";

    /// A long-running agent with a todo list between spinner and box — the
    /// shape from the issue report.
    const WORKING_TODO: &str = "\
⏺ Update(packages/observability/package.json)
  ⎿  Added 1 line

✳ Setting up monorepo root tooling… (6m 29s · ↓ 23.9k tokens · thinking with high effort)
  ⎿  ◼ Set up monorepo root tooling
     ◻ Scaffold shared packages

──────────────────────────────────────────
❯
──────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle)";

    /// A finished turn. Same glyph class, past tense, no ellipsis.
    const IDLE_DONE: &str = "\
⏺ 111
✻ Cogitated for 4s
──────────────────────────────────────────
❯
──────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents";

    /// Finished, with a shell still running in the background — captured live,
    /// and the closest an idle screen comes to looking busy.
    const IDLE_SHELL: &str = "\
⏺ OIOS-side everything remains clean: no watchers, no vercel processes.
✻ Baked for 1m 32s · 1 shell still running

※ recap: Goal: stop the flood of Vercel Authorize browser tabs.
──────────────────────────────────────────
❯
──────────────────────────────────────────
  ⏵⏵ auto mode on · 1 shell · ← for agents";

    /// Idle with text typed into the box and never sent.
    const IDLE_DRAFT: &str = "\
⏺ 111
✻ Churned for 19s
──────────────────────────────────────────
❯ update the WP-6 file with what was done
──────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle) · PR #508 · ← for agents";

    /// A freshly started agent nobody has prompted — what dispatch found after
    /// its paste was swallowed, three Enters pressed into it.
    const WELCOME: &str = "\n\n\n\n                       ● high · /effort\n\
──────────────────────────────────────────\n\
❯\n\
──────────────────────────────────────────\n\
  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents";

    /// Claude Code's trust prompt, which a fresh worktree gets on first launch.
    /// herdr reports it `idle` with no matching rule at all.
    const TRUST_DIALOG: &str = "\
 Quick safety check: Is this a project you created or one you trust?

 ❯ 1. Yes, I trust this folder
   2. No, exit

 Enter to confirm · Esc to cancel";

    const PROMPT: &str = "You are working on: claude detection manifest reads a working agent as idle\n\nWork in this worktree.";

    #[test]
    fn a_spinner_without_a_timer_still_means_working() {
        // The whole point of gh#9: for the first two seconds of a turn this is
        // all there is, and a rule that waits for `↓ N tokens` misses it.
        assert!(working(WORKING_EARLY));
        assert!(working(WORKING_COUNTER));
        assert!(working(WORKING_TODO));
    }

    #[test]
    fn a_finished_turn_is_not_working() {
        // `✻ Cogitated for 4s` shares the spinner's glyph and its position. The
        // tense is the only difference, and the ellipsis is the tense.
        assert!(!working(IDLE_DONE));
        assert!(!working(IDLE_SHELL));
        assert!(!working(IDLE_DRAFT));
        assert!(!working(WELCOME));
    }

    #[test]
    fn every_spinner_frame_counts() {
        for frame in SPINNER_FRAMES {
            assert!(
                working(&format!("{frame} Sublimating…")),
                "frame {frame} missed"
            );
        }
    }

    #[test]
    fn an_ellipsis_mid_sentence_is_not_a_spinner() {
        // Claude Code elides long lines with the same character. A spinner's
        // ellipsis ends the phrase, or hands over to the timer.
        assert!(!working("· Reading …/some/elided/path.rs and moving on"));
        assert!(working("· Whirring… (3m 11s · ↓ 10.5k tokens)"));
        assert!(working(
            "✻ Effecting… (29s · ↓ 1.5k tokens · thinking with high effort)"
        ));
    }

    #[test]
    fn a_bullet_is_not_a_spinner() {
        assert!(!working("·Composing…"));
        assert!(!working("· "));
        assert!(!working("  - one thing…"));
    }

    #[test]
    fn an_untouched_pane_is_the_one_safe_place_to_send_again() {
        assert_eq!(classify(WELCOME, PROMPT), Screen::Untouched);
    }

    #[test]
    fn a_pane_holding_our_text_is_never_untouched() {
        // Sending again here would leave the agent reading the brief twice.
        let holding = WELCOME.replace(
            "❯\n",
            "❯ You are working on: claude detection manifest reads a working\n",
        );
        assert_eq!(classify(&holding, PROMPT), Screen::Draft);

        // ...and neither is one that has already answered it, even with the box
        // empty and the echo scrolled away.
        assert_eq!(classify(IDLE_DONE, PROMPT), Screen::Unclear);
        assert_eq!(classify(IDLE_SHELL, PROMPT), Screen::Unclear);
    }

    #[test]
    fn a_wrapped_echo_still_counts_as_our_text() {
        // The screen wraps and indents; the comparison has to survive that.
        let wrapped = WELCOME.replace(
            "❯\n",
            "❯ You are working on: claude detection\n  manifest reads a working agent as idle\n❯\n",
        );
        assert!(!untouched(&wrapped, PROMPT));
    }

    #[test]
    fn a_running_turn_beats_everything_else_on_screen() {
        assert_eq!(classify(WORKING_EARLY, PROMPT), Screen::Working);
        assert_eq!(classify(WORKING_TODO, PROMPT), Screen::Working);
    }

    #[test]
    fn a_menu_is_a_draft_because_enter_is_what_it_wants() {
        // The trust prompt a fresh worktree opens on. herdr has no rule for it,
        // so it reads `idle`; pressing Enter is both what clears it and what we
        // would have done anyway.
        assert_eq!(classify(TRUST_DIALOG, PROMPT), Screen::Draft);
    }

    #[test]
    fn a_pane_is_ready_once_its_box_is_drawn() {
        // The shell before Claude Code paints, and the pane a moment later.
        assert!(!ready("brede@host tmp % claude\n"));
        assert!(ready(WELCOME));
        assert!(ready(TRUST_DIALOG));
    }

    #[test]
    fn a_pane_with_no_box_at_all_settles_nothing() {
        assert_eq!(classify("some shell output\n$ ", PROMPT), Screen::Unclear);
    }

    #[test]
    fn the_last_chevron_is_the_box_and_the_ones_above_are_history() {
        // Every submitted message is echoed with the same marker, so reading
        // the first one would call an empty box a draft forever.
        assert_eq!(prompt_box(WORKING_EARLY), Some(""));
        assert_eq!(
            prompt_box(IDLE_DRAFT),
            Some("update the WP-6 file with what was done")
        );
    }

    #[test]
    fn a_prompt_too_short_to_recognise_leans_on_the_markers() {
        assert_eq!(fragment("hi"), None);
        assert!(untouched(WELCOME, "hi"));
        assert!(!untouched(IDLE_DONE, "hi"));
    }
}
