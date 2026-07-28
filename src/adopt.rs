//! Repos that have a herdr workspace but nothing on the board watching them.
//!
//! Opening a pane in a new repo used to leave the board silent: its issues were
//! not polled and nothing said so. The two pieces of config that fix that are
//! independent — `[github] repos` controls **visibility**, a `[[route]]`
//! controls **dispatchability** — which is exactly what makes it easy to
//! half-fix. Miss the route and the row appears but `enter` refuses it; miss the
//! repos entry and the issue never arrives at all.
//!
//! So: **detect automatically, adopt explicitly.** Detection is cheap and runs
//! on the sync cycle. Writing is not silent, because `routing.toml` is
//! hand-edited and documented as "not managed config", and a pane gets opened in
//! plenty of repos that do not belong on the board.
//!
//! Everything written here goes through [`apply`], which re-parses and
//! *validates* the result before it replaces the file. A writer that could emit
//! a config `doctor` refuses would be worse than no writer at all.

use crate::config::{RoutingConfig, shorten_home};
use crate::db::Db;
use crate::herdr::{Herdr, Workspace};
use crate::log::Logger;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where the last detection sweep is stored, so the TUI renders the section
/// without shelling out to herdr on its own tick.
pub const META_UNADOPTED: &str = "unadopted";

/// Which half of the config is missing.
///
/// Named rather than implied: "the row appears but cannot be dispatched" and
/// "the issue never arrives" are different bugs with the same cause, and an
/// operator looking at the section deserves to know which one they have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Missing {
    /// Not under `[github] repos`: nothing polls it.
    Polling,
    /// No route *names* it, so its rows either cannot be dispatched at all or
    /// would be dispatched into a catch-all's workspace instead of its own.
    Route,
    Both,
}

impl Missing {
    /// Suffix for the board row. Empty for `Both`, which is the ordinary case
    /// and needs no explaining.
    pub fn note(self) -> &'static str {
        match self {
            Missing::Polling => " · not polled",
            Missing::Route => " · no route",
            Missing::Both => "",
        }
    }
}

/// A git repo with a herdr workspace that the board is not watching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unadopted {
    /// herdr workspace label — the name the operator recognises.
    pub label: String,
    /// `owner/repo`, from the git remote.
    pub slug: String,
    /// Repo root on disk, for the route's `repo =`.
    pub repo_root: String,
    pub missing: Missing,
}

impl Unadopted {
    /// Selection id for the board. The NUL prefix keeps it from ever colliding
    /// with a task id, exactly as section headers do.
    pub fn row_id(&self) -> String {
        row_id(&self.slug)
    }

    /// The bare repo name — `Florin-AS/tripletex-mcp` → `tripletex-mcp`.
    pub fn name(&self) -> &str {
        self.slug.rsplit('/').next().unwrap_or(&self.slug)
    }
}

pub fn row_id(slug: &str) -> String {
    format!("\u{0}unadopted:{slug}")
}

// ---- detection ---------------------------------------------------------

/// Which workspaces the board is not watching.
///
/// `remote_of` resolves a repo root to its git remote URL; injected so the
/// whole decision is testable without a git checkout on disk.
pub fn detect<F>(workspaces: &[Workspace], cfg: &RoutingConfig, remote_of: F) -> Vec<Unadopted>
where
    F: Fn(&str) -> Option<String>,
{
    let mut out: Vec<Unadopted> = Vec::new();
    for w in workspaces {
        // Linked worktrees are attempts the board itself cut, not projects.
        if w.is_linked_worktree {
            continue;
        }
        let Some(root) = w.repo_root.as_deref() else {
            continue;
        };
        // Without a GitHub remote there is nothing to write: `[github] repos`
        // and `match.gh_repo` both name an `owner/repo`, and a git remote that
        // is not GitHub cannot supply one.
        let Some(slug) = remote_of(root).as_deref().and_then(crate::cli::github_slug) else {
            continue;
        };
        if cfg.adopt.ignores(&slug) {
            continue;
        }

        let polled = cfg
            .github
            .repos
            .iter()
            .any(|r| r.eq_ignore_ascii_case(&slug));
        // A route that *names* this repo, not merely one that would match it.
        //
        // A catch-all matches everything, so asking `resolve` would call this
        // repo routed — and it would dispatch, into whatever workspace the
        // catch-all names. Starting an agent for a tripletex-mcp issue in
        // somebody else's checkout is a worse silent failure than the one this
        // whole feature exists to remove, so a catch-all does not count.
        let routed = cfg.routes.iter().any(|r| {
            r.match_
                .gh_repo
                .as_deref()
                .is_some_and(|g| g.eq_ignore_ascii_case(&slug))
        });

        let missing = match (polled, routed) {
            (true, true) => continue,
            (false, true) => Missing::Polling,
            (true, false) => Missing::Route,
            (false, false) => Missing::Both,
        };
        // Two workspaces can sit on one repo — the checkout and a clone of it.
        // The repo is what gets adopted, so it is offered once.
        if out.iter().any(|u| u.slug.eq_ignore_ascii_case(&slug)) {
            continue;
        }
        out.push(Unadopted {
            label: if w.label.is_empty() {
                slug.rsplit('/').next().unwrap_or(&slug).to_string()
            } else {
                w.label.clone()
            },
            slug,
            repo_root: root.to_string(),
            missing,
        });
    }
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    out
}

/// The git remote a checkout pushes to, or `None`.
pub fn git_remote(repo_root: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", repo_root, "remote", "get-url", "origin"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Run a detection sweep and store it for the board to read.
///
/// Best-effort by design: herdr being unreachable means the board shows the
/// last sweep rather than an empty section, which is the same bargain every
/// other source makes here.
pub fn refresh(db: &Db, cfg: &RoutingConfig, herdr: &Herdr, log: &Logger) -> Vec<Unadopted> {
    let workspaces = match herdr.workspace_list() {
        Ok(w) => w,
        Err(e) => {
            log.warn(format!("could not list workspaces for adoption: {e}"));
            return stored(db);
        }
    };
    let found = detect(&workspaces, cfg, git_remote);
    match serde_json::to_string(&found) {
        Ok(json) => {
            let _ = db.meta_set(META_UNADOPTED, &json);
        }
        Err(e) => log.warn(format!("could not store the adoption sweep: {e}")),
    }
    found
}

/// The last stored sweep.
pub fn stored(db: &Db) -> Vec<Unadopted> {
    db.meta_get(META_UNADOPTED)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

// ---- writing routing.toml ----------------------------------------------

/// What one adoption wrote, for the footer to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adopted {
    pub wrote_route: bool,
    pub wrote_repo: bool,
    /// The label suggested for Linear issues, left commented out in the file.
    pub suggested_label: String,
}

/// Adopt a repo: write the route it needs and the `[github] repos` entry, and
/// leave a suggestion for the one thing that cannot be derived.
pub fn adopt(path: &Path, u: &Unadopted) -> Result<Adopted> {
    let before = read(path)?;
    let mut text = before.clone();
    let mut wrote_route = false;
    let mut wrote_repo = false;

    if u.missing != Missing::Polling {
        // Parsed from the file rather than taken from the caller's config: a
        // config the caller failed to load would leave them holding an empty
        // route list, and the insertion point is computed from that list.
        let cfg: RoutingConfig = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        text = insert_route(&text, &cfg, u)?;
        wrote_route = true;
    }
    if u.missing != Missing::Route {
        text = add_to_array(&text, "github", "repos", &u.slug, NEW_GITHUB_TABLE);
        wrote_repo = true;
    }

    apply(path, &before, &text)?;
    Ok(Adopted {
        wrote_route,
        wrote_repo,
        suggested_label: u.name().to_string(),
    })
}

/// Stop offering a repo. Plenty of repos you open a pane in are ones you are
/// only reading — another org's, a dependency, something you cloned to look at.
pub fn ignore(path: &Path, slug: &str) -> Result<()> {
    let before = read(path)?;
    let text = add_to_array(&before, "adopt", "ignore", slug, NEW_ADOPT_TABLE);
    apply(path, &before, &text)
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| {
        format!(
            "reading {} — run `herdr-board init` if it does not exist yet",
            path.display()
        )
    })
}

/// Commit an edit to a file the operator hand-writes.
///
/// Three things happen before the new text lands, in this order: it has to
/// parse, it has to *validate* — so a catch-all can never end up shadowing what
/// we just wrote — and the previous contents are kept beside it. A one-keypress
/// writer that could corrupt somebody's routing config would not be worth
/// having.
fn apply(path: &Path, before: &str, after: &str) -> Result<()> {
    if before == after {
        return Ok(());
    }
    let cfg: RoutingConfig = toml::from_str(after)
        .context("the edit would not have parsed; routing.toml is untouched")?;
    cfg.check()
        .context("the edit would not have validated; routing.toml is untouched")?;

    let backup = backup_path(path);
    std::fs::write(&backup, before)
        .with_context(|| format!("writing the backup {}", backup.display()))?;
    std::fs::write(path, after).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn backup_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push(".bak");
    PathBuf::from(p)
}

const NEW_GITHUB_TABLE: &str = "\n# Repos polled for issues and pull requests. Needs GITHUB_TOKEN in .env for\n# private ones.\n[github]\n";

const NEW_ADOPT_TABLE: &str = "\n# Repos with a herdr workspace that the board should stop offering to adopt —\n# ones you are only reading rather than working in. Written by `X` on the\n# board's UNADOPTED section; delete a line to be offered it again.\n[adopt]\n";

/// The route block, plus the suggestion for what a git remote cannot tell us.
fn route_block(u: &Unadopted, runtime: &str) -> String {
    let repo = shorten_home(Path::new(&u.repo_root));
    let label = u.name();
    format!(
        "# Adopted from the board's UNADOPTED section.\n\
         #\n\
         # A Linear issue carries no repo, so nothing below routes one here.\n\
         # `{label}` is a *guess* at the label you would use — a git remote cannot\n\
         # name one. Uncomment and edit to use it. Issues assigned to you arrive\n\
         # whatever their labels, so the gap is only unassigned ones.\n\
         #\n\
         # [[route]]\n\
         # match = {{ label = \"{label}\" }}\n\
         # workspace = \"{ws}\"\n\
         # repo = \"{repo}\"\n\
         # runtime = \"{runtime}\"\n\
         [[route]]\n\
         match = {{ gh_repo = \"{slug}\" }}\n\
         workspace = \"{ws}\"\n\
         repo = \"{repo}\"\n\
         runtime = \"{runtime}\"\n\
         \n",
        ws = u.label,
        slug = u.slug,
    )
}

/// The runtime to write.
///
/// Whatever the existing routes mostly use, because that is the operator's
/// established habit and a route that disagrees with every other one is a
/// surprise. `claude-code` only when there is nothing to copy.
fn habitual_runtime(cfg: &RoutingConfig) -> String {
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for r in &cfg.routes {
        *counts.entry(r.runtime.as_str()).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(r, _)| r.to_string())
        .unwrap_or_else(|| "claude-code".into())
}

/// Insert a route **ahead of whatever is currently last**.
///
/// This is the load-bearing part. First matching route wins, so a naive append
/// lands after the catch-all, is shadowed by it, and never fires — silently,
/// which is the failure mode this whole feature exists to remove.
///
/// "Ahead of the last route" rather than "ahead of the empty `match`" on
/// purpose. Validation already guarantees an empty-`match` catch-all *is* last,
/// so for that case the two rules are the same — but the catch-alls people
/// actually write are usually a team-wide `match = { linear_team = "AGE" }`,
/// which validation cannot recognise as one and which the README tells you to
/// keep last. Going in ahead of the last route respects both, and costs nothing
/// when the routes are disjoint: order only matters where matches overlap, and
/// a route naming one repo is the more specific of any pair it overlaps with.
fn insert_route(text: &str, cfg: &RoutingConfig, u: &Unadopted) -> Result<String> {
    let block = route_block(u, &habitual_runtime(cfg));
    let lines: Vec<&str> = text.lines().collect();
    let headers = header_lines(text);

    // Correlate parsed routes with their headers by document order — the same
    // order `toml` reports them in.
    let route_headers: Vec<usize> = headers
        .iter()
        .filter(|(_, name)| name == "[[route]]")
        .map(|(i, _)| *i)
        .collect();
    if route_headers.len() != cfg.routes.len() {
        // Nothing in the file matches what was parsed from it; refuse rather
        // than guess at an insertion point.
        bail!(
            "routing.toml has {} `[[route]]` header(s) but parsed {} route(s) — \
             not editing a file this writer does not understand",
            route_headers.len(),
            cfg.routes.len()
        );
    }

    // With no routes at all there is nothing to stay behind: append. Nothing
    // outside the `[[route]]` sequence cares where one sits.
    let at = route_headers
        .last()
        .map(|i| start_of_block(&lines, *i))
        .unwrap_or(lines.len());

    let mut out: Vec<String> = lines[..at].iter().map(|s| s.to_string()).collect();
    // Never run the block straight onto the line above it; a route that reads
    // as a continuation of the one before is exactly the kind of thing nobody
    // notices until it matters.
    if out.last().is_some_and(|l| !l.trim().is_empty()) {
        out.push(String::new());
    }
    out.extend(block.lines().map(str::to_string));
    out.extend(lines[at..].iter().map(|s| s.to_string()));
    Ok(join(&out, text))
}

/// Walk back from a table header over the comment block attached to it, so an
/// insertion does not land between a comment and the route it describes.
fn start_of_block(lines: &[&str], header: usize) -> usize {
    let mut i = header;
    while i > 0 {
        let prev = lines[i - 1].trim_start();
        if prev.starts_with('#') {
            i -= 1;
        } else {
            break;
        }
    }
    i
}

fn header_lines_after(lines: &[&str], after: usize) -> Option<usize> {
    let text = lines.join("\n");
    header_lines(&text)
        .into_iter()
        .map(|(i, _)| i)
        .find(|i| *i > after)
}

/// Add one string to a `key = [...]` array in a top-level table, creating
/// either as needed. A value already present is left alone.
///
/// Deliberately a text edit rather than a re-serialization: `routing.toml` is
/// hand-written and full of comments explaining choices, and rewriting it from
/// the parsed structure would silently throw all of that away.
fn add_to_array(text: &str, table: &str, key: &str, value: &str, preamble: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let headers = header_lines(text);
    let header = format!("[{table}]");

    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    let entry = format!("\"{value}\"");

    let Some(&(table_at, _)) = headers.iter().find(|(_, name)| *name == header) else {
        // No such table. Appending is safe: table order carries no meaning in
        // TOML — only the order of `[[route]]` entries among themselves does.
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.extend(preamble.lines().map(str::to_string));
        out.push(format!("{key} = [{entry}]"));
        return join(&out, text);
    };

    let end = header_lines_after(&lines, table_at).unwrap_or(lines.len());
    let key_at = (table_at + 1..end).find(|&i| {
        let t = lines[i].trim_start();
        t.strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    });

    let Some(key_at) = key_at else {
        out.insert(table_at + 1, format!("{key} = [{entry}]"));
        return join(&out, text);
    };

    // The array may span lines; find the `]` that closes it.
    let Some(close) = (key_at..end).find(|&i| lines[i].contains(']')) else {
        return text.to_string();
    };

    // Everything between the brackets, so the separator can be decided from
    // what is actually there rather than from the shape of one line.
    let joined = lines[key_at..=close].join("\n");
    let inner = joined
        .split_once('[')
        .map(|(_, r)| r)
        .and_then(|r| r.rsplit_once(']'))
        .map(|(l, _)| l.trim())
        .unwrap_or("");
    if inner.split(',').any(|v| v.trim() == entry) {
        // Already listed. Pressing `X` twice must not corrupt the array.
        return text.to_string();
    }
    // A trailing comma is a separator that is already there; a second one is a
    // parse error.
    let sep = if inner.is_empty() || inner.ends_with(',') {
        ""
    } else {
        ", "
    };

    let line = out[close].clone();
    let cut = line.rfind(']').expect("find() matched a `]`");
    let (head, tail) = line.split_at(cut);
    if head.trim().is_empty() && close > key_at {
        // A `]` on a line of its own: keep the array multi-line rather than
        // collapsing the new value onto the closing bracket.
        if !sep.is_empty()
            && let Some(prev) = (key_at..close).rev().find(|&i| !out[i].trim().is_empty())
        {
            // The last entry has no trailing comma; it needs one now.
            out[prev] = format!("{},", out[prev].trim_end());
        }
        let indent = " ".repeat(head.chars().count() + 2);
        out.insert(close, format!("{indent}{entry},"));
    } else {
        out[close] = format!("{}{sep}{entry}{tail}", head.trim_end());
    }
    join(&out, text)
}

/// Reassemble lines, preserving whether the original ended in a newline.
fn join(lines: &[String], original: &str) -> String {
    let mut s = lines.join("\n");
    if original.ends_with('\n') || original.is_empty() {
        s.push('\n');
    }
    s
}

/// Line indices of top-level table headers, with the header text.
///
/// The multi-line-string tracking is not decoration: `prompt = """..."""` is in
/// the shipped example config, and a prompt that happens to contain a line
/// starting with `[` would otherwise be read as a table header and every
/// insertion point computed from it would be wrong.
fn header_lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut open: Option<&'static str> = None;
    for (i, line) in text.lines().enumerate() {
        if open.is_none() {
            let t = line.trim();
            if t.starts_with('[') {
                let end = t.find("] ").map(|p| p + 1).unwrap_or(t.len());
                let name = t[..end].trim_end().to_string();
                if name.ends_with(']') {
                    out.push((i, name));
                }
            }
        }
        scan_multiline(line, &mut open);
    }
    out
}

/// Track `"""` / `'''` across a line.
fn scan_multiline(line: &str, open: &mut Option<&'static str>) {
    let mut rest = line;
    loop {
        match *open {
            Some(delim) => match rest.find(delim) {
                Some(i) => {
                    rest = &rest[i + delim.len()..];
                    *open = None;
                }
                None => return,
            },
            None => {
                let candidates = [("\"\"\"", rest.find("\"\"\"")), ("'''", rest.find("'''"))];
                let Some((delim, at)) = candidates
                    .into_iter()
                    .filter_map(|(d, p)| p.map(|p| (d, p)))
                    .min_by_key(|(_, p)| *p)
                else {
                    return;
                };
                // An opener inside a comment opens nothing.
                if rest.find('#').is_some_and(|h| h < at) {
                    return;
                }
                rest = &rest[at + delim.len()..];
                *open = Some(delim);
            }
        }
    }
}

/// One line for `doctor`.
///
/// Never a failure: a repo you are only reading is not a broken environment,
/// and `doctor` exiting non-zero over one would make the whole report ignorable.
pub fn doctor_detail(cfg: &RoutingConfig, herdr: &Herdr) -> String {
    let Ok(workspaces) = herdr.workspace_list() else {
        return "could not ask herdr for its workspaces".into();
    };
    let found = detect(&workspaces, cfg, git_remote);
    if found.is_empty() {
        if cfg.adopt.ignore.is_empty() {
            "every workspace with a GitHub remote is on the board".into()
        } else {
            format!(
                "every workspace with a GitHub remote is on the board ({} ignored)",
                cfg.adopt.ignore.len()
            )
        }
    } else {
        format!(
            "{} not on the board: {} — press `a` on the board's UNADOPTED section",
            found.len(),
            found
                .iter()
                .map(|u| u.slug.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouteContext;

    fn ws(label: &str, root: Option<&str>, linked: bool) -> Workspace {
        Workspace {
            workspace_id: format!("w-{label}"),
            label: label.into(),
            repo_root: root.map(str::to_string),
            is_linked_worktree: linked,
        }
    }

    fn cfg(text: &str) -> RoutingConfig {
        toml::from_str(text).unwrap()
    }

    fn remotes(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |root: &str| map.get(root).cloned()
    }

    const CATCH_ALL: &str = r#"
[sync]
labels = ["herd"]

[[route]]
match = { gh_repo = "Florin-AS/Tally" }
workspace = "tally"
repo = "~/code/tally"
runtime = "claude-code"

# Anything else lands in the scratch workspace.
[[route]]
workspace = "scratch"
repo = "~/code/scratch"
runtime = "claude-code"

[github]
repos = ["Florin-AS/Tally"]
"#;

    // ---- detection -----------------------------------------------------

    #[test]
    fn a_workspace_with_no_config_at_all_is_offered() {
        let cfg = cfg("[github]\nrepos = []\n");
        let found = detect(
            &[ws("tripletex-mcp", Some("/code/tm"), false)],
            &cfg,
            remotes(&[("/code/tm", "git@github.com:Florin-AS/tripletex-mcp.git")]),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].slug, "Florin-AS/tripletex-mcp");
        assert_eq!(found[0].missing, Missing::Both);
        assert_eq!(found[0].name(), "tripletex-mcp");
    }

    #[test]
    fn the_half_fixes_are_told_apart() {
        // The two keys are independent, which is what makes it easy to get one
        // of them and think you are done.
        let polled_only = cfg("[github]\nrepos = [\"o/r\"]\n");
        let found = detect(
            &[ws("r", Some("/code/r"), false)],
            &polled_only,
            remotes(&[("/code/r", "https://github.com/o/r")]),
        );
        assert_eq!(found[0].missing, Missing::Route, "rows appear, enter refuses");

        let routed_only = cfg(
            "[[route]]\nmatch = { gh_repo = \"o/r\" }\nworkspace = \"r\"\nrepo = \"/code/r\"\nruntime = \"claude\"\n",
        );
        let found = detect(
            &[ws("r", Some("/code/r"), false)],
            &routed_only,
            remotes(&[("/code/r", "https://github.com/o/r")]),
        );
        assert_eq!(found[0].missing, Missing::Polling, "nothing ever polls it");
    }

    #[test]
    fn a_catch_all_does_not_count_as_having_adopted_a_repo() {
        // `enter` would work — and would start an agent in the catch-all's
        // checkout rather than this repo's. Dispatching to the wrong workspace
        // is worse than the row not being there at all.
        let found = detect(
            &[ws("thing", Some("/code/thing"), false)],
            &cfg(CATCH_ALL),
            remotes(&[("/code/thing", "git@github.com:o/thing.git")]),
        );
        assert_eq!(found[0].missing, Missing::Both);

        // ...and adopting it gives the repo a route of its own, ahead of the
        // catch-all, so `enter` lands in the right checkout.
        let out = adopt_text(CATCH_ALL, &found[0]);
        let parsed = cfg(&out);
        let route = parsed
            .resolve(&RouteContext {
                gh_repo: Some("o/thing".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(route.workspace, "thing");
    }

    #[test]
    fn an_adopted_repo_is_not_offered_again() {
        let found = detect(
            &[ws("tally", Some("/code/tally"), false)],
            &cfg(CATCH_ALL),
            remotes(&[("/code/tally", "git@github.com:Florin-AS/Tally.git")]),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn ignoring_is_what_stops_it_being_offered() {
        // Plenty of repos you open a pane in are another org's.
        let cfg = cfg("[adopt]\nignore = [\"someorg/vendor\"]\n");
        let found = detect(
            &[ws("vendor", Some("/code/v"), false)],
            &cfg,
            remotes(&[("/code/v", "git@github.com:SomeOrg/vendor.git")]),
        );
        assert!(found.is_empty(), "the ignore list is case-insensitive");
    }

    #[test]
    fn attempts_and_non_repos_are_never_offered() {
        let cfg = cfg("");
        let found = detect(
            &[
                // A worktree the board itself cut for an attempt.
                ws("lin-142", Some("/code/tm"), true),
                // A workspace that is not a checkout at all.
                ws("notes", None, false),
                // A checkout whose remote is not GitHub: nothing to write.
                ws("internal", Some("/code/int"), false),
            ],
            &cfg,
            remotes(&[
                ("/code/tm", "git@github.com:o/tm.git"),
                ("/code/int", "git@git.example.com:o/int.git"),
            ]),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn one_repo_is_offered_once_however_many_workspaces_hold_it() {
        let found = detect(
            &[
                ws("thing", Some("/code/thing"), false),
                ws("thing-clone", Some("/code/thing2"), false),
            ],
            &cfg(""),
            remotes(&[
                ("/code/thing", "git@github.com:o/thing.git"),
                ("/code/thing2", "https://github.com/o/thing.git"),
            ]),
        );
        assert_eq!(found.len(), 1);
    }

    // ---- the writer ----------------------------------------------------

    fn unadopted() -> Unadopted {
        Unadopted {
            label: "tripletex-mcp".into(),
            slug: "Florin-AS/tripletex-mcp".into(),
            repo_root: "/code/tripletex-mcp".into(),
            missing: Missing::Both,
        }
    }

    fn adopt_text(text: &str, u: &Unadopted) -> String {
        let parsed = cfg(text);
        let mut out = text.to_string();
        if u.missing != Missing::Polling {
            out = insert_route(&out, &parsed, u).unwrap();
        }
        if u.missing != Missing::Route {
            out = add_to_array(&out, "github", "repos", &u.slug, NEW_GITHUB_TABLE);
        }
        // Everything the writer emits has to survive the gate `apply` puts it
        // through, or the keypress does nothing at all.
        let reparsed: RoutingConfig = toml::from_str(&out).expect("must parse");
        reparsed.check().expect("must validate");
        out
    }

    #[test]
    fn a_new_route_lands_ahead_of_the_catch_all() {
        // A naive append is shadowed by the catch-all and never matches —
        // silently. This is the whole reason the writer is not one `push`.
        let out = adopt_text(CATCH_ALL, &unadopted());
        let parsed = cfg(&out);
        let ours = parsed
            .routes
            .iter()
            .position(|r| r.match_.gh_repo.as_deref() == Some("Florin-AS/tripletex-mcp"))
            .expect("the route was written");
        let catch_all = parsed
            .routes
            .iter()
            .position(|r| r.match_.is_empty())
            .expect("the catch-all is still there");
        assert!(ours < catch_all, "shadowed by the catch-all:\n{out}");
        assert_eq!(catch_all, parsed.routes.len() - 1, "and it is still last");

        // ...and it actually resolves now.
        let route = parsed
            .resolve(&RouteContext {
                gh_repo: Some("Florin-AS/tripletex-mcp".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(route.workspace, "tripletex-mcp");
    }

    #[test]
    fn a_team_wide_catch_all_also_keeps_its_place() {
        // The catch-alls people actually write are usually a team route, which
        // `validate` cannot recognise as one — it has a `match`. The README
        // tells you to keep it last, so adoption does.
        let text = r#"[[route]]
match = { label = "tally" }
workspace = "tally"
repo = "~/dev/tally"
runtime = "claude-code"

# last: anything in the team with no repo label
[[route]]
match = { linear_team = "AGE" }
workspace = "herdr-board"
repo = "~/dev/herdr-board"
runtime = "claude-code"
"#;
        let out = adopt_text(text, &unadopted());
        let parsed = cfg(&out);
        assert_eq!(parsed.routes.len(), 3);
        assert_eq!(
            parsed.routes[2].match_.linear_team.as_deref(),
            Some("AGE"),
            "the team route stopped being last:\n{out}"
        );
        assert_eq!(
            parsed.routes[1].match_.gh_repo.as_deref(),
            Some("Florin-AS/tripletex-mcp")
        );
    }

    #[test]
    fn the_catch_alls_own_comment_stays_with_it() {
        let out = adopt_text(CATCH_ALL, &unadopted());
        let lines: Vec<&str> = out.lines().collect();
        let comment = lines
            .iter()
            .position(|l| l.contains("Anything else lands in the scratch"))
            .unwrap();
        assert!(
            lines[comment + 1].trim() == "[[route]]"
                && lines[comment + 2].contains("scratch"),
            "the insertion split a comment from its route:\n{out}"
        );
    }

    #[test]
    fn adopting_writes_both_halves() {
        let out = adopt_text(CATCH_ALL, &unadopted());
        let parsed = cfg(&out);
        assert!(
            parsed.github.repos.iter().any(|r| r == "Florin-AS/tripletex-mcp"),
            "the repo was not added to [github] repos:\n{out}"
        );
        assert_eq!(parsed.github.repos.len(), 2, "the existing one survived");
    }

    #[test]
    fn only_the_missing_half_is_written() {
        let mut u = unadopted();
        u.missing = Missing::Polling;
        let out = adopt_text(CATCH_ALL, &u);
        assert_eq!(cfg(&out).routes.len(), 2, "no route was needed:\n{out}");
        assert!(cfg(&out).github.repos.iter().any(|r| r.contains("tripletex")));

        let mut u = unadopted();
        u.missing = Missing::Route;
        let out = adopt_text(CATCH_ALL, &u);
        assert_eq!(cfg(&out).routes.len(), 3);
        assert_eq!(cfg(&out).github.repos.len(), 1, "repos was left alone");
    }

    #[test]
    fn the_linear_label_is_offered_as_a_comment_and_never_as_config() {
        // A git remote cannot name a Linear label. Writing a guess as live
        // config would be inventing one; a commented block says it is a guess
        // and costs nothing if it is wrong.
        let out = adopt_text(CATCH_ALL, &unadopted());
        assert!(out.contains("# match = { label = \"tripletex-mcp\" }"), "{out}");
        assert!(
            !cfg(&out).routes.iter().any(|r| r.match_.label.is_some()),
            "the guess became real config"
        );
        assert!(out.contains("guess"), "it has to say it is a guess:\n{out}");
    }

    #[test]
    fn a_file_with_no_routes_at_all_still_gets_one() {
        let out = adopt_text("[sync]\ninterval = \"30s\"\n", &unadopted());
        let parsed = cfg(&out);
        assert_eq!(parsed.routes.len(), 1);
        assert_eq!(parsed.sync.interval, "30s", "the rest survived:\n{out}");
    }

    #[test]
    fn a_route_inside_a_prompt_string_is_not_mistaken_for_a_header() {
        // The shipped example config has a `prompt = """..."""`, and a prompt
        // that mentions `[[route]]` would otherwise move every insertion point.
        let text = "[[route]]\nmatch = { gh_repo = \"o/a\" }\nworkspace = \"a\"\nrepo = \"/a\"\nruntime = \"claude\"\nprompt = \"\"\"\n[github] is not a table here\n[[route]] neither\n\"\"\"\n\n[[route]]\nworkspace = \"catch\"\nrepo = \"/c\"\nruntime = \"claude\"\n";
        assert_eq!(
            header_lines(text)
                .iter()
                .filter(|(_, n)| n == "[[route]]")
                .count(),
            2,
            "the prompt body was read as config"
        );
        let out = adopt_text(text, &unadopted());
        let parsed = cfg(&out);
        assert_eq!(parsed.routes.len(), 3);
        assert!(parsed.routes[2].match_.is_empty(), "the catch-all is last");
        assert!(
            parsed.routes[0].prompt.as_deref().unwrap().contains("[[route]] neither"),
            "the prompt was edited"
        );
    }

    #[test]
    fn arrays_grow_however_they_are_written() {
        for (input, want) in [
            ("[github]\nrepos = []\n", vec!["o/new"]),
            ("[github]\nrepos = [\"a/b\"]\n", vec!["a/b", "o/new"]),
            (
                "[github]\nrepos = [\n  \"a/b\",\n]\n",
                vec!["a/b", "o/new"],
            ),
            // No `repos` key at all.
            ("[github]\nlabels = []\n", vec!["o/new"]),
            // No `[github]` table at all.
            ("[sync]\ninterval = \"30s\"\n", vec!["o/new"]),
        ] {
            let out = add_to_array(input, "github", "repos", "o/new", NEW_GITHUB_TABLE);
            assert_eq!(cfg(&out).github.repos, want, "from:\n{input}\ngot:\n{out}");
        }
    }

    #[test]
    fn ignoring_writes_a_list_that_detection_then_honours() {
        let text = add_to_array("[sync]\n", "adopt", "ignore", "o/vendor", NEW_ADOPT_TABLE);
        let parsed = cfg(&text);
        assert!(parsed.adopt.ignores("O/Vendor"));
        // And it round-trips: ignoring a second repo keeps the first.
        let text = add_to_array(&text, "adopt", "ignore", "o/other", NEW_ADOPT_TABLE);
        assert_eq!(cfg(&text).adopt.ignore, vec!["o/vendor", "o/other"]);
    }

    #[test]
    fn the_runtime_copies_whatever_the_other_routes_use() {
        // A route that disagrees with every other one is a surprise.
        let c = cfg(
            "[[route]]\nmatch = { label = \"a\" }\nworkspace = \"a\"\nrepo = \"/a\"\nruntime = \"codex\"\n\n[[route]]\nmatch = { label = \"b\" }\nworkspace = \"b\"\nrepo = \"/b\"\nruntime = \"codex\"\n",
        );
        assert_eq!(habitual_runtime(&c), "codex");
        assert_eq!(habitual_runtime(&cfg("")), "claude-code");
    }

    #[test]
    fn a_broken_edit_never_reaches_the_file() {
        // The gate is the point: this is a one-keypress writer aimed at a file
        // somebody hand-maintains.
        let dir = std::env::temp_dir().join(format!("hb-adopt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("routing.toml");
        std::fs::write(&path, CATCH_ALL).unwrap();

        let err = apply(&path, CATCH_ALL, "this is not toml = = =").unwrap_err();
        assert!(format!("{err:#}").contains("untouched"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CATCH_ALL);

        // ...including an edit that parses but would shadow a route.
        let shadowed = format!(
            "[[route]]\nworkspace = \"c\"\nrepo = \"/c\"\nruntime = \"claude\"\n{CATCH_ALL}"
        );
        let err = apply(&path, CATCH_ALL, &shadowed).unwrap_err();
        assert!(format!("{err:#}").contains("untouched"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CATCH_ALL);

        // A good edit lands, and the previous contents are kept beside it.
        let good = adopt_text(CATCH_ALL, &unadopted());
        apply(&path, CATCH_ALL, &good).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), good);
        assert_eq!(
            std::fs::read_to_string(backup_path(&path)).unwrap(),
            CATCH_ALL
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_keys_drive_a_real_file_from_silent_to_watched() {
        // What `a` and `X` actually do, through the public entry points and a
        // routing.toml on disk.
        let dir = std::env::temp_dir().join(format!("hb-adopt-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("routing.toml");
        std::fs::write(&path, CATCH_ALL).unwrap();

        let workspaces = [
            ws("tripletex-mcp", Some("/code/tm"), false),
            ws("vendor", Some("/code/v"), false),
        ];
        let remote = remotes(&[
            ("/code/tm", "git@github.com:Florin-AS/tripletex-mcp.git"),
            ("/code/v", "git@github.com:SomeOrg/vendor.git"),
        ]);

        let reload = |path: &Path| RoutingConfig::load(path).unwrap();
        let found = detect(&workspaces, &reload(&path), &remote);
        assert_eq!(found.len(), 2);

        // `a` on the one you work in.
        let tm = found.iter().find(|u| u.name() == "tripletex-mcp").unwrap();
        let done = adopt(&path, tm).unwrap();
        assert!(done.wrote_route && done.wrote_repo);
        assert_eq!(done.suggested_label, "tripletex-mcp");

        // `X` on the one you are only reading.
        let vendor = found.iter().find(|u| u.name() == "vendor").unwrap();
        ignore(&path, &vendor.slug).unwrap();

        let cfg = reload(&path);
        assert!(detect(&workspaces, &cfg, &remote).is_empty(), "still offered");
        assert!(cfg.github.repos.iter().any(|r| r == "Florin-AS/tripletex-mcp"));
        assert!(cfg.adopt.ignores("SomeOrg/vendor"));
        assert!(
            cfg.routes.last().unwrap().match_.is_empty(),
            "the catch-all stopped being last"
        );
        // Pressing `X` again is a no-op rather than a corrupted array.
        ignore(&path, &vendor.slug).unwrap();
        assert_eq!(reload(&path).adopt.ignore.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adopting_every_workspace_leaves_nothing_to_adopt() {
        // The round trip the operator actually performs: detect, adopt, and the
        // row is gone — including the catch-all still being last afterwards.
        let workspaces = [
            ws("tripletex-mcp", Some("/code/tm"), false),
            ws("brreg", Some("/code/brreg"), false),
        ];
        let remote = remotes(&[
            ("/code/tm", "git@github.com:Florin-AS/tripletex-mcp.git"),
            ("/code/brreg", "https://github.com/Florin-AS/brreg.git"),
        ]);
        let mut text = CATCH_ALL.to_string();
        loop {
            let parsed = cfg(&text);
            let found = detect(&workspaces, &parsed, &remote);
            let Some(u) = found.first() else { break };
            text = adopt_text(&text, u);
        }
        let parsed = cfg(&text);
        assert_eq!(parsed.github.repos.len(), 3);
        assert!(parsed.routes.last().unwrap().match_.is_empty());
        assert!(detect(&workspaces, &parsed, &remote).is_empty());
    }
}
