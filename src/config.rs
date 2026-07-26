//! Plugin directories, `.env` secrets, and `routing.toml` (impl spec §5).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// herdr's supported `agent start --kind` values, verbatim from the 0.7.5 CLI
/// reference. Routing config names a *runtime*; this is what herdr will accept.
pub const HERDR_AGENT_KINDS: &[&str] = &[
    "pi",
    "claude",
    "codex",
    "gemini",
    "cursor",
    "devin",
    "agy",
    "cline",
    "omp",
    "mastracode",
    "opencode",
    "copilot",
    "kimi",
    "kiro",
    "droid",
    "amp",
    "grok",
    "hermes",
    "kilo",
    "qodercli",
    "maki",
];

/// Map a routing-config runtime name onto a herdr agent kind.
///
/// The impl spec's example config says `runtime = "claude-code"`, which is not a
/// herdr kind — herdr calls it `claude`. Rather than silently rewriting the
/// operator's config (the design renders the runtime name in a 12-cell column,
/// and `claude-code` is what they wrote), we keep the display name and translate
/// only at the point of invocation.
pub fn herdr_kind_for_runtime(runtime: &str) -> Option<&'static str> {
    let alias = match runtime {
        "claude-code" | "claude" => "claude",
        "openai-codex" | "codex" => "codex",
        "github-copilot" | "copilot" => "copilot",
        "opencode" => "opencode",
        other => other,
    };
    HERDR_AGENT_KINDS.iter().find(|k| **k == alias).copied()
}

/// Where herdr told us to put things. Falls back to XDG-ish paths so `doctor`,
/// `demo`, and tests work outside a plugin invocation.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

/// Our plugin id, as declared in `herdr-plugin.toml`.
pub const PLUGIN_ID: &str = "board";

/// Ask herdr where our config lives.
///
/// Only used when `HERDR_PLUGIN_CONFIG_DIR` is absent — i.e. when the binary is
/// run by hand rather than launched by herdr. herdr nests plugin config under
/// `plugins/config/<id>`, which is not a path worth guessing: the design spec
/// says to resolve it with `herdr plugin config-dir` rather than hardcoding.
fn ask_herdr_config_dir() -> Option<PathBuf> {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    let out = std::process::Command::new(bin)
        .args(["plugin", "config-dir", PLUGIN_ID])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

impl Paths {
    pub fn discover() -> Result<Paths> {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let config_dir = match std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
            Ok(v) if !v.is_empty() => PathBuf::from(v),
            _ => ask_herdr_config_dir().unwrap_or_else(|| {
                PathBuf::from(&home).join(format!("{}{}", ".config/herdr/plugins/config/", PLUGIN_ID))
            }),
        };
        let state_dir = match std::env::var("HERDR_PLUGIN_STATE_DIR") {
            Ok(v) if !v.is_empty() => PathBuf::from(v),
            _ => PathBuf::from(&home).join(format!("{}{}", ".local/state/herdr/plugins/", PLUGIN_ID)),
        };
        std::fs::create_dir_all(&config_dir)
            .with_context(|| format!("creating config dir {}", config_dir.display()))?;
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("creating state dir {}", state_dir.display()))?;
        Ok(Paths {
            config_dir,
            state_dir,
        })
    }

    pub fn db(&self) -> PathBuf {
        self.state_dir.join("board.db")
    }
    pub fn routing(&self) -> PathBuf {
        self.config_dir.join("routing.toml")
    }
    pub fn env_file(&self) -> PathBuf {
        self.config_dir.join(".env")
    }
    pub fn pidfile(&self) -> PathBuf {
        self.state_dir.join("syncd.pid")
    }
    pub fn logfile(&self) -> PathBuf {
        self.state_dir.join("syncd.log")
    }
    pub fn worktree_root(&self) -> PathBuf {
        self.state_dir.join("wt")
    }
}

/// Load `.env` from the plugin config dir into the process environment. Values
/// already present in the environment win, so a shell export can override.
pub fn load_env(paths: &Paths) {
    let f = paths.env_file();
    if f.exists() {
        let _ = dotenvy::from_path(&f);
    }
}

pub fn linear_api_key() -> Option<String> {
    std::env::var("LINEAR_API_KEY").ok().filter(|s| !s.is_empty())
}

pub fn github_token() -> Option<String> {
    std::env::var("GITHUB_TOKEN").ok().filter(|s| !s.is_empty())
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default, rename = "route")]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub github: GithubConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyncConfig {
    /// Poll interval, e.g. `"30s"`.
    #[serde(default = "default_interval")]
    pub interval: String,
    /// Linear labels that mean "dispatchable".
    #[serde(default)]
    pub labels: Vec<String>,
}

fn default_interval() -> String {
    "30s".into()
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            interval: default_interval(),
            labels: Vec::new(),
        }
    }
}

impl SyncConfig {
    /// Parse `30s` / `5m` / `90` (bare = seconds). Clamped to a sane floor so a
    /// typo cannot hammer Linear.
    pub fn interval_secs(&self) -> u64 {
        parse_duration_secs(&self.interval).unwrap_or(30).max(5)
    }
}

pub fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        _ => (s, 1),
    };
    num.trim().parse::<u64>().ok().map(|n| n * mult)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Defaults {
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_per_workspace: usize,
    #[serde(default = "default_branch_template")]
    pub branch_template: String,
    /// Which way a dispatched agent's pane splits: `auto` (default) splits right
    /// into a tab with one pane and down once it already holds two, so a busy
    /// tab does not become a row of narrow columns. `right` or `down` force it.
    #[serde(default)]
    pub split_direction: Option<String>,
}

fn default_max_concurrent() -> usize {
    3
}

/// Impl spec §5. The design fixtures show `lin-145-altinn-retry`, but the design
/// handoff (#11) defers to the impl spec here, and it is config either way.
fn default_branch_template() -> String {
    "board/{identifier_lower}".into()
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            max_concurrent_per_workspace: default_max_concurrent(),
            branch_template: default_branch_template(),
            split_direction: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubConfig {
    /// `owner/repo` entries to poll for issues and PRs.
    #[serde(default)]
    pub repos: Vec<String>,
    /// Only surface issues carrying one of these labels. Empty = all.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Show open pull requests as their own `review` rows.
    ///
    /// A PR raised by a board dispatch attaches to its task instead; this is
    /// about the ones nobody dispatched, which are still work waiting on you.
    #[serde(default = "default_true")]
    pub pull_requests: bool,
    /// Leave the same trail on GitHub that Linear gets: a comment on dispatch
    /// and on outcome, and close the issue when the task is done.
    ///
    /// On by default, because the alternative — deriving state locally while
    /// upstream never changes — makes `d mark done` a key that undoes itself.
    /// Turn it off if the board should stay strictly read-only.
    #[serde(default = "default_true")]
    pub writeback: bool,
}

fn default_true() -> bool {
    true
}

impl Default for GithubConfig {
    fn default() -> Self {
        GithubConfig {
            repos: Vec::new(),
            labels: Vec::new(),
            pull_requests: true,
            writeback: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    /// Name shown in the picker and the prompt view. Defaults to the workspace.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "match")]
    pub match_: RouteMatch,
    pub workspace: String,
    pub repo: String,
    pub runtime: String,
    #[serde(default)]
    pub prompt: Option<String>,
    /// Per-route override of `defaults.branch_template`.
    #[serde(default)]
    pub branch_template: Option<String>,
    #[serde(default)]
    pub max_concurrent: Option<usize>,
}

impl Route {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.workspace)
    }

    /// Expand `~` in the configured repo path.
    pub fn repo_path(&self) -> PathBuf {
        expand_tilde(&self.repo)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RouteMatch {
    pub linear_team: Option<String>,
    pub linear_project: Option<String>,
    pub gh_repo: Option<String>,
    pub label: Option<String>,
}

impl RouteMatch {
    fn is_empty(&self) -> bool {
        self.linear_team.is_none()
            && self.linear_project.is_none()
            && self.gh_repo.is_none()
            && self.label.is_none()
    }
}

/// The reverse of [`expand_tilde`], for display: a home-relative path fits on a
/// terminal row where an absolute one gets truncated exactly where the useful
/// part is.
pub fn shorten_home(p: &Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && s.starts_with(&home) => {
            format!("~{}", &s[home.len()..])
        }
        _ => s,
    }
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

/// The facts about a task a route can match on.
#[derive(Debug, Clone, Default)]
pub struct RouteContext {
    pub linear_team: Option<String>,
    pub linear_project: Option<String>,
    pub gh_repo: Option<String>,
    pub labels: Vec<String>,
}

impl RoutingConfig {
    pub fn load(path: &Path) -> Result<RoutingConfig> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: RoutingConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Parse without validating. `doctor` uses this so that one bad route does
    /// not hide the problems in every other route.
    pub fn load_unvalidated(path: &Path) -> Result<RoutingConfig> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn check(&self) -> Result<()> {
        self.validate()
    }

    /// Load if present; an absent file is not an error (the board renders with
    /// every row marked `no route`, which is the honest empty state).
    pub fn load_or_default(path: &Path) -> RoutingConfig {
        RoutingConfig::load(path).unwrap_or_else(|_| RoutingConfig {
            sync: SyncConfig::default(),
            routes: Vec::new(),
            defaults: Defaults::default(),
            github: GithubConfig::default(),
        })
    }

    fn validate(&self) -> Result<()> {
        for (i, r) in self.routes.iter().enumerate() {
            if r.match_.is_empty() {
                // A catch-all route is legal and useful, but it must be last or
                // it silently shadows everything after it.
                if i + 1 != self.routes.len() {
                    bail!(
                        "route {} ({}) has an empty `match` but is not last; \
                         first matching route wins, so it would shadow the {} route(s) after it",
                        i + 1,
                        r.display_name(),
                        self.routes.len() - i - 1
                    );
                }
            }
            if herdr_kind_for_runtime(&r.runtime).is_none() {
                bail!(
                    "route {} ({}) has runtime `{}`, which is not a herdr agent kind. \
                     Known kinds: {}",
                    i + 1,
                    r.display_name(),
                    r.runtime,
                    HERDR_AGENT_KINDS.join(", ")
                );
            }
        }
        Ok(())
    }

    /// First matching route wins (impl spec §5).
    pub fn resolve(&self, ctx: &RouteContext) -> Option<&Route> {
        self.routes.iter().find(|r| route_matches(&r.match_, ctx))
    }

    pub fn branch_template<'a>(&'a self, route: &'a Route) -> &'a str {
        route
            .branch_template
            .as_deref()
            .unwrap_or(&self.defaults.branch_template)
    }

    pub fn max_concurrent(&self, route: &Route) -> usize {
        route
            .max_concurrent
            .unwrap_or(self.defaults.max_concurrent_per_workspace)
    }
}

/// All *specified* keys must match (AND). Unspecified keys are ignored.
fn route_matches(m: &RouteMatch, ctx: &RouteContext) -> bool {
    if let Some(team) = &m.linear_team
        && ctx.linear_team.as_deref() != Some(team.as_str())
    {
        return false;
    }
    if let Some(project) = &m.linear_project
        && ctx.linear_project.as_deref() != Some(project.as_str())
    {
        return false;
    }
    if let Some(repo) = &m.gh_repo
        && !ctx
            .gh_repo
            .as_deref()
            .is_some_and(|r| r.eq_ignore_ascii_case(repo))
    {
        return false;
    }
    if let Some(label) = &m.label
        && !ctx.labels.iter().any(|l| l.eq_ignore_ascii_case(label))
    {
        return false;
    }
    true
}

/// Interpolate `{key}` placeholders. Unknown placeholders are left untouched
/// rather than blanked, so a typo is visible in the prompt view instead of
/// silently sending an empty string to the agent.
pub fn interpolate(template: &str, vars: &BTreeMap<&str, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let key = &after[..end];
                match vars.get(key) {
                    Some(v) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('{');
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Slugify for branch names: lowercase, non-alphanumerics collapsed to `-`.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[sync]
interval = "30s"
labels = ["herd"]

[[route]]
match = { linear_team = "OFF" }
workspace = "offhand"
repo = "~/code/offhand"
runtime = "claude-code"
prompt = """
You are working on: {title} ({identifier})
{body}
"""

[[route]]
match = { label = "fintech" }
workspace = "fintech"
repo = "~/code/tripletex-int"
runtime = "claude-code"

[defaults]
max_concurrent_per_workspace = 3
branch_template = "board/{identifier_lower}"
"#;

    fn cfg() -> RoutingConfig {
        let c: RoutingConfig = toml::from_str(SAMPLE).unwrap();
        c.validate().unwrap();
        c
    }

    #[test]
    fn parses_the_spec_example() {
        let c = cfg();
        assert_eq!(c.routes.len(), 2);
        assert_eq!(c.sync.interval_secs(), 30);
        assert_eq!(c.sync.labels, vec!["herd"]);
        assert_eq!(c.defaults.max_concurrent_per_workspace, 3);
    }

    #[test]
    fn first_matching_route_wins() {
        let c = cfg();
        // Matches both the team route and the label route; the team route is
        // declared first.
        let ctx = RouteContext {
            linear_team: Some("OFF".into()),
            labels: vec!["fintech".into()],
            ..Default::default()
        };
        assert_eq!(c.resolve(&ctx).unwrap().workspace, "offhand");
    }

    #[test]
    fn label_route_matches_when_team_does_not() {
        let c = cfg();
        let ctx = RouteContext {
            linear_team: Some("TAL".into()),
            labels: vec!["fintech".into()],
            ..Default::default()
        };
        assert_eq!(c.resolve(&ctx).unwrap().workspace, "fintech");
    }

    #[test]
    fn unmatched_task_has_no_route() {
        let c = cfg();
        let ctx = RouteContext {
            linear_team: Some("TAL".into()),
            labels: vec!["chore".into()],
            ..Default::default()
        };
        assert!(c.resolve(&ctx).is_none());
    }

    #[test]
    fn match_keys_are_anded() {
        let c: RoutingConfig = toml::from_str(
            r#"
[[route]]
match = { linear_team = "OFF", label = "herd" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
"#,
        )
        .unwrap();
        let team_only = RouteContext {
            linear_team: Some("OFF".into()),
            ..Default::default()
        };
        assert!(c.resolve(&team_only).is_none());
        let both = RouteContext {
            linear_team: Some("OFF".into()),
            labels: vec!["herd".into()],
            ..Default::default()
        };
        assert!(c.resolve(&both).is_some());
    }

    #[test]
    fn unknown_runtime_is_rejected_with_the_known_kinds() {
        // The impl spec's own example says `claude-code`, which is not a herdr
        // kind; we accept it as an alias but reject genuine typos.
        let c: RoutingConfig = toml::from_str(
            r#"
[[route]]
match = { label = "x" }
workspace = "w"
repo = "/tmp"
runtime = "claude-codex"
"#,
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("not a herdr agent kind"), "{err}");
    }

    #[test]
    fn runtime_aliases_map_onto_herdr_kinds() {
        assert_eq!(herdr_kind_for_runtime("claude-code"), Some("claude"));
        assert_eq!(herdr_kind_for_runtime("claude"), Some("claude"));
        assert_eq!(herdr_kind_for_runtime("codex"), Some("codex"));
        assert_eq!(herdr_kind_for_runtime("opencode"), Some("opencode"));
        assert_eq!(herdr_kind_for_runtime("nonesuch"), None);
    }

    #[test]
    fn catch_all_route_must_be_last() {
        let c: RoutingConfig = toml::from_str(
            r#"
[[route]]
workspace = "catchall"
repo = "/tmp"
runtime = "claude"

[[route]]
match = { label = "fintech" }
workspace = "fintech"
repo = "/tmp"
runtime = "claude"
"#,
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("would shadow"), "{err}");
    }

    #[test]
    fn interpolation_fills_known_keys_and_preserves_typos() {
        let mut v = BTreeMap::new();
        v.insert("title", "Add retry".to_string());
        v.insert("identifier", "LIN-145".to_string());
        let out = interpolate("{identifier}: {title} [{nope}]", &v);
        assert_eq!(out, "LIN-145: Add retry [{nope}]");
    }

    #[test]
    fn branch_template_renders_the_spec_default() {
        let mut v = BTreeMap::new();
        v.insert("identifier_lower", "lin-145".to_string());
        assert_eq!(
            interpolate("board/{identifier_lower}", &v),
            "board/lin-145"
        );
    }

    #[test]
    fn home_paths_shorten_for_display() {
        // An absolute plugin path is long enough to be truncated on an 80-cell
        // row exactly where the filename is.
        unsafe { std::env::set_var("HOME", "/Users/x") };
        assert_eq!(
            shorten_home(Path::new("/Users/x/.config/herdr/plugins/config/board/.env")),
            "~/.config/herdr/plugins/config/board/.env"
        );
        assert_eq!(shorten_home(Path::new("/etc/hosts")), "/etc/hosts");
    }

    #[test]
    fn slugify_makes_branch_safe_text() {
        assert_eq!(slugify("Add retry to Altinn poller"), "add-retry-to-altinn-poller");
        assert_eq!(slugify("LIN-145"), "lin-145");
        assert_eq!(slugify("  weird///chars  "), "weird-chars");
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration_secs("30s"), Some(30));
        assert_eq!(parse_duration_secs("5m"), Some(300));
        assert_eq!(parse_duration_secs("1h"), Some(3600));
        assert_eq!(parse_duration_secs("90"), Some(90));
        assert_eq!(parse_duration_secs(""), None);
    }

    #[test]
    fn interval_has_a_floor() {
        let s = SyncConfig {
            interval: "0s".into(),
            labels: vec![],
        };
        assert_eq!(s.interval_secs(), 5);
    }
}
