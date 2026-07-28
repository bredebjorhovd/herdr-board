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
//! So we ship one: the active manifest plus a rule matching Claude Code's
//! on-screen working line, which prints a token counter only mid-turn.

use crate::log::Logger;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

/// Marks the rule we append, so the override can be recognised and replaced.
const RULE_ID: &str = "board_working_token_counter";

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
# Its on-screen working line looks like `· Whirring… (3m 11s · ↓ 10.5k tokens)`.
# The token counter is only printed mid-turn, which makes it the reliable part.
#
# Priority sits above `live_prompt_box` and below the blocked rules (980), so an
# agent waiting on an approval still reports blocked.
#
# The region was 6, which is roughly where the spinner sits with nothing between
# it and the prompt box — and a todo list goes exactly there. Five items pushed
# the spinner to line 10 on a live pane, the rule missed it, `live_prompt_box`
# won, and an agent running a shell command reported `idle`. Widened to cover a
# long todo list plus the four lines of prompt-box chrome beneath it.
#
# Safe to widen because the spinner is *ephemeral*: it is removed when a turn
# ends. Checked across six genuinely idle panes — none carried a token-counter
# line at all — so a larger window cannot resurrect a finished turn.
id = "board_working_token_counter"
state = "working"
priority = 976
region = "bottom_non_empty_lines(20)"
visible_working = true
line_regex = ['\u{b7}\s*\u{2193}\s*[\d.]+[kKmM]?\s*tokens']
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

/// Is our override installed?
pub fn installed() -> bool {
    override_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|s| s.contains(RULE_ID))
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
    // operator's, and herdr's docs encourage them.
    let text = std::fs::read_to_string(&path)?;
    if !text.contains(RULE_ID) {
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
        let re = regex_lite_matches;
        // Real lines captured from Claude Code 2.1.220.
        assert!(re("· Whirring… (3m 11s · ↓ 10.5k tokens)"));
        assert!(re("✻ Effecting… (29s · ↓ 1.5k tokens · thinking with high effort)"));
        assert!(re("· Topsy-turvying… (41s · ↓ 990 tokens)"));
        // ...and not an idle prompt box, which is the whole point.
        assert!(!re("❯"));
        assert!(!re("  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents"));
    }

    /// The token-counter shape the rule's regex encodes: a middot, a down
    /// arrow, a number, then `tokens`.
    fn regex_lite_matches(line: &str) -> bool {
        let Some(pos) = line.find('\u{2193}') else {
            return false;
        };
        if !line[..pos].contains('\u{b7}') {
            return false;
        }
        let rest = line[pos + '\u{2193}'.len_utf8()..].trim_start();
        let digits: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        !digits.is_empty() && rest.contains("tokens")
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
    fn the_rule_carries_its_own_explanation() {
        // Someone will find this file in six months wondering why it exists.
        assert!(RULE.contains("osc_title_working"));
        assert!(RULE.contains("live_prompt_box"));
    }
}
