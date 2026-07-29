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
    /// Where the pane is. A workspace herdr has no `repo_root` for can still
    /// hold a pane sitting inside a checkout, which is the only thing that says
    /// what repo the operator is actually working in.
    pub cwd: Option<String>,
}

/// A pane's position and size within its tab.
#[derive(Debug, Clone)]
pub struct PaneRect {
    pub pane_id: String,
    pub x: u16,
    pub height: u16,
}

/// Where a new pane goes: beside something, or in a tab of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    Split {
        target: String,
        direction: &'static str,
    },
    NewTab,
}

#[derive(Debug, Clone)]
pub struct CreatedTab {
    pub tab_id: String,
    pub root_pane_id: String,
}

/// One checkout of a repository, as herdr sees it.
///
/// herdr reports the *real* path, which is what makes this the right source for
/// `gc`: the checkouts are herdr's now, it puts them under its configurable
/// `[worktrees] directory`, and the checkouts an older board left under
/// `$STATE/wt/` show up here too.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
    /// False for the repository's own working tree, which is not a checkout to
    /// collect.
    pub is_linked_worktree: bool,
    /// git still has an administrative entry, but the directory is gone.
    pub is_prunable: bool,
    /// Set while herdr holds this checkout open as a workspace. It is absent
    /// far more often than you would expect: a workspace whose last pane closes
    /// is dropped, so cancelling an attempt leaves the checkout with no
    /// workspace behind it.
    pub open_workspace_id: Option<String>,
}

/// A git worktree that herdr has opened as its own workspace, grouped under the
/// parent repo in the spaces sidebar.
#[derive(Debug, Clone)]
pub struct CreatedWorktree {
    pub workspace_id: String,
    pub root_pane_id: String,
    /// Where herdr put the checkout.
    pub path: PathBuf,
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

    /// Reload agent-detection manifests in the running server, so a local
    /// override applies without a restart.
    pub fn reload_agent_manifests(&self) -> Result<()> {
        self.run(&["server", "reload-agent-manifests"])?;
        Ok(())
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

    /// Cut a git worktree and open it as its own workspace.
    ///
    /// herdr does the checkout, the workspace, and the grouping under the
    /// parent repo — which is where a dispatched agent belongs: a space of its
    /// own in the sidebar, not a sliver of somebody else's tab.
    ///
    /// `--cwd` rather than `--workspace`: the routing config names a repo path,
    /// and it works whether or not herdr has classified the routed workspace as
    /// a repo.
    pub fn worktree_create(
        &self,
        repo: &Path,
        branch: &str,
        label: &str,
    ) -> Result<CreatedWorktree> {
        let repo = repo.to_string_lossy().into_owned();
        let r = self.run(&[
            "worktree",
            "create",
            "--cwd",
            &repo,
            "--branch",
            branch,
            "--label",
            label,
            // Dispatching must not yank the operator out of the board.
            "--no-focus",
        ])?;
        parse_worktree(&r)
    }

    /// Reopen the workspace for a branch that already has a checkout — what a
    /// retry needs, since git allows a branch in only one worktree.
    pub fn worktree_open(&self, repo: &Path, branch: &str) -> Result<CreatedWorktree> {
        let repo = repo.to_string_lossy().into_owned();
        let r = self.run(&[
            "worktree",
            "open",
            "--cwd",
            &repo,
            "--branch",
            branch,
            "--no-focus",
        ])?;
        parse_worktree(&r)
    }

    /// Every checkout herdr knows of for the repository containing `cwd`.
    ///
    /// `--cwd` is not optional in practice: without it herdr answers for
    /// whichever workspace happens to be focused, which for a background `gc`
    /// is somebody else's repository entirely.
    pub fn worktree_list(&self, cwd: &Path) -> Result<Vec<WorktreeInfo>> {
        let cwd = cwd.to_string_lossy().into_owned();
        let r = self.run_quiet(&["worktree", "list", "--cwd", &cwd])?;
        let arr = r
            .get("worktrees")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("worktree list: no `worktrees` array"))?;
        Ok(arr.iter().filter_map(parse_worktree_info).collect())
    }

    /// Remove a checkout through herdr: it runs `git worktree remove` *and*
    /// closes the workspace. Removing it with git alone would leave herdr
    /// showing a workspace whose directory is gone.
    pub fn worktree_remove(&self, workspace_id: &str) -> Result<()> {
        // Quiet: `workspace_not_found` is an expected answer — the workspace
        // may have been dropped between listing and removing — and the caller
        // falls back to git rather than treating it as a fault.
        self.run_quiet(&["worktree", "remove", "--workspace", workspace_id])?;
        Ok(())
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

    /// Current status of an agent, or `None` if herdr does not know it.
    pub fn agent_status(&self, target: &str) -> Option<AgentStatus> {
        self.run_quiet(&["agent", "get", target])
            .ok()?
            .get("agent")?
            .get("agent_status")?
            .as_str()
            .map(AgentStatus::parse)
    }

    /// The pane's visible screen, for detecting that *something* changed.
    pub fn pane_read_visible(&self, pane_id: &str) -> Option<String> {
        let out = Command::new(&self.bin)
            .args(["pane", "read", pane_id, "--source", "visible"])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Send literal keys to an agent's UI.
    pub fn agent_send_keys(&self, target: &str, keys: &[&str]) -> Result<()> {
        let mut args = vec!["agent", "send-keys", target];
        args.extend_from_slice(keys);
        self.run_quiet(&args)?;
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
        match self.board_placement(workspace_id)? {
            Placement::Split { target, .. } => Some(target),
            Placement::NewTab => None,
        }
    }

    /// A pane to split from, and which way to split it.
    ///
    /// Splitting right every time turns a busy tab into a row of narrow
    /// columns; once a tab already holds two panes, splitting down keeps
    /// everything readable.
    /// Where a dispatched **agent** pane should go.
    ///
    /// Agents stack down the left; the board keeps the right-hand column. The
    /// board is excluded from the count, because the limit is about how many
    /// agents you can read at once, not how many panes exist — counting it made
    /// agents overflow a tab sooner and, worse, made the board itself overflow.
    pub fn agent_placement(
        &self,
        workspace_id: &str,
        max_agent_panes_per_tab: usize,
    ) -> Option<Placement> {
        let panes = self.pane_list().ok()?;
        let here: Vec<&PaneInfo> = panes
            .iter()
            .filter(|p| p.workspace_id == workspace_id)
            .collect();
        let target = here.iter().find(|p| p.focused).or_else(|| here.first())?;
        let agents_in_tab = here
            .iter()
            .filter(|p| {
                p.tab_id.is_some()
                    && p.tab_id == target.tab_id
                    && p.label.as_deref() != Some("Board")
            })
            .count();
        if agents_in_tab >= max_agent_panes_per_tab {
            return Some(Placement::NewTab);
        }

        // Stack down the tallest pane that is not the board, so agents grow
        // their own column and the board keeps its one. Splitting whatever
        // happens to be focused shaves a sliver off an arbitrary pane and the
        // layout degrades differently every time.
        let board_ids: std::collections::HashSet<&str> = here
            .iter()
            .filter(|p| p.label.as_deref() == Some("Board"))
            .map(|p| p.pane_id.as_str())
            .collect();
        let split_from = self
            .pane_layout(&target.pane_id)
            .and_then(|rects| {
                let candidates: Vec<PaneRect> = rects
                    .into_iter()
                    .filter(|r| !board_ids.contains(r.pane_id.as_str()))
                    .collect();
                tallest_leftmost(&candidates)
            })
            .unwrap_or_else(|| target.pane_id.clone());

        Some(Placement::Split {
            target: split_from,
            direction: "down",
        })
    }

    /// Geometry of every pane in the tab containing `pane_id`.
    pub fn pane_layout(&self, pane_id: &str) -> Option<Vec<PaneRect>> {
        let r = self.run_quiet(&["pane", "layout", "--pane", pane_id]).ok()?;
        let panes = r.get("layout")?.get("panes")?.as_array()?;
        Some(
            panes
                .iter()
                .filter_map(|p| {
                    let rect = p.get("rect")?;
                    Some(PaneRect {
                        pane_id: p.get("pane_id")?.as_str()?.to_string(),
                        x: rect.get("x")?.as_u64()? as u16,
                        height: rect.get("height")?.as_u64()? as u16,
                    })
                })
                .collect(),
        )
    }

    /// Where the **board** should go: always beside your work, never in a tab
    /// of its own. Being able to see what is in flight while you work is the
    /// point of it; a tab you have to switch to defeats that entirely.
    ///
    /// A split inherits the height of the pane it came from, so splitting off
    /// whichever pane happens to be focused can leave the board a few rows tall
    /// in a stacked tab. Splitting off the *tallest* pane gives it the best
    /// column available, and the rightmost of equals keeps it on the right.
    pub fn board_placement(&self, workspace_id: &str) -> Option<Placement> {
        let panes = self.pane_list().ok()?;
        let here: Vec<&PaneInfo> = panes
            .iter()
            .filter(|p| p.workspace_id == workspace_id)
            .collect();
        let focused = here.iter().find(|p| p.focused).or_else(|| here.first())?;

        let target = self
            .pane_layout(&focused.pane_id)
            .and_then(|rects| tallest_rightmost(&rects))
            .unwrap_or_else(|| focused.pane_id.clone());

        Some(Placement::Split {
            target,
            direction: "right",
        })
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

    /// Raise a herdr notification. Best-effort: a missed toast must never
    /// break a sync cycle.
    pub fn notify(&self, title: &str, body: &str) {
        let _ = self.run_quiet(&["notification", "show", title, "--body", body, "--sound", "done"]);
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

/// The tallest pane, preferring the left — where agents live.
pub fn tallest_leftmost(rects: &[PaneRect]) -> Option<String> {
    rects
        .iter()
        .min_by_key(|r| (std::cmp::Reverse(r.height), r.x))
        .map(|r| r.pane_id.clone())
}

/// The pane that yields the tallest column: tallest, then rightmost.
pub fn tallest_rightmost(rects: &[PaneRect]) -> Option<String> {
    rects
        .iter()
        .max_by_key(|r| (r.height, r.x))
        .map(|r| r.pane_id.clone())
}

fn parse_worktree(r: &Value) -> Result<CreatedWorktree> {
    let root = r
        .get("root_pane")
        .ok_or_else(|| anyhow!("worktree: no .result.root_pane"))?;
    Ok(CreatedWorktree {
        workspace_id: root
            .get("workspace_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        root_pane_id: root
            .get("pane_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("worktree: no root pane id"))?
            .to_string(),
        path: root
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("worktree: no cwd"))?,
    })
}

fn parse_worktree_info(w: &Value) -> Option<WorktreeInfo> {
    Some(WorktreeInfo {
        path: PathBuf::from(w.get("path")?.as_str()?),
        branch: w.get("branch").and_then(Value::as_str).map(str::to_string),
        is_linked_worktree: w
            .get("is_linked_worktree")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_prunable: w
            .get("is_prunable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        open_workspace_id: w
            .get("open_workspace_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
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
        cwd: p
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Build a herdr agent name for an attempt.
///
/// herdr requires `[a-z][a-z0-9_-]{0,31}` and uniqueness among live agents, so
/// the attempt number is part of the name; a retry of LIN-142 is `lin-142-2`.
///
/// `base` is the task's branch slug — `dispatch::branch_slug`, which is
/// repo-qualified for GitHub. Uniqueness is why: `gh#2` names an issue in two
/// repos at once. The cap truncates the tail, and the slug puts the identifier
/// first for exactly that reason: a long repo name loses characters, the issue
/// number never does.
pub fn agent_name(base: &str, attempt: usize) -> String {
    let base = crate::config::slugify(base);
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
            cwd: None,
        }
    }

    /// The board rule, without a running herdr: always right, always here.
    fn direction_for(panes: &[PaneInfo], ws: &str) -> Option<(String, &'static str)> {
        let here: Vec<&PaneInfo> = panes.iter().filter(|p| p.workspace_id == ws).collect();
        let target = here.iter().find(|p| p.focused).or_else(|| here.first())?;
        Some((target.pane_id.clone(), "right"))
    }

    fn board(id: &str, ws: &str, tab: &str) -> PaneInfo {
        let mut p = pane(id, ws, tab, false);
        p.label = Some("Board".into());
        p
    }

    /// The agent rule, without a running herdr.
    fn agent_placement_for(panes: &[PaneInfo], ws: &str, max: usize) -> Option<Placement> {
        let here: Vec<&PaneInfo> = panes.iter().filter(|p| p.workspace_id == ws).collect();
        let target = here.iter().find(|p| p.focused).or_else(|| here.first())?;
        let agents = here
            .iter()
            .filter(|p| {
                p.tab_id.is_some()
                    && p.tab_id == target.tab_id
                    && p.label.as_deref() != Some("Board")
            })
            .count();
        if agents >= max {
            return Some(Placement::NewTab);
        }
        Some(Placement::Split {
            target: target.pane_id.clone(),
            direction: "down",
        })
    }

    #[test]
    fn agents_stack_off_the_tallest_pane_that_is_not_the_board() {
        let rects = vec![
            PaneRect { pane_id: "w1:p1".into(), x: 0, height: 53 },
            PaneRect { pane_id: "w1:board".into(), x: 94, height: 53 },
        ];
        // The board is filtered out before this is called; of what remains the
        // tallest and leftmost wins, keeping agents in their own column.
        let without_board: Vec<PaneRect> =
            rects.into_iter().filter(|r| r.pane_id != "w1:board").collect();
        assert_eq!(tallest_leftmost(&without_board).as_deref(), Some("w1:p1"));
    }

    #[test]
    fn the_board_never_counts_towards_the_agent_limit() {
        // Counting it made agents overflow a tab sooner than asked, and made
        // the board itself get shunted into a tab where it cannot be seen.
        let panes = vec![
            pane("w1:p1", "w1", "w1:t1", true),
            board("w1:p2", "w1", "w1:t1"),
        ];
        assert!(matches!(
            agent_placement_for(&panes, "w1", 2),
            Some(Placement::Split { .. })
        ));
    }

    #[test]
    fn agents_stack_down_and_leave_the_board_its_column() {
        let panes = vec![pane("w1:p1", "w1", "w1:t1", true)];
        assert!(matches!(
            agent_placement_for(&panes, "w1", 3),
            Some(Placement::Split { direction, .. }) if direction == "down"
        ));
    }

    #[test]
    fn a_tab_fills_before_anything_opens_a_new_one() {
        // The board never counts, so the limit is about the working column.
        let mut panes = vec![board("w1:b", "w1", "w1:t1")];
        assert!(matches!(
            agent_placement_for(&panes, "w1", 2),
            Some(Placement::Split { .. })
        ));
        panes.push(pane("w1:p1", "w1", "w1:t1", true));
        assert!(matches!(
            agent_placement_for(&panes, "w1", 2),
            Some(Placement::Split { .. })
        ));
        panes.push(pane("w1:p2", "w1", "w1:t1", false));
        assert_eq!(
            agent_placement_for(&panes, "w1", 2),
            Some(Placement::NewTab),
            "past the limit, a tab"
        );
        // ...and the limit is the operator's to set: a tab is easy to miss, so
        // some will prefer a tighter column to an agent out of sight.
        assert!(matches!(
            agent_placement_for(&panes, "w1", 3),
            Some(Placement::Split { .. })
        ));
    }

    #[test]
    fn the_board_splits_off_the_tallest_pane_available() {
        // A split inherits its source pane's height, so in a stacked tab the
        // focused pane is often the worst choice.
        let rects = vec![
            PaneRect { pane_id: "w1:p1".into(), x: 0, height: 16 },
            PaneRect { pane_id: "w1:p2".into(), x: 0, height: 16 },
            PaneRect { pane_id: "w1:p3".into(), x: 94, height: 53 },
        ];
        assert_eq!(tallest_rightmost(&rects).as_deref(), Some("w1:p3"));
    }

    #[test]
    fn equal_heights_break_towards_the_right() {
        let rects = vec![
            PaneRect { pane_id: "w1:p1".into(), x: 0, height: 53 },
            PaneRect { pane_id: "w1:p2".into(), x: 94, height: 53 },
        ];
        assert_eq!(tallest_rightmost(&rects).as_deref(), Some("w1:p2"));
    }

    #[test]
    fn the_board_always_splits_right_beside_your_work() {
        let panes = vec![pane("w1:p1", "w1", "w1:t1", true)];
        assert_eq!(direction_for(&panes, "w1").unwrap().1, "right");
    }

    #[test]
    fn the_board_stays_in_the_tab_however_busy_it_is() {
        // It is the pane you keep an eye on while working; a tab you have to
        // switch to defeats the point of it.
        let panes = vec![
            pane("w1:p1", "w1", "w1:t1", true),
            pane("w1:p2", "w1", "w1:t1", false),
            pane("w1:p3", "w1", "w1:t1", false),
        ];
        let (target, direction) = direction_for(&panes, "w1").unwrap();
        assert_eq!(target, "w1:p1", "splits from the focused pane");
        assert_eq!(direction, "right");
    }

    #[test]
    fn only_the_tab_being_split_into_counts() {
        let panes = vec![
            pane("w1:p1", "w1", "w1:t1", true),
            pane("w1:p2", "w1", "w1:t2", false),
            pane("w1:p3", "w1", "w1:t2", false),
        ];
        assert_eq!(
            agent_placement_for(&panes, "w1", 2),
            Some(Placement::Split {
                target: "w1:p1".into(),
                direction: "down"
            })
        );
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
