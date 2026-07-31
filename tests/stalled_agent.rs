//! Integration test: a dead agent that reports `working` forever (gh#32).
//!
//! Three attempts sat `working` for over an hour after their agents died on
//! Anthropic 5xx errors, because `board_working_spinner` matched spinner-glyph
//! lines Claude Code had left in scrollback. Nothing could ever settle them, so
//! nothing reached the operator or the pane that released them, and all three
//! kept counting against `max_concurrent_per_workspace`.
//!
//! The unit tests in `screen` cover the rule that decides it. This covers the
//! wiring: that reconciliation actually reads the pane, actually overrules herdr,
//! and actually says so — through the real `herdr pane read` argv, served by a
//! stub binary on `HERDR_BIN_PATH`.

use anyhow::Result;
use herdr_board::config::Paths;
use herdr_board::db::{Db, NewAttempt, UpsertTask};
use herdr_board::herdr::{Herdr, PaneInfo};
use herdr_board::model::{AgentStatus, BoardState, Source, UpstreamState};
use herdr_board::sources::linear::GraphQl;
use herdr_board::sync::SyncEngine;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// A dead agent, in the shape gh#32 found all three in: the idle welcome state,
/// a spinner line from the turn that died still in scrollback above it, and the
/// cause printed outright.
const DEAD_ON_A_529: &str = "\
⏺ Calling claude-in-chrome 5 times, running 8 shell commands…
  ⎿  $ pnpm --filter @itsm/ui run build 2>&1 | tail -5

✳ Tinkering… (31m 25s · ↓ 129.0k tokens)
  ⎿  Tip: Use /clear to start fresh when switching topics and free up context

⏺ API Error: 529 Overloaded. This is a server-side issue, usually temporary
──────────────────────────────────────────
❯
──────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents
";

/// The other shape gh#32 found: frozen, but with nothing on screen naming a
/// cause. `screen::api_error` is deliberately narrow, and this is what it
/// declines to guess at.
const DEAD_WITHOUT_A_REASON: &str = "\
⏺ 111
✻ Crunched for 3m 19s
  ⎿  Tip: Ask Claude to create subagents for specific tasks.
──────────────────────────────────────────
❯
──────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents
";

/// The same pane a second later, if it were alive. Captured 14s apart from a
/// genuinely working agent: the elapsed timer is the difference, and it is the
/// whole reason this check is safe to run against agents that are fine.
const ALIVE: &str = "\
✳ Tinkering… (31m 39s · ↓ 131.2k tokens)
──────────────────────────────────────────
❯
──────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents
";

// ---- the stub herdr ------------------------------------------------------

/// Where the stub binary and its canned screens live.
///
/// One per test *binary*, because `HERDR_BIN_PATH` is process-global and cargo
/// runs these on threads. Tests stay independent by using their own pane ids and
/// their own task identifiers, which is what the argv log is filtered on.
fn stub() -> &'static Path {
    static STUB: OnceLock<PathBuf> = OnceLock::new();
    STUB.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("herdr-board-stub-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("screens")).unwrap();
        let bin = dir.join("herdr");
        std::fs::write(
            &bin,
            r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\n' "$*" >> "$dir/argv.log"
if [ "$1" = "pane" ] && [ "$2" = "read" ]; then
  f="$dir/screens/$(printf '%s' "$3" | tr ':' '_')"
  [ -f "$f" ] && cat "$f"
  exit 0
fi
echo '{"result":{}}'
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // SAFETY: single-threaded at this point — `OnceLock` serialises the
        // initialiser, and nothing in this binary reads the variable until a
        // `Herdr` is discovered, which happens after.
        unsafe { std::env::set_var("HERDR_BIN_PATH", &bin) };
        dir
    })
    .as_path()
}

fn herdr() -> Herdr {
    stub();
    Herdr::discover(Arc::new(herdr_board::log::Logger::new("", false)))
}

/// Put a screen on a pane, for the stub to serve.
fn show(pane: &str, screen: &str) {
    std::fs::write(stub().join("screens").join(pane.replace(':', "_")), screen).unwrap();
}

/// Every argv the stub was called with that mentions `needle`.
fn argv_mentioning(needle: &str) -> Vec<String> {
    std::fs::read_to_string(stub().join("argv.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(needle))
        .map(str::to_string)
        .collect()
}

// ---- the board under test ------------------------------------------------

const ROUTING: &str = r#"
[sync]
interval = "30s"

[[route]]
match = { linear_team = "OFF" }
workspace = "offhand"
repo = "/tmp/offhand"
runtime = "claude-code"
prompt = "Work on {identifier}: {title}"

[defaults]
notify = true
"#;

/// The same board with `nudge_stalled` on (gh#40). A separate string rather than
/// a mutation, so what the operator writes in `routing.toml` is what the test
/// parses.
const ROUTING_NUDGING: &str = r#"
[sync]
interval = "30s"

[[route]]
match = { linear_team = "OFF" }
workspace = "offhand"
repo = "/tmp/offhand"
runtime = "claude-code"
prompt = "Work on {identifier}: {title}"

[defaults]
notify = true
nudge_stalled = true
"#;

struct Board {
    dir: PathBuf,
    engine: SyncEngine,
}

impl Drop for Board {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Board {
    fn new(name: &str) -> Board {
        Board::with(name, ROUTING)
    }

    /// A board that has been asked to nudge stalled agents (gh#40).
    fn nudging(name: &str) -> Board {
        Board::with(name, ROUTING_NUDGING)
    }

    fn with(name: &str, routing: &str) -> Board {
        let dir = std::env::temp_dir().join(format!(
            "herdr-board-gh32-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths {
            config_dir: dir.clone(),
            state_dir: dir.clone(),
        };
        let engine = SyncEngine {
            db: Db::open(&paths.db()).unwrap(),
            cfg: toml::from_str(routing).unwrap(),
            credentials: Default::default(),
            paths,
            log: Arc::new(herdr_board::log::Logger::new("", false)),
            linear: None::<herdr_board::sources::linear::Linear<Box<dyn GraphQl>>>,
            github: None,
        };
        Board { dir, engine }
    }

    /// A task with one live attempt in `pane`, as dispatch would leave it.
    fn dispatched(&self, identifier: &str, pane: &str) -> Result<i64> {
        let id = format!("linear:{identifier}");
        self.engine.db.upsert_task(&UpsertTask {
            id: id.clone(),
            source: Source::Linear,
            source_id: "n1".into(),
            identifier: identifier.into(),
            title: "Something that takes a while".into(),
            body: None,
            url: format!("https://linear.app/x/issue/{identifier}"),
            labels: vec![],
            source_state: None,
            linear_team: Some("OFF".into()),
            linear_project: None,
            upstream: UpstreamState::Started,
            updated_at: herdr_board::db::now(),
        })?;
        let attempt = self.engine.db.insert_attempt(&NewAttempt {
            task_id: id,
            pane_id: None,
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: Some(format!("board/{}", identifier.to_lowercase())),
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
        })?;
        self.engine.db.set_attempt_pane(attempt, pane)?;
        // The agent got going: `saw_working` is latched long before it dies, and
        // without it the AGE-19 guard would be the thing holding the attempt
        // open rather than the bogus `working`.
        self.engine.db.set_saw_working(attempt)?;
        Ok(attempt)
    }

    /// Backdate the recorded screen sample, standing in for the seconds between
    /// two reconciles. `set_screen_sample` always stamps *now*, which is right in
    /// production and useless in a test that cannot wait.
    fn sample_taken_secs_ago(&self, attempt: i64, screen: &str, secs: i64) {
        let at = herdr_board::db::rfc3339(
            chrono::Utc::now() - chrono::Duration::seconds(secs),
        );
        self.engine
            .db
            .conn
            .execute(
                "UPDATE attempts SET screen_print = ?2, screen_at = ?3 WHERE id = ?1",
                rusqlite::params![attempt, herdr_board::screen::fingerprint(screen), at],
            )
            .unwrap();
    }

    /// Put an attempt part-way through a nudge sequence, with its last nudge
    /// backdated. Standing in for the minutes between tries, which a test cannot
    /// wait out (gh#40).
    fn nudged_secs_ago(&self, attempt: i64, nudges: i64, secs: i64) {
        let at = herdr_board::db::rfc3339(chrono::Utc::now() - chrono::Duration::seconds(secs));
        self.engine
            .db
            .conn
            .execute(
                "UPDATE attempts SET nudges = ?2, nudged_at = ?3 WHERE id = ?1",
                rusqlite::params![attempt, nudges, at],
            )
            .unwrap();
    }

    fn nudges(&self, attempt: i64) -> i64 {
        self.engine
            .db
            .live_attempts()
            .unwrap()
            .into_iter()
            .find(|a| a.id == attempt)
            .expect("the attempt is still live")
            .nudges
    }

    fn state(&self, identifier: &str) -> BoardState {
        self.engine
            .db
            .get_task(&format!("linear:{identifier}"))
            .unwrap()
            .unwrap()
            .state
    }

    fn live_status(&self, attempt: i64) -> Option<AgentStatus> {
        self.engine
            .db
            .live_attempts()
            .unwrap()
            .into_iter()
            .find(|a| a.id == attempt)
            .expect("the attempt is still live")
            .agent_status
    }

    /// One reconcile, with the handle — the daemon's own path.
    fn tick(&self, panes: &[PaneInfo]) {
        let h = herdr();
        self.engine.reconcile_with(panes, Some(&h)).unwrap();
        self.engine.rederive_all().unwrap();
    }
}

fn working_pane(id: &str) -> PaneInfo {
    PaneInfo {
        pane_id: id.into(),
        workspace_id: "w1".into(),
        tab_id: Some("w1:t1".into()),
        agent: Some("claude".into()),
        agent_status: Some(AgentStatus::Working),
        focused: false,
        label: None,
        cwd: None,
        scroll_offset: 0,
    }
}

// ---- the tests -----------------------------------------------------------

/// The bug, end to end. herdr reports `working` off a stale `✳ Tinkering…`; the
/// screen has not moved in half a minute; the row must stop claiming an agent is
/// on it, and the operator must be told why.
#[test]
fn a_frozen_pane_stops_being_reported_as_working_and_names_the_error() {
    let b = Board::new("frozen");
    let attempt = b.dispatched("OFF-101", "w1:p101").unwrap();
    show("w1:p101", DEAD_ON_A_529);
    b.sample_taken_secs_ago(attempt, DEAD_ON_A_529, 40);

    b.tick(&[working_pane("w1:p101")]);

    assert_eq!(
        b.live_status(attempt),
        Some(AgentStatus::Blocked),
        "herdr said working; nothing had drawn to the pane for 40s"
    );
    assert_eq!(
        b.state("OFF-101"),
        BoardState::Blocked,
        "a stalled attempt belongs where the operator looks first"
    );
    // The attempt is deliberately *not* closed: the pane is still there with its
    // checkout and its history, and retrying or cancelling is a person's call.
    assert!(
        b.engine
            .db
            .live_attempts()
            .unwrap()
            .iter()
            .any(|a| a.id == attempt),
        "the attempt stays open — `cancel` is what returns it to ready"
    );

    // Told why, not merely that. A stall alone does not say `529`.
    let toasts = argv_mentioning("OFF-101 stalled");
    assert_eq!(toasts.len(), 1, "{toasts:?}");
    assert!(toasts[0].contains("529 Overloaded"), "{toasts:?}");

    // And told once. A pane can sit like this for hours.
    b.tick(&[working_pane("w1:p101")]);
    assert_eq!(b.live_status(attempt), Some(AgentStatus::Blocked));
    assert_eq!(argv_mentioning("OFF-101 stalled").len(), 1);
}

/// The false positive that matters: an agent that is genuinely working must be
/// left alone. Its spinner carries an elapsed timer, so its screen cannot hold
/// still — this is the same pane one tick later, and the only difference is that
/// timer.
#[test]
fn an_agent_whose_screen_is_moving_is_still_working() {
    let b = Board::new("alive");
    let attempt = b.dispatched("OFF-102", "w1:p102").unwrap();
    show("w1:p102", ALIVE);
    b.sample_taken_secs_ago(attempt, DEAD_ON_A_529, 40);

    b.tick(&[working_pane("w1:p102")]);

    assert_eq!(b.live_status(attempt), Some(AgentStatus::Working));
    assert_eq!(b.state("OFF-102"), BoardState::Working);
    assert!(argv_mentioning("OFF-102 stalled").is_empty());

    // ...and the screen it moved to is what the next tick compares against.
    let live = b.engine.db.live_attempts().unwrap();
    let a = live.iter().find(|a| a.id == attempt).unwrap();
    assert_eq!(
        a.screen_print.as_deref(),
        Some(herdr_board::screen::fingerprint(ALIVE).as_str())
    );
}

/// One sample settles nothing. The first time an attempt is reconciled there is
/// nothing to compare against, and calling that a stall would flag every agent
/// the moment it was dispatched.
#[test]
fn the_first_look_at_a_pane_accuses_nobody() {
    let b = Board::new("first");
    let attempt = b.dispatched("OFF-103", "w1:p103").unwrap();
    show("w1:p103", DEAD_ON_A_529);

    b.tick(&[working_pane("w1:p103")]);

    assert_eq!(b.live_status(attempt), Some(AgentStatus::Working));
    assert!(argv_mentioning("OFF-103 stalled").is_empty());
    // It did record the screen, which is what makes the *next* tick able to tell.
    let live = b.engine.db.live_attempts().unwrap();
    let a = live.iter().find(|a| a.id == attempt).unwrap();
    assert_eq!(
        a.screen_print.as_deref(),
        Some(herdr_board::screen::fingerprint(DEAD_ON_A_529).as_str())
    );
    assert!(a.screen_at.is_some());
}

/// `pane.agent_status_changed` fires reconciliation between daemon cycles, and
/// two reads a second apart being equal proves nothing — the spinner's frame
/// only rotates every 300ms.
#[test]
fn two_reads_a_moment_apart_are_not_a_stall() {
    let b = Board::new("tooquick");
    let attempt = b.dispatched("OFF-104", "w1:p104").unwrap();
    show("w1:p104", DEAD_ON_A_529);
    b.sample_taken_secs_ago(attempt, DEAD_ON_A_529, 1);

    b.tick(&[working_pane("w1:p104")]);

    assert_eq!(b.live_status(attempt), Some(AgentStatus::Working));
    assert!(argv_mentioning("OFF-104 stalled").is_empty());
}

/// Somebody reading the pane's scrollback freezes the visible screen for a
/// working agent too. The agent must not be blamed for the operator scrolling.
#[test]
fn a_pane_somebody_is_scrolled_back_in_is_not_judged() {
    let b = Board::new("scrolled");
    let attempt = b.dispatched("OFF-105", "w1:p105").unwrap();
    show("w1:p105", DEAD_ON_A_529);
    b.sample_taken_secs_ago(attempt, DEAD_ON_A_529, 40);

    let mut pane = working_pane("w1:p105");
    pane.scroll_offset = 12;
    b.tick(&[pane]);

    assert_eq!(b.live_status(attempt), Some(AgentStatus::Working));
    assert!(argv_mentioning("OFF-105 stalled").is_empty());
}

/// A pull request outranks the stall. The agent declared itself finished before
/// it froze, so the attempt has an end — and this is the case that used to be
/// unreachable: `working` returned before the pull request was ever looked at.
#[test]
fn a_frozen_pane_that_left_a_pull_request_settles_instead() {
    let b = Board::new("withpr");
    let attempt = b.dispatched("OFF-106", "w1:p106").unwrap();
    b.engine
        .db
        .conn
        .execute(
            "UPDATE tasks SET pr_open = 1, pr_number = 7,
                    pr_url = 'https://github.com/o/r/pull/7'
             WHERE id = 'linear:OFF-106'",
            [],
        )
        .unwrap();
    show("w1:p106", DEAD_ON_A_529);
    b.sample_taken_secs_ago(attempt, DEAD_ON_A_529, 40);

    b.tick(&[working_pane("w1:p106")]);

    assert_eq!(b.state("OFF-106"), BoardState::Review);
    assert!(
        b.engine
            .db
            .live_attempts()
            .unwrap()
            .iter()
            .all(|a| a.id != attempt),
        "a settle closes the attempt"
    );
    let toasts = argv_mentioning("OFF-106");
    assert!(
        toasts.iter().any(|t| t.contains("finished")),
        "the operator hears that it finished, not that it stalled: {toasts:?}"
    );
    assert!(!toasts.iter().any(|t| t.contains("stalled")), "{toasts:?}");
}

// ---- gh#40: nudging the pane instead of leaving it parked -----------------

/// Every `agent prompt` the stub was handed for this pane.
fn prompts_into(pane: &str) -> Vec<String> {
    argv_mentioning(&format!("agent prompt {pane}"))
}

/// The point of gh#40. gh#32 saw the stall and stopped there; three agents then
/// sat parked for 30, 62 and 65 minutes, and a prompt typed by hand was all any
/// of them needed. So type it.
#[test]
fn a_stalled_agent_is_prompted_to_carry_on() {
    let b = Board::nudging("nudge");
    let attempt = b.dispatched("OFF-110", "w1:p110").unwrap();
    show("w1:p110", DEAD_ON_A_529);
    b.sample_taken_secs_ago(attempt, DEAD_ON_A_529, 40);

    b.tick(&[working_pane("w1:p110")]);

    let sent = prompts_into("w1:p110");
    assert_eq!(sent.len(), 1, "{sent:?}");
    // It tells the agent what happened, because from inside the session the last
    // thing that happened is a turn that produced nothing.
    assert!(sent[0].contains("529 Overloaded"), "{sent:?}");
    assert!(sent[0].contains("herdr-board"), "{sent:?}");
    // Into the pane it is already in. A re-dispatch would throw away half an
    // hour of context to recover from a transient server error — and would show
    // up here as a second attempt.
    let attempts = b.engine.db.attempts_for("linear:OFF-110").unwrap();
    assert_eq!(attempts.len(), 1, "nudged, not re-dispatched");
    assert_eq!(attempts[0].pane_id.as_deref(), Some("w1:p110"));
    assert_eq!(b.nudges(attempt), 1);
    // And it is still reported for what it is until it proves otherwise.
    assert_eq!(b.live_status(attempt), Some(AgentStatus::Blocked));
}

/// Off by default. A board that types into panes unprompted is a different thing
/// from one that watches them, and that is opted into.
#[test]
fn nothing_is_typed_into_a_pane_nobody_asked_for() {
    let b = Board::new("quiet");
    let attempt = b.dispatched("OFF-111", "w1:p111").unwrap();
    show("w1:p111", DEAD_ON_A_529);
    b.sample_taken_secs_ago(attempt, DEAD_ON_A_529, 40);

    b.tick(&[working_pane("w1:p111")]);

    assert!(prompts_into("w1:p111").is_empty());
    assert_eq!(b.nudges(attempt), 0);
    // The reporting gh#32 added is unchanged by the flag being off.
    assert_eq!(b.live_status(attempt), Some(AgentStatus::Blocked));
}

/// A stall with no error named is not a 5xx, and nothing is typed into it.
/// `screen::api_error` is the narrow part, and this is the wiring honouring it.
#[test]
fn a_stall_with_no_server_error_on_it_is_not_nudged() {
    let b = Board::nudging("noerror");
    let attempt = b.dispatched("OFF-112", "w1:p112").unwrap();
    show("w1:p112", DEAD_WITHOUT_A_REASON);
    b.sample_taken_secs_ago(attempt, DEAD_WITHOUT_A_REASON, 40);

    b.tick(&[working_pane("w1:p112")]);

    assert_eq!(b.live_status(attempt), Some(AgentStatus::Blocked));
    assert!(prompts_into("w1:p112").is_empty());
}

/// Back off between tries. A pane nudged moments ago has not had time to answer,
/// and hammering it would tell us nothing a wait does not.
#[test]
fn a_pane_nudged_a_moment_ago_is_left_alone() {
    let b = Board::nudging("backoff");
    let attempt = b.dispatched("OFF-113", "w1:p113").unwrap();
    show("w1:p113", DEAD_ON_A_529);
    b.sample_taken_secs_ago(attempt, DEAD_ON_A_529, 40);
    b.nudged_secs_ago(attempt, 1, 30);

    b.tick(&[working_pane("w1:p113")]);
    assert!(prompts_into("w1:p113").is_empty());
    assert_eq!(b.nudges(attempt), 1, "and the try is not spent either");

    // Once the wait is up, it gets its second.
    b.nudged_secs_ago(attempt, 1, 200);
    b.sample_taken_secs_ago(attempt, DEAD_ON_A_529, 40);
    b.tick(&[working_pane("w1:p113")]);
    assert_eq!(prompts_into("w1:p113").len(), 1);
    assert_eq!(b.nudges(attempt), 2);
}

/// The cap. An agent that has ignored three nudges is not suffering from a 5xx
/// any more, and going on nudging it would hide that behind a board that looks
/// busy. It stays blocked, and says why.
#[test]
fn a_pane_that_ignored_every_nudge_is_left_blocked_and_said_so() {
    let b = Board::nudging("capped");
    let attempt = b.dispatched("OFF-114", "w1:p114").unwrap();
    show("w1:p114", DEAD_ON_A_529);
    b.sample_taken_secs_ago(attempt, DEAD_ON_A_529, 40);
    b.nudged_secs_ago(attempt, herdr_board::nudge::MAX_NUDGES, 3_600);

    b.tick(&[working_pane("w1:p114")]);

    assert!(
        prompts_into("w1:p114").is_empty(),
        "three was the allowance"
    );
    assert_eq!(b.live_status(attempt), Some(AgentStatus::Blocked));
    assert_eq!(b.state("OFF-114"), BoardState::Blocked);
    // The toast is cut at 78 characters, so both facts have to fit inside them:
    // that the board tried, and what it was up against.
    let toasts = argv_mentioning("OFF-114 stalled");
    assert_eq!(toasts.len(), 1, "{toasts:?}");
    assert!(toasts[0].contains("after 3 nudges"), "{toasts:?}");
    assert!(toasts[0].contains("529"), "{toasts:?}");
}

/// A nudge that worked costs nothing later. The counter resets — but only once
/// the pane has been moving long enough for the movement to be the agent rather
/// than the echo of the nudge itself.
#[test]
fn a_pane_that_came_back_gets_its_nudges_back() {
    let b = Board::nudging("recovered");
    let attempt = b.dispatched("OFF-115", "w1:p115").unwrap();
    show("w1:p115", ALIVE);
    b.sample_taken_secs_ago(attempt, DEAD_ON_A_529, 40);

    // The screen moved, but only seconds after we typed into it. That is what a
    // nudge looks like being echoed, and clearing here is what would make the
    // cap unreachable.
    b.nudged_secs_ago(attempt, 1, 5);
    b.tick(&[working_pane("w1:p115")]);
    assert_eq!(b.live_status(attempt), Some(AgentStatus::Working));
    assert_eq!(b.nudges(attempt), 1, "an echo is not a recovery");

    // Still moving when its next nudge would have been due. An echo is one
    // frame; this is an agent.
    b.sample_taken_secs_ago(attempt, DEAD_ON_A_529, 40);
    b.nudged_secs_ago(attempt, 1, 200);
    b.tick(&[working_pane("w1:p115")]);
    assert_eq!(b.nudges(attempt), 0);
}

/// The pane may be holding somebody else by now — a handoff, or an agent that
/// exited and left a shell behind. Typing a prompt at bash is the failure this
/// check exists for, and it is the same one review delivery makes.
#[test]
fn a_pane_that_no_longer_holds_the_agent_is_not_typed_into() {
    let b = Board::nudging("gone");
    let attempt = b.dispatched("OFF-116", "w1:p116").unwrap();
    show("w1:p116", DEAD_ON_A_529);
    b.sample_taken_secs_ago(attempt, DEAD_ON_A_529, 40);

    let mut pane = working_pane("w1:p116");
    pane.agent = None;
    b.tick(&[pane]);

    assert!(prompts_into("w1:p116").is_empty());
    assert_eq!(b.nudges(attempt), 0);
}
