//! Typed wrappers over the herdr CLI — the single place argv lives.
//!
//! Every invocation goes through [`Herdr::run`], which logs the exact argv. If a
//! herdr verb ever differs from expectation, this is the only file to fix.
//!
//! Verified against herdr 0.7.5 (`herdr completion zsh` + `/docs/cli-reference/`):
//!   - `agent start <name> --kind KIND --pane ID [--timeout MS]`
//!   - `agent prompt <target> <text> [--wait] [--until STATUS] [--timeout MS]`
//!   - `agent focus <target>` / `tab focus <tab_id>` / `workspace focus <id>`
//!   - `tab create [--workspace ID] [--cwd PATH] [--label TEXT] [--no-focus]`
//!   - `pane list [--workspace ID]` / `pane close <pane_id>`
//!
//! Note `pane focus` is *directional* (`--direction left|right|up|down`); it
//! cannot focus an arbitrary pane id. Focusing a specific pane goes through
//! `agent focus`, which accepts a pane id as its target.

use crate::log::Logger;
use crate::model::AgentStatus;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Workspace {
    pub workspace_id: String,
    pub label: String,
    /// Git checkout this workspace belongs to, when it has one.
    pub repo_root: Option<String>,
    /// True for linked worktrees — those are attempts, not projects.
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: Option<String>,
    /// Agent kind herdr detected in the pane, recorded in the log so a
    /// misrouted dispatch is diagnosable.
    pub agent: Option<String>,
    pub agent_status: Option<AgentStatus>,
    pub focused: bool,
    /// Pane label. herdr sets this from the manifest `title` for plugin panes,
    /// which is how we recognise our own board.
    pub label: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CreatedTab {
    pub tab_id: String,
    pub root_pane_id: String,
}

pub struct Herdr {
    bin: PathBuf,
    log: Arc<Logger>,
}

impl Herdr {
    /// Always resolve through `HERDR_BIN_PATH` when herdr provided it — that is
    /// what keeps the plugin portable across Unix sockets and Windows named
    /// pipes. Bare `herdr` is only a fallback for running outside a plugin
    /// invocation (`doctor`, `demo`, tests).
    pub fn discover(log: Arc<Logger>) -> Herdr {
        let bin = match std::env::var("HERDR_BIN_PATH") {
            Ok(v) if !v.is_empty() => PathBuf::from(v),
            _ => PathBuf::from("herdr"),
        };
        Herdr { bin, log }
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// Run a herdr subcommand and return its `.result` object.
    fn run(&self, args: &[&str]) -> Result<Value> {
        self.run_inner(args, true)
    }

    /// As [`Herdr::run`], but without logging a failure as an error. For calls
    /// where a failure is an expected answer rather than a fault — asking about
    /// a pane that may well be gone.
    fn run_quiet(&self, args: &[&str]) -> Result<Value> {
        self.run_inner(args, false)
    }

    fn run_inner(&self, args: &[&str], log_errors: bool) -> Result<Value> {
        self.log.info(format!("herdr argv: {:?}", args));
        let out = Command::new(&self.bin)
            .args(args)
            .output()
            .with_context(|| format!("spawning {} {:?}", self.bin.display(), args))?;

        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

        if !out.status.success() {
            // herdr emits a JSON error on stderr with exit status 1; CLI usage
            // errors exit 2 with plain text.
            let detail = serde_json::from_str::<Value>(stderr.trim())
                .ok()
                .and_then(|v| {
                    let e = v.get("error")?;
                    Some(format!(
                        "{}: {}",
                        e.get("code").and_then(Value::as_str).unwrap_or("error"),
                        e.get("message").and_then(Value::as_str).unwrap_or("")
                    ))
                })
                .unwrap_or_else(|| {
                    let t = stderr.trim();
                    if t.is_empty() {
                        format!("exit status {}", out.status)
                    } else {
                        t.to_string()
                    }
                });
            if log_errors {
                self.log
                    .error(format!("herdr {:?} failed: {}", args, detail));
            }
            bail!("herdr {}: {}", args.join(" "), detail);
        }

        if stdout.trim().is_empty() {
            return Ok(Value::Null);
        }
        let v: Value = serde_json::from_str(stdout.trim())
            .with_context(|| format!("parsing herdr output for {:?}", args))?;
        Ok(v.get("result").cloned().unwrap_or(v))
    }

    pub fn version(&self) -> Result<String> {
        let out = Command::new(&self.bin).arg("--version").output()?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub fn workspace_list(&self) -> Result<Vec<Workspace>> {
        let r = self.run(&["workspace", "list"])?;
        let arr = r
            .get("workspaces")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("workspace list: no `workspaces` array"))?;
        Ok(arr
            .iter()
            .filter_map(|w| {
                let wt = w.get("worktree");
                Some(Workspace {
                    workspace_id: w.get("workspace_id")?.as_str()?.to_string(),
                    label: w
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    repo_root: wt
                        .and_then(|t| t.get("repo_root"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    is_linked_worktree: wt
                        .and_then(|t| t.get("is_linked_worktree"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect())
    }

    /// Resolve a workspace *label* (what `routing.toml` names) to its id.
    pub fn workspace_id_for_label(&self, label: &str) -> Result<Option<String>> {
        Ok(self
            .workspace_list()?
            .into_iter()
            .find(|w| w.label.eq_ignore_ascii_case(label))
            .map(|w| w.workspace_id))
    }

    pub fn pane_list(&self) -> Result<Vec<PaneInfo>> {
        let r = self.run(&["pane", "list"])?;
        let arr = r
            .get("panes")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("pane list: no `panes` array"))?;
        Ok(arr.iter().filter_map(parse_pane).collect())
    }

    pub fn tab_create(
        &self,
        workspace_id: &str,
        cwd: &Path,
        label: &str,
    ) -> Result<CreatedTab> {
        let cwd = cwd.to_string_lossy().into_owned();
        // `--no-focus` matters: dispatching must not yank the operator out of
        // the board pane they pressed enter in.
        let r = self.run(&[
            "tab",
            "create",
            "--workspace",
            workspace_id,
            "--cwd",
            &cwd,
            "--label",
            label,
            "--no-focus",
        ])?;
        let tab_id = r
            .get("tab")
            .and_then(|t| t.get("tab_id"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("tab create: no .result.tab.tab_id"))?
            .to_string();
        let root_pane_id = r
            .get("root_pane")
            .and_then(|p| p.get("pane_id"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("tab create: no .result.root_pane.pane_id"))?
            .to_string();
        Ok(CreatedTab {
            tab_id,
            root_pane_id,
        })
    }

    /// Split an existing pane, giving the new one its own cwd.
    ///
    /// Preferred over `tab create` for dispatch: a new tab is invisible until
    /// you switch to it, so an agent that goes `blocked` waiting for approval
    /// would sit unseen. A split lands in the workspace's active tab, where it
    /// is visible and actionable.
    pub fn pane_split(
        &self,
        target_pane: &str,
        cwd: &Path,
        direction: &str,
    ) -> Result<String> {
        let cwd = cwd.to_string_lossy().into_owned();
        let r = self.run(&[
            "pane",
            "split",
            target_pane,
            "--direction",
            direction,
            "--cwd",
            &cwd,
            // Do not steal focus: dispatching should leave the operator on the
            // board they pressed enter in.
            "--no-focus",
        ])?;
        r.get("pane")
            .and_then(|p| p.get("pane_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("pane split: no .result.pane.pane_id"))
    }

    /// Start an agent in an existing shell pane.
    ///
    /// `agent start` never creates layout — the pane must already exist and be
    /// sitting at its interactive shell prompt. `kind` must be a herdr agent
    /// kind, not the routing config's display name (see
    /// [`crate::config::herdr_kind_for_runtime`]).
    pub fn agent_start(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        timeout_ms: u64,
    ) -> Result<()> {
        let t = timeout_ms.to_string();
        // Quiet: `agent_pane_busy` is the expected answer while a freshly
        // created pane's shell reaches its prompt, and the caller retries. A
        // real failure is reported by the caller with context.
        self.run_quiet(&[
            "agent",
            "start",
            name,
            "--kind",
            kind,
            "--pane",
            pane_id,
            "--timeout",
            &t,
        ])?;
        Ok(())
    }

    /// Submit the templated prompt.
    ///
    /// Deliberately **without** `--wait`: the daemon reconciles pane state on
    /// its own tick, and a blocking dispatch would hold the picker open for the
    /// length of an agent turn.
    pub fn agent_prompt(&self, target: &str, text: &str) -> Result<()> {
        self.run(&["agent", "prompt", target, text])?;
        Ok(())
    }

    pub fn agent_focus(&self, target: &str) -> Result<()> {
        self.run(&["agent", "focus", target])?;
        Ok(())
    }

    pub fn tab_focus(&self, tab_id: &str) -> Result<()> {
        self.run(&["tab", "focus", tab_id])?;
        Ok(())
    }

    pub fn workspace_focus(&self, workspace_id: &str) -> Result<()> {
        self.run(&["workspace", "focus", workspace_id])?;
        Ok(())
    }

    pub fn pane_close(&self, pane_id: &str) -> Result<()> {
        self.run(&["pane", "close", pane_id])?;
        Ok(())
    }

    /// Ask herdr about a pane. A pane that no longer exists is a normal answer,
    /// not a failure — the board overlay may have been closed from inside, or a
    /// whole session ago.
    /// A pane to split from in a given workspace: the focused one if it is
    /// there, otherwise any of them.
    pub fn split_target_in(&self, workspace_id: &str) -> Option<String> {
        self.split_target_and_direction(workspace_id).map(|(p, _)| p)
    }

    /// A pane to split from, and which way to split it.
    ///
    /// Splitting right every time turns a busy tab into a row of narrow
    /// columns; once a tab already holds two panes, splitting down keeps
    /// everything readable.
    pub fn split_target_and_direction(
        &self,
        workspace_id: &str,
    ) -> Option<(String, &'static str)> {
        let panes = self.pane_list().ok()?;
        let here: Vec<&PaneInfo> = panes
            .iter()
            .filter(|p| p.workspace_id == workspace_id)
            .collect();
        let target = here.iter().find(|p| p.focused).or_else(|| here.first())?;
        // Count only the tab we are actually splitting into.
        let in_tab = here
            .iter()
            .filter(|p| p.tab_id.is_some() && p.tab_id == target.tab_id)
            .count();
        let direction = if in_tab >= 2 { "down" } else { "right" };
        Some((target.pane_id.clone(), direction))
    }

    pub fn pane_get(&self, pane_id: &str) -> Result<Option<PaneInfo>> {
        match self.run_quiet(&["pane", "get", pane_id]) {
            Ok(r) => Ok(r.get("pane").and_then(parse_pane)),
            // `not_found` is the expected answer for an orphaned pane, not a
            // failure of the call.
            Err(e) if e.to_string().contains("not_found") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Open one of our own manifest-declared panes.
    ///
    /// `--placement` is deliberately not passed: the manifest already declares
    /// `placement = "popup"` for the picker, and herdr 0.7.5's CLI completion
    /// does not offer `popup` as a `--placement` value even though the docs
    /// list it. Letting the manifest decide avoids that discrepancy entirely.
    pub fn plugin_pane_open(&self, entrypoint: &str) -> Result<Option<String>> {
        self.plugin_pane_open_as(entrypoint, None, None)
    }

    /// Open a manifest pane, optionally overriding its placement.
    ///
    /// `direction` only applies to a split, and has no manifest field, so it has
    /// to be passed here.
    pub fn plugin_pane_open_as(
        &self,
        entrypoint: &str,
        placement: Option<&str>,
        direction: Option<&str>,
    ) -> Result<Option<String>> {
        self.plugin_pane_open_in(entrypoint, placement, direction, None)
    }

    /// As above, but split off a specific existing pane.
    ///
    /// A split has to be split *from* something: herdr rejects
    /// `--placement split --workspace <id>` with "split and zoomed plugin panes
    /// target an existing pane". `--target-pane` carries the workspace with it,
    /// so this is how a split lands somewhere other than the focused workspace.
    pub fn plugin_pane_open_in(
        &self,
        entrypoint: &str,
        placement: Option<&str>,
        direction: Option<&str>,
        target_pane: Option<&str>,
    ) -> Result<Option<String>> {
        let plugin_id = std::env::var("HERDR_PLUGIN_ID")
            .unwrap_or_else(|_| crate::config::PLUGIN_ID.to_string());
        let mut args: Vec<&str> = vec![
            "plugin",
            "pane",
            "open",
            "--plugin",
            &plugin_id,
            "--entrypoint",
            entrypoint,
        ];
        if let Some(p) = placement {
            args.push("--placement");
            args.push(p);
        }
        if let Some(d) = direction {
            args.push("--direction");
            args.push(d);
        }
        if let Some(t) = target_pane {
            args.push("--target-pane");
            args.push(t);
        }
        let r = self.run(&args)?;
        Ok(r
            .get("plugin_pane")
            .and_then(|p| p.get("pane"))
            .and_then(|p| p.get("pane_id"))
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    /// Focus one of our own plugin panes by id.
    pub fn plugin_pane_focus(&self, pane_id: &str) -> Result<()> {
        self.run(&["plugin", "pane", "focus", pane_id])?;
        Ok(())
    }

    pub fn plugin_pane_close(&self, pane_id: &str) -> Result<()> {
        self.run(&["plugin", "pane", "close", pane_id])?;
        Ok(())
    }

    /// Focus the pane bound to an attempt (`g` on the board). Falls back to the
    /// tab when the pane no longer hosts a recognized agent.
    pub fn focus_pane(&self, pane_id: &str) -> Result<()> {
        if self.agent_focus(pane_id).is_ok() {
            return Ok(());
        }
        let pane = self
            .pane_get(pane_id)?
            .ok_or_else(|| anyhow!("herdr does not know pane {pane_id}"))?;
        if let Some(tab) = pane.tab_id.as_deref() {
            self.tab_focus(tab)?;
        } else {
            self.workspace_focus(&pane.workspace_id)?;
        }
        Ok(())
    }
}

fn parse_pane(p: &Value) -> Option<PaneInfo> {
    Some(PaneInfo {
        pane_id: p.get("pane_id")?.as_str()?.to_string(),
        workspace_id: p
            .get("workspace_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        tab_id: p.get("tab_id").and_then(Value::as_str).map(str::to_string),
        agent: p.get("agent").and_then(Value::as_str).map(str::to_string),
        agent_status: p
            .get("agent_status")
            .and_then(Value::as_str)
            .map(AgentStatus::parse),
        focused: p.get("focused").and_then(Value::as_bool).unwrap_or(false),
        label: p.get("label").and_then(Value::as_str).map(str::to_string),
    })
}

/// Build a herdr agent name for an attempt.
///
/// herdr requires `[a-z][a-z0-9_-]{0,31}` and uniqueness among live agents, so
/// the attempt number is part of the name; a retry of LIN-142 is `lin-142-2`.
pub fn agent_name(identifier: &str, attempt: usize) -> String {
    let base = crate::config::slugify(identifier);
    let base = if base.starts_with(|c: char| c.is_ascii_lowercase()) {
        base
    } else {
        // GitHub identifiers render as `#87`, which slugifies to `87` and would
        // be rejected for starting with a digit.
        format!("b{base}")
    };
    let suffix = format!("-{attempt}");
    let room = 32usize.saturating_sub(suffix.len());
    let mut name: String = base.chars().take(room).collect();
    while name.ends_with('-') {
        name.pop();
    }
    if name.is_empty() {
        name.push('b');
    }
    format!("{name}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, ws: &str, tab: &str, focused: bool) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            workspace_id: ws.into(),
            tab_id: Some(tab.into()),
            agent: None,
            agent_status: None,
            focused,
            label: None,
        }
    }

    /// The direction rule, without a running herdr.
    fn direction_for(panes: &[PaneInfo], ws: &str) -> Option<(String, &'static str)> {
        let here: Vec<&PaneInfo> = panes.iter().filter(|p| p.workspace_id == ws).collect();
        let target = here.iter().find(|p| p.focused).or_else(|| here.first())?;
        let in_tab = here
            .iter()
            .filter(|p| p.tab_id.is_some() && p.tab_id == target.tab_id)
            .count();
        Some((
            target.pane_id.clone(),
            if in_tab >= 2 { "down" } else { "right" },
        ))
    }

    #[test]
    fn a_quiet_tab_splits_right() {
        let panes = vec![pane("w1:p1", "w1", "w1:t1", true)];
        assert_eq!(direction_for(&panes, "w1").unwrap().1, "right");
    }

    #[test]
    fn a_busy_tab_splits_down_instead_of_stacking_columns() {
        let panes = vec![
            pane("w1:p1", "w1", "w1:t1", true),
            pane("w1:p2", "w1", "w1:t1", false),
        ];
        let (target, direction) = direction_for(&panes, "w1").unwrap();
        assert_eq!(target, "w1:p1", "splits from the focused pane");
        assert_eq!(direction, "down");
    }

    #[test]
    fn panes_in_other_tabs_do_not_make_a_tab_look_busy() {
        // Only the tab being split into counts.
        let panes = vec![
            pane("w1:p1", "w1", "w1:t1", true),
            pane("w1:p2", "w1", "w1:t2", false),
            pane("w1:p3", "w1", "w1:t2", false),
        ];
        assert_eq!(direction_for(&panes, "w1").unwrap().1, "right");
    }

    #[test]
    fn other_workspaces_are_ignored_entirely() {
        let panes = vec![
            pane("w2:p1", "w2", "w2:t1", false),
            pane("w2:p2", "w2", "w2:t1", false),
            pane("w1:p1", "w1", "w1:t1", true),
        ];
        assert_eq!(direction_for(&panes, "w1").unwrap().1, "right");
    }

    #[test]
    fn agent_names_satisfy_herdrs_pattern() {
        let re_ok = |s: &str| {
            let mut cs = s.chars();
            cs.next().is_some_and(|c| c.is_ascii_lowercase())
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
                && s.len() <= 32
        };
        for (id, n) in [("LIN-142", 1), ("#87", 3), ("LIN-1", 12)] {
            let name = agent_name(id, n);
            assert!(re_ok(&name), "{name} does not match herdr's agent name rule");
        }
        assert_eq!(agent_name("LIN-142", 1), "lin-142-1");
        // A GitHub identifier must not start with a digit.
        assert_eq!(agent_name("#87", 2), "b87-2");
    }

    #[test]
    fn long_identifiers_are_truncated_to_the_limit() {
        let name = agent_name(&"x".repeat(60), 10);
        assert!(name.len() <= 32, "{name} is {} chars", name.len());
        assert!(name.ends_with("-10"));
    }
}
