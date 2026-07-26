//! Reporting Claude Code's lifecycle state to herdr ourselves.
//!
//! herdr detects agent state by reading the screen, and its rule for Claude
//! Code matches the live prompt box (`live_prompt_box`, priority 950). Claude
//! Code keeps that box on screen while it works and underneath its own approval
//! dialogs, so herdr reports `idle` for an agent that is thinking, and — worse —
//! for one that is blocked waiting on you. The `blocked` section of the board
//! never fires for a Claude pane, which is the one state the board exists to
//! surface.
//!
//! herdr's own `integration install claude` does not fix this: it installs a
//! single `SessionStart` hook whose only call is `pane.report_agent_session`,
//! which records *which* session occupies a pane and explicitly not its
//! lifecycle state.
//!
//! `pane report-agent` exists for exactly this, and a source reporting full
//! lifecycle takes authority over screen detection. So we install our own
//! hooks. This is an intrusion into the operator's global Claude Code settings,
//! so it is opt-in, reversible, and never touches an entry it did not write.

use crate::log::Logger;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::PathBuf;

/// Marks the settings entries we own, so uninstall can remove exactly those.
const MARKER: &str = "herdr-board-agent-state.sh";

/// `--source` for our reports. herdr keeps at most 32 sequenced sources per
/// pane, so this stays a single stable id.
const SOURCE: &str = "offhand.board.claude";

/// Claude Code hook event → the state it implies.
const HOOKS: &[(&str, &str)] = &[
    // The operator (or the board) sent a prompt: the agent is now working.
    ("UserPromptSubmit", "working"),
    // Tool calls keep a long turn looking alive rather than ageing into doubt.
    ("PreToolUse", "working"),
    // The turn ended. Ready for input.
    ("Stop", "idle"),
    ("StopFailure", "idle"),
    // Claude Code raises these when it wants a human: permission prompts and
    // attention notifications. This is the case screen detection misses.
    ("Notification", "blocked"),
    ("PermissionRequest", "blocked"),
    // The session is over; hand authority back rather than pinning a dead state.
    ("SessionEnd", "release"),
];

const SCRIPT: &str = r#"#!/bin/sh
# Installed by herdr-board. Reports Claude Code's lifecycle state to herdr,
# because herdr's screen detection reads a live prompt box as `idle` even while
# Claude Code is working or waiting on an approval.
#
# Managed file: `herdr-board integration uninstall claude` removes it.
#
# This runs on every Claude Code hook event, so it must be cheap, must never
# block, and must never fail in a way Claude Code notices.
set -eu

state="${1:-}"

# Hooks are handed JSON on stdin. Drain it: leaving it unread can block the
# writer.
cat >/dev/null 2>&1 || true

# Only meaningful inside a herdr pane. Outside one this is a no-op, which is
# what makes the hook safe to install globally.
[ "${HERDR_ENV:-}" = "1" ] || exit 0
[ -n "${HERDR_PANE_ID:-}" ] || exit 0

herdr="${HERDR_BIN_PATH:-herdr}"
command -v "$herdr" >/dev/null 2>&1 || exit 0

# Monotonic enough to order reports; herdr drops stale ones from the same
# source rather than letting a late `working` overwrite a fresh `idle`.
seq="$(date +%s 2>/dev/null || echo 0)"

if [ "$state" = "release" ]; then
  "$herdr" pane release-agent "$HERDR_PANE_ID" \
    --source SOURCE_ID --agent claude --seq "$seq" >/dev/null 2>&1 || true
else
  "$herdr" pane report-agent "$HERDR_PANE_ID" \
    --source SOURCE_ID --agent claude --state "$state" --seq "$seq" >/dev/null 2>&1 || true
fi

# Never fail a hook: a non-zero exit here would surface inside Claude Code.
exit 0
"#;

fn claude_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".claude"))
}

pub fn hook_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join("hooks").join(MARKER))
}

pub fn settings_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join("settings.json"))
}

/// Write the hook script and register it for each event.
pub fn install(log: &Logger) -> Result<()> {
    let hook = hook_path()?;
    std::fs::create_dir_all(hook.parent().unwrap())?;
    std::fs::write(&hook, SCRIPT.replace("SOURCE_ID", SOURCE))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    log.info(format!("wrote {}", hook.display()));

    let settings = settings_path()?;
    let mut root = read_settings(&settings)?;
    // Back up before touching global settings — this is the operator's file,
    // shared with every other tool that writes hooks.
    if settings.exists() {
        let backup = settings.with_extension("json.herdr-board.bak");
        std::fs::copy(&settings, &backup)?;
        log.info(format!("backed up settings to {}", backup.display()));
    }

    let command = format!("sh '{}'", hook.display());
    let hooks = root
        .as_object_mut()
        .context("settings.json is not an object")?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .context("settings.json `hooks` is not an object")?;

    for (event, state) in HOOKS {
        let list = hooks
            .entry(event.to_string())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("settings.json hooks.{event} is not an array"))?;
        // Replace our own entry rather than appending a second one.
        list.retain(|e| !is_ours(e));
        list.push(json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": format!("{command} {state}"),
                "timeout": 5
            }]
        }));
    }

    write_settings(&settings, &root)?;
    log.info(format!("registered {} hooks", HOOKS.len()));
    Ok(())
}

/// Remove our hook entries and the script. Never touches anything else.
pub fn uninstall(log: &Logger) -> Result<()> {
    let settings = settings_path()?;
    if settings.exists() {
        let mut root = read_settings(&settings)?;
        if let Some(hooks) = root
            .get_mut("hooks")
            .and_then(Value::as_object_mut)
        {
            for list in hooks.values_mut() {
                if let Some(list) = list.as_array_mut() {
                    list.retain(|e| !is_ours(e));
                }
            }
            // Leave no empty arrays behind.
            hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
        }
        write_settings(&settings, &root)?;
        log.info("removed hook entries from settings.json");
    }
    let hook = hook_path()?;
    if hook.exists() {
        std::fs::remove_file(&hook)?;
        log.info(format!("removed {}", hook.display()));
    }
    Ok(())
}

/// How many of our hook entries are registered.
pub fn installed_count() -> usize {
    let Ok(settings) = settings_path() else {
        return 0;
    };
    let Ok(root) = read_settings(&settings) else {
        return 0;
    };
    root.get("hooks")
        .and_then(Value::as_object)
        .map(|hooks| {
            hooks
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .filter(|e| is_ours(e))
                .count()
        })
        .unwrap_or(0)
}

pub fn expected_count() -> usize {
    HOOKS.len()
}

/// Is this settings entry one we wrote?
fn is_ours(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(MARKER))
            })
        })
        .unwrap_or(false)
}

fn read_settings(path: &std::path::Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn write_settings(path: &std::path::Path, root: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(root)?;
    // Write via a temporary file: a half-written settings.json would break
    // every Claude Code session on the machine.
    let tmp = path.with_extension("json.herdr-board.tmp");
    std::fs::write(&tmp, format!("{text}\n"))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

pub fn check_supported(agent: &str) -> Result<()> {
    if agent != "claude" {
        bail!(
            "only `claude` needs this: herdr detects the other runtimes' state \
             correctly from the screen"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ours() -> Value {
        json!({
            "matcher": "*",
            "hooks": [{ "type": "command", "command": format!("sh '/x/{MARKER}' working") }]
        })
    }

    fn theirs() -> Value {
        json!({
            "matcher": "*",
            "hooks": [{ "type": "command", "command": "bash '/x/someone-elses.sh'" }]
        })
    }

    #[test]
    fn only_our_own_entries_are_recognised() {
        assert!(is_ours(&ours()));
        assert!(!is_ours(&theirs()));
        assert!(!is_ours(&json!({})));
    }

    #[test]
    fn uninstall_leaves_other_tools_hooks_alone() {
        // The operator's settings.json is shared with every other tool that
        // writes hooks; removing one of theirs would be unforgivable.
        let mut list = vec![theirs(), ours(), theirs()];
        list.retain(|e| !is_ours(e));
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|e| !is_ours(e)));
    }

    #[test]
    fn every_hook_event_maps_to_a_state_herdr_accepts() {
        for (event, state) in HOOKS {
            assert!(
                matches!(*state, "working" | "idle" | "blocked" | "release"),
                "{event} maps to unknown state {state}"
            );
        }
    }

    #[test]
    fn the_blocked_states_are_covered() {
        // The whole point: screen detection misses `blocked` for Claude Code.
        let blocked: Vec<&str> = HOOKS
            .iter()
            .filter(|(_, s)| *s == "blocked")
            .map(|(e, _)| *e)
            .collect();
        assert!(blocked.contains(&"Notification"), "{blocked:?}");
        assert!(blocked.contains(&"PermissionRequest"), "{blocked:?}");
    }

    #[test]
    fn the_script_is_a_no_op_outside_a_herdr_pane() {
        // Installed globally, it runs in every Claude Code session — including
        // ones that have nothing to do with the board.
        assert!(SCRIPT.contains(r#"[ "${HERDR_ENV:-}" = "1" ] || exit 0"#));
        assert!(SCRIPT.contains(r#"[ -n "${HERDR_PANE_ID:-}" ] || exit 0"#));
    }

    #[test]
    fn the_script_never_fails_a_hook() {
        assert!(SCRIPT.trim_end().ends_with("exit 0"));
        // Every herdr call swallows its own failure.
        assert_eq!(SCRIPT.matches("|| true").count(), 3);
    }
}
