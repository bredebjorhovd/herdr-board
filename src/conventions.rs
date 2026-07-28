//! Teaching every runtime that the board exists.
//!
//! Dispatch has always supported `codex` and `opencode` — `herdr_kind_for_runtime`
//! maps both — but for the first two dozen attempts only Claude Code had ever
//! been told what `herdr-board` is *for*. The conventions lived in
//! `~/.claude/skills/board/SKILL.md` and nowhere else, so a dispatched codex
//! agent had the CLI on PATH, `gh` already authenticated, and no idea either
//! mattered.
//!
//! The prose was never Claude-specific. So it lives in `agent-conventions.md` at
//! the repo root and gets written into each runtime's *global instruction file*
//! between markers, which makes a correction in one place reach all of them.
//!
//! **Instruction file, not skill, deliberately.** All three runtimes have a
//! skill mechanism, and opencode already discovers `~/.claude/skills` — so the
//! board skill is in fact visible to it today. That is not enough: a skill is
//! loaded when the agent decides it is relevant, and an agent dispatched to fix
//! a flaky test has no reason to go looking for a board skill. It would finish,
//! leave the work uncommitted, and the row would sit in `working`. The
//! instruction file is always in context, which is the property this needs.

use crate::log::Logger;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

/// The canonical text, checked in beside the code so a review sees it change.
const SOURCE: &str = include_str!("../agent-conventions.md");

const BEGIN: &str = "<!-- BEGIN herdr-board conventions -->";
const END: &str = "<!-- END herdr-board conventions -->";

/// Runtimes with a global instruction file we write to, and where it is.
///
/// Claude Code is deliberately absent: its copy is the skill at
/// `~/.claude/skills/board/SKILL.md`, which is where this text came from, and
/// writing it into `CLAUDE.md` as well would only give Claude two copies to
/// disagree with each other.
pub const AGENTS: &[&str] = &["codex", "opencode"];

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var("HOME").context("HOME is not set")?,
    ))
}

/// The global instruction file for `agent`.
///
/// Both are `AGENTS.md`, and both are the *global* one rather than a repo's:
/// the board dispatches into whatever repo a route names, so a per-repo file
/// would have to be added to every one of them.
pub fn instruction_path(agent: &str) -> Result<PathBuf> {
    let dir = match agent {
        // Codex reads `$CODEX_HOME/AGENTS.md` before any repo-level one.
        "codex" => match std::env::var("CODEX_HOME") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => home()?.join(".codex"),
        },
        // opencode's is `<config>/AGENTS.md`, the dir `opencode debug paths`
        // reports as `config`.
        "opencode" => match std::env::var("XDG_CONFIG_HOME") {
            Ok(p) if !p.is_empty() => PathBuf::from(p).join("opencode"),
            _ => home()?.join(".config/opencode"),
        },
        "claude" | "claude-code" => bail!(
            "claude's copy is the skill at ~/.claude/skills/board/SKILL.md, \
             not an instruction file"
        ),
        other => bail!("no known instruction file for `{other}`"),
    };
    Ok(dir.join("AGENTS.md"))
}

/// The block to write, markers included.
fn block() -> Result<&'static str> {
    let start = SOURCE
        .find(BEGIN)
        .context("agent-conventions.md has no BEGIN marker")?;
    let end = SOURCE
        .find(END)
        .context("agent-conventions.md has no END marker")?;
    if end < start {
        bail!("agent-conventions.md has its markers the wrong way round");
    }
    Ok(SOURCE[start..end + END.len()].trim_end())
}

/// Splice the block into `text`, replacing an existing one.
///
/// Everything outside the markers is the operator's and is preserved verbatim —
/// these are files people keep their own instructions in.
fn splice(text: &str, block: &str) -> String {
    let (before, after) = match (text.find(BEGIN), text.find(END)) {
        (Some(s), Some(e)) if e > s => (&text[..s], &text[e + END.len()..]),
        // No block, or a half-written one we cannot safely bound: append.
        _ => (text, ""),
    };
    let mut out = String::new();
    let before = before.trim_end();
    if !before.is_empty() {
        out.push_str(before);
        out.push_str("\n\n");
    }
    out.push_str(block);
    out.push('\n');
    let after = after.trim_start();
    if !after.is_empty() {
        out.push('\n');
        out.push_str(after);
        if !after.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Is the current text installed in `agent`'s instruction file?
///
/// Compares the block, not just the marker, so a stale copy reads as not
/// installed — the point of a single source is noticing when one has drifted.
pub fn installed(agent: &str) -> bool {
    let Ok(block) = block() else { return false };
    instruction_path(agent)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|s| s.contains(block))
}

pub fn install(log: &Logger, agent: &str) -> Result<PathBuf> {
    let path = instruction_path(agent)?;
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, splice(&existing, block()?))
        .with_context(|| format!("writing {}", path.display()))?;
    log.info(format!("wrote board conventions to {}", path.display()));
    Ok(path)
}

pub fn uninstall(log: &Logger, agent: &str) -> Result<Option<PathBuf>> {
    let path = instruction_path(agent)?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let (Some(s), Some(e)) = (text.find(BEGIN), text.find(END)) else {
        return Ok(None);
    };
    if e < s {
        bail!("{} has its markers the wrong way round", path.display());
    }
    let rest = format!("{}{}", &text[..s], &text[e + END.len()..]);
    let rest = rest.trim();
    // A file that was ours alone goes away; one the operator also writes in
    // keeps everything outside the markers.
    if rest.is_empty() {
        std::fs::write(&path, "")?;
    } else {
        std::fs::write(&path, format!("{rest}\n"))?;
    }
    log.info(format!("removed board conventions from {}", path.display()));
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_block_is_bounded_by_its_markers() {
        let b = block().expect("the checked-in conventions parse");
        assert!(b.starts_with(BEGIN));
        assert!(b.ends_with(END));
        // The prose above the BEGIN marker explains the file to a human reader
        // and has no business in an agent's context window.
        assert!(!b.contains("integration install-conventions"));
    }

    #[test]
    fn the_block_carries_the_rules_that_matter() {
        let b = block().unwrap();
        // The three that are load-bearing and least guessable.
        assert!(b.contains("dispatchable"), "check dispatchable first");
        assert!(b.contains("ends the *attempt*"), "cancel ends the attempt");
        assert!(b.contains("Never dispatch speculatively"));
        // And the one this port added: the board settles on commits, so an
        // agent that leaves work uncommitted never settles at all.
        assert!(b.contains("commit your work"));
    }

    #[test]
    fn installing_preserves_what_the_operator_wrote() {
        let mine = "# My own notes\n\nAlways run `cargo fmt`.\n";
        let out = splice(mine, "BLOCK");
        assert!(out.starts_with("# My own notes"));
        assert!(out.contains("Always run `cargo fmt`."));
        assert!(out.contains("BLOCK"));
    }

    #[test]
    fn installing_twice_replaces_rather_than_appends() {
        let first = splice("", &format!("{BEGIN}\nold\n{END}"));
        let second = splice(&first, &format!("{BEGIN}\nnew\n{END}"));
        assert_eq!(second.matches(BEGIN).count(), 1);
        assert!(second.contains("new"));
        assert!(!second.contains("old"));
    }

    #[test]
    fn an_update_lands_between_text_on_both_sides() {
        let text = format!("above\n\n{BEGIN}\nold\n{END}\n\nbelow\n");
        let out = splice(&text, &format!("{BEGIN}\nnew\n{END}"));
        assert!(out.starts_with("above"));
        assert!(out.trim_end().ends_with("below"));
        assert!(out.contains("new") && !out.contains("old"));
    }

    #[test]
    fn every_agent_resolves_to_an_instruction_file() {
        for a in AGENTS {
            let p = instruction_path(a).unwrap_or_else(|e| panic!("{a}: {e}"));
            assert_eq!(p.file_name().unwrap(), "AGENTS.md");
        }
        // Claude is refused with a pointer rather than a shrug: its copy is the
        // skill, and being told that is more useful than "unknown agent".
        let e = instruction_path("claude").unwrap_err().to_string();
        assert!(e.contains("skills/board/SKILL.md"), "{e}");
    }
}
