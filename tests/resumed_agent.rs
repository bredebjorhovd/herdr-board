//! Integration test: an agent that kept working after the board closed it
//! (gh#34).
//!
//! gh#543 was settled on commits while its agent had another twenty minutes of
//! work in it. From that moment nothing looked at its pane again — reconciliation
//! only ever visited live attempts — so the row sat in `review` while the work
//! went on, the pane was never disposed of, and gh#32's stall detection could not
//! help because that runs for live attempts only.
//!
//! The exact inverse of gh#32, and it needs the same care: herdr's `working` is
//! the signal that lies, so re-opening on the status alone would flap every
//! finished task between `review` and `working` forever. The unit tests cover the
//! rules; this covers the wiring — that a closed attempt's pane is read at all,
//! that a moving screen re-opens it, and that a frozen one does not — through the
//! real `herdr pane read` argv, served by a stub binary on `HERDR_BIN_PATH`.

use anyhow::Result;
use herdr_board::config::Paths;
use herdr_board::db::{Db, NewAttempt, UpsertTask};
use herdr_board::herdr::{Herdr, PaneInfo};
use herdr_board::model::{AgentStatus, BoardState, Outcome, Source, UpstreamState};
use herdr_board::sources::linear::GraphQl;
use herdr_board::sync::SyncEngine;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// The checkout every attempt here is dispatched into. Never touched on disk —
/// only compared against a pane's `cwd`, which is how the board tells its own
/// agent from whoever might be in that pane now.
const WORKTREE: &str = "/wt/board-gh-34";

/// A pane mid-turn. The elapsed timer is what makes the next screen differ.
const WORKING: &str = "\
⏺ Update(src/sync.rs)
✳ Tinkering… (22m 04s · ↓ 129.0k tokens)
──────────────────────────────────────────
❯
──────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents
";

/// The same pane fourteen seconds later. Only the timer and the frame moved,
/// which is all a live turn needs to prove itself.
const WORKING_LATER: &str = "\
⏺ Update(src/sync.rs)
✽ Tinkering… (22m 18s · ↓ 131.2k tokens)
──────────────────────────────────────────
❯
──────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents
";

/// A pane nobody is using, which herdr nonetheless reports `working`: the
/// spinner line is left over from the turn that ended, sitting inside the
/// twenty-line detection region. This is the shape that would flap a finished
/// task back to `working` on every cycle, forever, if the status were trusted.
const STALE_SPINNER: &str = "\
✻ Crunched for 3m 19s
✳ Tinkering… (22m 04s · ↓ 129.0k tokens)
  ⎿  Tip: Use /clear to start fresh when switching topics
──────────────────────────────────────────
❯
──────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents
";

// ---- the stub herdr ------------------------------------------------------

/// Where the stub binary and its canned screens live. One per test *binary*,
/// because `HERDR_BIN_PATH` is process-global; tests stay independent by using
/// their own pane ids and task identifiers.
fn stub() -> &'static Path {
    static STUB: OnceLock<PathBuf> = OnceLock::new();
    STUB.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("herdr-board-stub34-{}", std::process::id()));
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
        // initialiser, and nothing reads the variable until a `Herdr` is
        // discovered, which happens after.
        unsafe { std::env::set_var("HERDR_BIN_PATH", &bin) };
        dir
    })
    .as_path()
}

fn herdr() -> Herdr {
    stub();
    Herdr::discover(Arc::new(herdr_board::log::Logger::new("", false)))
}

fn show(pane: &str, screen: &str) {
    std::fs::write(stub().join("screens").join(pane.replace(':', "_")), screen).unwrap();
}

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
        let dir = std::env::temp_dir()
            .join(format!("herdr-board-gh34-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths {
            config_dir: dir.clone(),
            state_dir: dir.clone(),
        };
        let engine = SyncEngine {
            db: Db::open(&paths.db()).unwrap(),
            cfg: toml::from_str(ROUTING).unwrap(),
            credentials: Default::default(),
            paths,
            log: Arc::new(herdr_board::log::Logger::new("", false)),
            linear: None::<herdr_board::sources::linear::Linear<Box<dyn GraphQl>>>,
            github: None,
        };
        Board { dir, engine }
    }

    /// A task the board has already closed as finished, its pull request open and
    /// its pane still there — the state gh#543 was left in.
    ///
    /// Closed by reconciliation rather than by writing the row, so the settle does
    /// everything the real one does, including forgetting the screen it recorded.
    /// A pull request is the one artifact that settles on the first sample that
    /// sees it, which makes it the cheapest honest way to get here.
    fn settled(&self, identifier: &str, pane: &str) -> Result<i64> {
        let attempt = self.dispatched(identifier, pane)?;
        self.open_pull_request(identifier)?;
        self.tick(&[idle_pane(pane)]);
        Ok(attempt)
    }

    fn open_pull_request(&self, identifier: &str) -> Result<()> {
        self.engine.db.set_pr(
            &format!("linear:{identifier}"),
            Some("https://github.com/Florin-AS/Tally/pull/544"),
            Some(544),
            true,
        )
    }

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
            worktree: Some(WORKTREE.into()),
            branch: Some(format!("board/{}", identifier.to_lowercase())),
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
        })?;
        self.engine.db.set_attempt_pane(attempt, pane)?;
        self.engine.db.set_saw_working(attempt)?;
        Ok(attempt)
    }

    /// Backdate the recorded screen sample, standing in for the seconds between
    /// two reconciles. `set_screen_sample` always stamps *now*, which is right in
    /// production and useless in a test that cannot wait.
    fn sample_taken_secs_ago(&self, attempt: i64, screen: &str, secs: i64) {
        let at = herdr_board::db::rfc3339(chrono::Utc::now() - chrono::Duration::seconds(secs));
        self.engine
            .db
            .conn
            .execute(
                "UPDATE attempts SET screen_print = ?2, screen_at = ?3 WHERE id = ?1",
                rusqlite::params![attempt, herdr_board::screen::fingerprint(screen), at],
            )
            .unwrap();
    }

    fn state(&self, identifier: &str) -> BoardState {
        self.engine
            .db
            .get_task(&format!("linear:{identifier}"))
            .unwrap()
            .unwrap()
            .state
    }

    fn attempt(&self, identifier: &str, id: i64) -> herdr_board::model::Attempt {
        self.engine
            .db
            .attempts_for(&format!("linear:{identifier}"))
            .unwrap()
            .into_iter()
            .find(|a| a.id == id)
            .expect("the attempt is still on the task")
    }

    /// One reconcile, with the handle — the daemon's own path.
    fn tick(&self, panes: &[PaneInfo]) {
        let h = herdr();
        self.engine.reconcile_with(panes, Some(&h)).unwrap();
        self.engine.rederive_all().unwrap();
    }
}

/// A pane herdr reports as `working`, sitting in the attempt's own checkout.
fn working_pane(id: &str) -> PaneInfo {
    PaneInfo {
        pane_id: id.into(),
        workspace_id: "w1".into(),
        tab_id: Some("w1:t1".into()),
        agent: Some("claude".into()),
        agent_status: Some(AgentStatus::Working),
        focused: false,
        label: None,
        cwd: Some(WORKTREE.into()),
        scroll_offset: 0,
    }
}

/// The same pane with its agent between turns.
fn idle_pane(id: &str) -> PaneInfo {
    PaneInfo {
        agent_status: Some(AgentStatus::Idle),
        ..working_pane(id)
    }
}

// ---- the tests -----------------------------------------------------------

/// The bug, end to end. The board closed the attempt; the agent carried on; the
/// screen proves it. The row must stop claiming the work is finished, and the
/// attempt must go back to being something reconciliation looks at.
#[test]
fn an_agent_still_working_after_the_board_closed_it_is_reopened() {
    let b = Board::new("reopened");
    let attempt = b.settled("OFF-201", "w1:p201").unwrap();
    assert_eq!(b.state("OFF-201"), BoardState::Review, "closed as finished");

    // A first look records the screen and concludes nothing — there is nothing
    // yet to compare against.
    show("w1:p201", WORKING);
    b.tick(&[working_pane("w1:p201")]);
    assert_eq!(
        b.attempt("OFF-201", attempt).outcome,
        Some(Outcome::Done),
        "one sample of a pane says nothing about whether it is moving"
    );

    // The pane moves. That is the claim: work is still happening in a checkout
    // the board said was done with.
    show("w1:p201", WORKING_LATER);
    b.tick(&[working_pane("w1:p201")]);

    let a = b.attempt("OFF-201", attempt);
    assert_eq!(a.outcome, None, "the attempt is live again");
    assert_eq!(a.ended_at, None, "and no longer claims to have ended");
    assert_eq!(a.reopened, 1, "with the board's mistake recorded on it");
    assert_eq!(
        b.state("OFF-201"),
        BoardState::Working,
        "and the row says so on the same cycle, not the next one"
    );

    // No second attempt: nobody dispatched anything, and a retry the board
    // invented would inflate the only number it has for work done twice.
    assert_eq!(
        b.engine
            .db
            .attempts_for("linear:OFF-201")
            .unwrap()
            .len(),
        1
    );
    // The operator was told it finished. They are told that was wrong.
    let toasts = argv_mentioning("OFF-201 is still working");
    assert_eq!(toasts.len(), 1, "{toasts:?}");
}

/// The false positive that would be worse than the bug. Claude Code leaves
/// spinner lines in scrollback, so herdr reports `working` for a pane nobody is
/// using — and re-opening on that would flap every finished task between
/// `review` and `working` for the rest of the session.
#[test]
fn a_finished_pane_that_only_looks_busy_stays_closed() {
    let b = Board::new("stale");
    let attempt = b.settled("OFF-202", "w1:p202").unwrap();
    show("w1:p202", STALE_SPINNER);
    // Recorded a while ago and unchanged since: frozen, which is exactly what a
    // finished pane looks like.
    b.sample_taken_secs_ago(attempt, STALE_SPINNER, 40);

    for _ in 0..3 {
        b.tick(&[working_pane("w1:p202")]);
    }

    let a = b.attempt("OFF-202", attempt);
    assert_eq!(a.outcome, Some(Outcome::Done));
    assert_eq!(a.reopened, 0);
    assert_eq!(b.state("OFF-202"), BoardState::Review);
    assert!(argv_mentioning("OFF-202 is still working").is_empty());
}

/// A settle clears the screen it recorded, so the first look afterwards is a
/// first look. Without that, the last screen from while the agent *was* working
/// would be the baseline, every closed attempt would read as having moved, and
/// every one of them would be re-opened on the next cycle.
#[test]
fn closing_an_attempt_forgets_the_screen_it_was_working_on() {
    let b = Board::new("baseline");
    let attempt = b.dispatched("OFF-203", "w1:p203").unwrap();
    show("w1:p203", WORKING);
    b.tick(&[working_pane("w1:p203")]);
    assert!(
        b.attempt("OFF-203", attempt).screen_print.is_some(),
        "a live attempt records its pane"
    );

    b.open_pull_request("OFF-203").unwrap();
    b.tick(&[idle_pane("w1:p203")]);

    let closed = b.attempt("OFF-203", attempt);
    assert_eq!(closed.outcome, Some(Outcome::Done));
    assert_eq!(closed.screen_print, None);
    assert_eq!(closed.screen_at, None);
}

/// herdr pane ids are not reused, but a pane can be handed to somebody else —
/// and a pane working in another checkout is not this attempt's agent. Same
/// check review delivery makes before typing into it.
#[test]
fn a_pane_that_moved_on_to_other_work_reopens_nothing() {
    let b = Board::new("elsewhere");
    let attempt = b.settled("OFF-204", "w1:p204").unwrap();
    show("w1:p204", WORKING);
    b.sample_taken_secs_ago(attempt, STALE_SPINNER, 40);

    let mut pane = working_pane("w1:p204");
    pane.cwd = Some("/wt/somebody-elses-branch".into());
    b.tick(&[pane]);

    assert_eq!(b.attempt("OFF-204", attempt).outcome, Some(Outcome::Done));
    // And nothing was even read: the pane was ruled out before the cost.
    assert!(argv_mentioning("pane read w1:p204").is_empty());
}

/// A pane whose agent has exited is a shell prompt. Nothing is working in it,
/// whatever herdr's last status said.
#[test]
fn a_pane_whose_agent_is_gone_reopens_nothing() {
    let b = Board::new("noagent");
    let attempt = b.settled("OFF-205", "w1:p205").unwrap();
    show("w1:p205", WORKING_LATER);
    b.sample_taken_secs_ago(attempt, WORKING, 40);

    let mut pane = working_pane("w1:p205");
    pane.agent = None;
    b.tick(&[pane]);

    assert_eq!(b.attempt("OFF-205", attempt).outcome, Some(Outcome::Done));
}

/// Somebody re-dispatched the task. That attempt is the live one — the board
/// allows exactly one — and the older row stays closed.
#[test]
fn a_re_dispatched_task_keeps_its_new_attempt() {
    let b = Board::new("redispatched");
    let first = b.settled("OFF-206", "w1:p206").unwrap();
    let retry = b.dispatched("OFF-206", "w1:p216").unwrap();
    show("w1:p206", WORKING_LATER);
    b.sample_taken_secs_ago(first, WORKING, 40);

    b.tick(&[working_pane("w1:p206"), working_pane("w1:p216")]);

    assert_eq!(b.attempt("OFF-206", first).outcome, Some(Outcome::Done));
    assert_eq!(b.attempt("OFF-206", first).reopened, 0);
    assert_eq!(b.attempt("OFF-206", retry).outcome, None);
}

/// The issue was closed. Somebody decided this task is over, and an agent still
/// typing does not overrule them — the work is moot, which is the same rule
/// `derive_state` applies to a live attempt on a terminal issue.
#[test]
fn a_closed_issue_is_not_reopened_by_an_agent_that_kept_going() {
    let b = Board::new("terminal");
    let attempt = b.settled("OFF-207", "w1:p207").unwrap();
    b.engine
        .db
        .conn
        .execute(
            "UPDATE tasks SET upstream = 'terminal' WHERE id = 'linear:OFF-207'",
            [],
        )
        .unwrap();
    show("w1:p207", WORKING_LATER);
    b.sample_taken_secs_ago(attempt, WORKING, 40);

    b.tick(&[working_pane("w1:p207")]);

    assert_eq!(b.attempt("OFF-207", attempt).outcome, Some(Outcome::Done));
    assert_eq!(b.state("OFF-207"), BoardState::Done);
}

/// The cost gate. A closed attempt whose pane herdr calls anything but `working`
/// is not read at all — otherwise every task the board has ever finished would
/// cost a pane read on every cycle.
#[test]
fn an_idle_pane_costs_no_screen_read() {
    let b = Board::new("idlepane");
    let attempt = b.settled("OFF-208", "w1:p208").unwrap();
    show("w1:p208", WORKING_LATER);
    b.sample_taken_secs_ago(attempt, WORKING, 40);

    let mut pane = working_pane("w1:p208");
    pane.agent_status = Some(AgentStatus::Idle);
    b.tick(&[pane]);

    assert_eq!(b.attempt("OFF-208", attempt).outcome, Some(Outcome::Done));
    assert!(argv_mentioning("pane read w1:p208").is_empty());
}

/// Once re-opened, the attempt is an ordinary live one again: the settle path
/// applies to it exactly as it did the first time, and the record that the board
/// got it wrong survives the round trip. Nothing here is a one-way door.
#[test]
fn a_reopened_attempt_settles_again_and_keeps_the_record() {
    let b = Board::new("resettle");
    let attempt = b.settled("OFF-209", "w1:p209").unwrap();
    show("w1:p209", WORKING);
    b.tick(&[working_pane("w1:p209")]);
    show("w1:p209", WORKING_LATER);
    b.tick(&[working_pane("w1:p209")]);
    assert_eq!(b.attempt("OFF-209", attempt).outcome, None, "re-opened");

    b.tick(&[idle_pane("w1:p209")]);
    let a = b.attempt("OFF-209", attempt);
    assert_eq!(
        a.outcome,
        Some(Outcome::Done),
        "its pull request settles it again"
    );
    assert_eq!(a.reopened, 1, "and the board's mistake is still on the record");
    assert_eq!(b.state("OFF-209"), BoardState::Review);
    assert_eq!(
        b.engine.db.attempts_for("linear:OFF-209").unwrap().len(),
        1,
        "one attempt throughout, whatever the board thought of it"
    );
}
