//! Making herdr report Claude Code's state correctly.
//!
//! herdr classifies agent state by matching TOML rules against the bottom of
//! the pane. Its stock Claude Code manifest has exactly one general `working`
//! rule — `osc_title_working`, which matches a braille spinner in the *terminal
//! title* — and Claude Code emits no title in a herdr pane, so that rule can
//! never fire. Meanwhile `live_prompt_box` (priority 950) matches the prompt
//! box, which Claude Code keeps on screen while it works and underneath its own
//! approval dialogs. The result: a thinking or blocked Claude agent reports
//! `idle`, and the board's `blocked` section never fires for one.
//!
//! Hooks cannot fix this. herdr accepts `pane report-agent` from any source, but
//! the docs are explicit that Claude Code's state authority is the screen
//! manifest and that its integrations are "intentionally not lifecycle
//! authorities" — reports are taken and ignored. herdr's own
//! `integration install claude` only reports session identity.
//!
//! What herdr does support is a local manifest override, which "always wins".
//! So we ship one: the active manifest plus a rule matching the spinner line
//! Claude Code keeps on screen for the length of a turn.
//!
//! A manifest is still a heuristic sampled on a tick, and `crate::screen` reads
//! the same pane directly for the callers that cannot afford to be wrong about
//! a single sample.

use crate::log::Logger;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

/// Marks the rule we append, so the override can be recognised and replaced.
const RULE_ID: &str = "board_working_spinner";

/// The id this rule had before gh#9. An override carrying it is ours, and out
/// of date: it waits for a token counter that the opening seconds of a turn do
/// not have.
const OLD_RULE_ID: &str = "board_working_token_counter";

/// Above `live_prompt_box` (950) so working beats a live prompt box, and below
/// the blocked rules (980) so an approval prompt still wins — being told an
/// agent needs you matters more than being told it is busy.
const RULE: &str = r#"
[[rules]]
# Added by herdr-board. See `herdr-board integration install claude`.
#
# Claude Code's only general `working` rule upstream is `osc_title_working`,
# which matches a braille spinner in the terminal title — and Claude Code emits
# no title in a herdr pane, so nothing can ever match it. The prompt box stays
# live while it works, so `live_prompt_box` (950) wins and a thinking agent
# reports idle.
#
# Priority sits above `live_prompt_box` and below the blocked rules (980), so an
# agent waiting on an approval still reports blocked.
#
# What this matches is the spinner line Claude Code keeps just above the prompt
# box for the length of a turn: a rotating frame, a gerund, and an ellipsis,
# with a timer once there is anything to report.
#
#     ✶ Sublimating…
#     ✽ Composing… (2s · ↓ 2 tokens)
#     ✳ Setting up monorepo root tooling… (6m 29s · ↓ 23.9k tokens)
#
# The ellipsis is the whole discriminator, because a *finished* turn leaves a
# line of the same shape and the same glyph one line higher:
#
#     ✻ Cogitated for 4s
#     ✻ Baked for 1m 32s · 1 shell still running
#
# Present participle versus past tense. The frames are `·` `✢` `✳` `✶` `✻` `✽`.
#
# This rule used to match the `· ↓ N tokens` counter instead, which is printed
# only once the first token arrives. Sampling a real pane every 300ms through a
# turn (gh#9) showed the counter appearing 2.7s after the prompt landed, and the
# pane reporting `idle` for every sample before that — right across the window
# where dispatch checks whether its prompt was delivered. The counter is kept as
# a second alternative in case a future spinner drops the ellipsis.
#
# The region was 6, which is roughly where the spinner sits with nothing between
# it and the prompt box — and a todo list goes exactly there. Five items pushed
# the spinner to line 10 on a live pane, the rule missed it, `live_prompt_box`
# won, and an agent running a shell command reported `idle`. Widened to cover a
# long todo list plus the four lines of prompt-box chrome beneath it.
#
# The widening was justified with "safe, because the spinner is ephemeral: it is
# removed when a turn ends", checked against six idle panes that carried no
# spinner line at all. That was wrong, and gh#32 is the bill. Claude Code does
# leave spinner-glyph lines in scrollback — a stale `✳ Tinkering…` from a turn
# that died on an Anthropic 5xx, and past-tense summaries like `✻ Crunched for
# 3m 19s` — and 20 non-empty lines reaches them. Three attempts reported
# `working` for over an hour with nothing running in them, holding concurrency
# slots, unable ever to settle.
#
# The region is still 20, deliberately. Narrowing it only trades this failure for
# the todo-list one it was widened to fix, and no width fixes either: the fact
# that separates a live spinner from a frozen one is *change over time*, which a
# rule matching one screenshot does not contain. So the backstop is not here —
# `crate::screen::vitals` resamples the pane across reconciles and overrules a
# `working` whose screen has stopped moving.
id = "board_working_spinner"
state = "working"
priority = 976
region = "bottom_non_empty_lines(20)"
visible_working = true
line_regex = ['(?:^\s*[\x{b7}\x{2722}\x{2733}\x{2736}\x{273b}\x{273d}] +\S[^\n]*\x{2026}\s*(?:\(|$))|(?:\x{b7}\s*\x{2193}\s*[\d.]+[kKmM]?\s*tokens)']
"#;

fn config_home() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home))
}

/// Where herdr reads local agent-detection overrides from.
pub fn override_path() -> Result<PathBuf> {
    Ok(config_home()?
        .join(".config/herdr/agent-detection")
        .join("claude.toml"))
}

/// The manifest herdr is using today, which the override is built from.
fn active_manifest() -> Result<String> {
    let remote = config_home()?
        .join(".local/state/herdr/agent-detection/remote")
        .join("claude.toml");
    if remote.exists() {
        return std::fs::read_to_string(&remote)
            .with_context(|| format!("reading {}", remote.display()));
    }
    bail!(
        "no Claude Code manifest at {} — run `herdr server update-agent-manifests` first",
        remote.display()
    )
}

/// What is sitting at the override path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No override of ours — herdr is reading the stock manifest, or one the
    /// operator wrote.
    Missing,
    /// Ours, from before gh#9: it waits for a token counter, so it misses the
    /// opening seconds of every turn.
    Stale,
    /// Ours, current.
    Current,
}

/// What state our override is in.
pub fn status() -> Status {
    let text = override_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    if text.contains(RULE_ID) {
        Status::Current
    } else if text.contains(OLD_RULE_ID) {
        Status::Stale
    } else {
        Status::Missing
    }
}

/// Is an override of ours installed at all, current or not?
pub fn installed() -> bool {
    status() != Status::Missing
}

/// Write the override: herdr's current manifest plus our rule.
///
/// A local override replaces the manifest wholesale, so this snapshots the
/// active one. That means it also shadows later upstream updates — `doctor`
/// says so, and `uninstall` puts things back.
pub fn install(log: &Logger) -> Result<()> {
    let base = active_manifest()?;
    if base.contains(RULE_ID) {
        bail!("the active manifest already defines {RULE_ID}");
    }
    let path = override_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, format!("{}\n{}", base.trim_end(), RULE))?;
    log.info(format!("wrote {}", path.display()));
    Ok(())
}

pub fn uninstall(log: &Logger) -> Result<()> {
    let path = override_path()?;
    if !path.exists() {
        return Ok(());
    }
    // Only ever remove a file we wrote: a hand-written override is the
    // operator's, and herdr's docs encourage them. An override from before
    // gh#9 is still one of ours.
    let text = std::fs::read_to_string(&path)?;
    if !text.contains(RULE_ID) && !text.contains(OLD_RULE_ID) {
        bail!(
            "{} exists but is not ours — leaving it alone",
            path.display()
        );
    }
    std::fs::remove_file(&path)?;
    log.info(format!("removed {}", path.display()));
    Ok(())
}

/// Only Claude Code needs the override — and this is now checked rather than
/// assumed.
///
/// The refusal used to rest on a belief nobody had tested, which mattered
/// because of the AGE-19 guard: an attempt may only settle on commits once herdr
/// has reported it `working` at least once. A runtime herdr never reports
/// `working` for could therefore never settle without a PR, and would sit
/// `working` forever.
///
/// Checked under AGE-26 by dispatching a real task to each and watching the row
/// through its whole life. Both went `working` → `review` on commits alone, no
/// PR involved, and the daemon logged `agent done with commits — attempt done`:
///
/// - **codex** has `screen_working_fallback` (priority 500), matching its
///   on-screen `• Working (7s • esc to interrupt)` line. Its manifest has no
///   `live_prompt_box` rule to outrank it, which is the whole of Claude Code's
///   problem.
/// - **opencode** has `interrupt_hint_working` (priority 110), matching the
///   `esc to interrupt` hint it shows while running.
pub fn check_supported(agent: &str) -> Result<()> {
    if agent != "claude" {
        bail!(
            "only `claude` needs this — herdr reads codex and opencode from the \
             screen correctly, verified by dispatching to both (AGE-26)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rule_outranks_idle_but_not_blocked() {
        // The ordering is the whole design: a working agent must beat a live
        // prompt box, and an agent waiting on you must beat both.
        let priority: u32 = RULE
            .lines()
            .find_map(|l| l.strip_prefix("priority = ")?.trim().parse().ok())
            .expect("the rule declares a priority");
        assert!(priority > 950, "must outrank live_prompt_box");
        assert!(priority < 980, "must not outrank the blocked rules");
    }

    #[test]
    fn the_rule_matches_claude_codes_working_line() {
        // Real lines captured from Claude Code 2.1.220.
        assert!(matches("· Whirring… (3m 11s · ↓ 10.5k tokens)"));
        assert!(matches(
            "✻ Effecting… (29s · ↓ 1.5k tokens · thinking with high effort)"
        ));
        assert!(matches("· Topsy-turvying… (41s · ↓ 990 tokens)"));
        // The one gh#9 is about: the first seconds of a turn, before the token
        // counter exists.
        assert!(matches("✶ Sublimating…"));
        // ...and not an idle prompt box, which is the whole point.
        assert!(!matches("❯"));
        assert!(!matches(
            "  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents"
        ));
        // ...nor the past-tense line a finished turn leaves behind.
        assert!(!matches("✻ Cogitated for 4s"));
        assert!(!matches("✻ Baked for 1m 32s · 1 shell still running"));
    }

    /// The rule's regex, evaluated. herdr owns the engine and this crate has no
    /// regex dependency, so `crate::screen` is the executable copy of the
    /// pattern and the two are asserted to agree below.
    fn matches(line: &str) -> bool {
        crate::screen::working(line)
    }

    #[test]
    fn the_regex_and_the_screen_reader_encode_the_same_line() {
        // Two readings of the same screen — herdr's, via this regex, and ours,
        // via `screen::working`. They drift apart silently, so tie them
        // together by construction: every frame and the ellipsis appear in both.
        let pattern = RULE
            .lines()
            .find_map(|l| l.strip_prefix("line_regex = "))
            .expect("the rule declares a line_regex");
        for frame in [
            '\u{b7}', '\u{2722}', '\u{2733}', '\u{2736}', '\u{273b}', '\u{273d}',
        ] {
            let escaped = format!("\\x{{{:x}}}", frame as u32);
            assert!(
                pattern.contains(&escaped),
                "frame {frame} ({escaped}) is missing from the manifest regex"
            );
            assert!(
                crate::screen::working(&format!("{frame} Sublimating…")),
                "frame {frame} is missing from screen::working"
            );
        }
        assert!(
            pattern.contains("\\x{2026}"),
            "the ellipsis is the discriminator; the regex has to require it"
        );
    }

    #[test]
    fn the_region_clears_a_todo_list() {
        // Seen live (herdr-board#9): a five-item todo sits between the spinner
        // and the prompt box, putting the spinner ten non-empty lines from the
        // bottom. A region of 6 missed it, `live_prompt_box` won at 950, and an
        // agent visibly running a shell command reported `idle`.
        //
        // The floor: four lines of prompt-box chrome (rule, `❯`, rule, auto
        // mode) plus a todo list plus its header. Anything under about 16 is
        // one long todo away from the same failure.
        let region: usize = RULE
            .lines()
            .find_map(|l| {
                l.strip_prefix("region = \"bottom_non_empty_lines(")?
                    .split(')')
                    .next()?
                    .parse()
                    .ok()
            })
            .expect("the rule declares a bottom_non_empty_lines region");
        assert!(
            region >= 16,
            "region {region} leaves no room for a todo list above the prompt box"
        );
    }

    #[test]
    fn the_rule_admits_that_a_region_this_wide_needs_a_backstop() {
        // gh#32: the comment above the rule used to argue the widening was safe
        // because a finished turn leaves no spinner behind. It does, in
        // scrollback, and three attempts sat `working` for an hour on the
        // strength of it. Whoever narrows this region next should find out here
        // why that is not the fix, and what is.
        assert!(RULE.contains("gh#32"));
        assert!(
            RULE.contains("crate::screen::vitals"),
            "the rule has to point at what actually catches a frozen spinner"
        );
    }

    #[test]
    fn the_rule_carries_its_own_explanation() {
        // Someone will find this file in six months wondering why it exists.
        assert!(RULE.contains("osc_title_working"));
        assert!(RULE.contains("live_prompt_box"));
    }

    #[test]
    fn an_override_from_before_gh9_reads_as_stale_not_missing() {
        // The two ids are what tells "never installed" from "installed, and now
        // wrong". Reinstalling is the fix for the second, and `doctor` can only
        // say so if it can see the difference.
        assert_ne!(RULE_ID, OLD_RULE_ID);
        assert!(RULE.contains(RULE_ID));
        assert!(!RULE.contains(OLD_RULE_ID));
    }
}
