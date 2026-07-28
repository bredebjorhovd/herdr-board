//! Integration test: drive a full `sync --once` cycle against recorded Linear
//! responses, with no network and no running herdr.
//!
//! The transport double lives here rather than in the crate so the shipped
//! binary carries no test scaffolding.

use anyhow::Result;
use herdr_board::config::{Paths, RoutingConfig};
use herdr_board::db::Db;
use herdr_board::herdr::PaneInfo;
use herdr_board::model::{AgentStatus, BoardState, Outcome, Source, UpstreamState};
use herdr_board::sources::linear::{GraphQl, Linear};
use herdr_board::sync::{SourceHealth, SyncEngine, meta};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;

/// Replays recorded responses in order, recording what was asked.
struct Recorded {
    responses: RefCell<VecDeque<Value>>,
    sent: RefCell<Vec<Value>>,
}

impl Recorded {
    fn new(files: &[&str]) -> Recorded {
        let responses = files
            .iter()
            .map(|f| {
                let path = format!("{}/tests/fixtures/{f}", env!("CARGO_MANIFEST_DIR"));
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("reading {path}: {e}"));
                serde_json::from_str(&text).expect("fixture is valid JSON")
            })
            .collect::<VecDeque<Value>>();
        Recorded {
            responses: RefCell::new(responses),
            sent: RefCell::new(Vec::new()),
        }
    }
}

impl GraphQl for Recorded {
    fn query(&self, body: &Value) -> Result<Value> {
        self.sent.borrow_mut().push(body.clone());
        self.responses
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("ran out of recorded responses"))
    }
}

/// Shares one [`Recorded`] between the engine, which owns its transport, and the
/// test, which needs to read back what was sent.
struct Shared(std::rc::Rc<Recorded>);

impl GraphQl for Shared {
    fn query(&self, body: &Value) -> Result<Value> {
        self.0.query(body)
    }
}

/// A transport that always fails, for the outage test.
struct Offline;
impl GraphQl for Offline {
    fn query(&self, _: &Value) -> Result<Value> {
        anyhow::bail!("connection refused")
    }
}

const ROUTING: &str = r#"
[sync]
interval = "30s"
labels = ["herd"]

[[route]]
match = { linear_team = "OFF" }
workspace = "offhand"
repo = "/tmp/offhand"
runtime = "claude-code"
prompt = "Work on {identifier}: {title}"

[defaults]
max_concurrent_per_workspace = 2
branch_template = "board/{identifier_lower}"
"#;

struct Harness {
    dir: std::path::PathBuf,
}

impl Harness {
    fn new(name: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!(
            "herdr-board-it-{}-{name}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Harness { dir }
    }

    fn engine(&self, transport: Box<dyn GraphQl>) -> SyncEngine {
        let paths = Paths {
            config_dir: self.dir.clone(),
            state_dir: self.dir.clone(),
        };
        let cfg: RoutingConfig = toml::from_str(ROUTING).unwrap();
        SyncEngine {
            db: Db::open(&paths.db()).unwrap(),
            cfg,
            credentials: Default::default(),
            paths,
            log: Arc::new(herdr_board::log::Logger::new("", false)),
            linear: Some(Linear::new(transport)),
            github: None,
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn pane(id: &str, status: AgentStatus) -> PaneInfo {
    PaneInfo {
        pane_id: id.into(),
        workspace_id: "w1".into(),
        tab_id: Some("w1:t2".into()),
        agent: Some("claude".into()),
        agent_status: Some(status),
        focused: false,
        label: None,
        cwd: None,
    }
}

#[test]
fn a_sync_cycle_ingests_recorded_issues_and_derives_their_states() {
    let h = Harness::new("ingest");
    let e = h.engine(Box::new(Recorded::new(&[
        "linear_board_page1.json",
        "linear_board_page2.json",
        // The follow-up query for issues with open attempts (none yet).
        "linear_empty.json",
    ])));

    // No herdr: reconciliation is skipped, everything else runs.
    e.sync_once(None).unwrap();

    let tasks = e.db.load_tasks().unwrap();
    assert_eq!(tasks.len(), 5, "both recorded pages were ingested");

    let by = |id: &str| tasks.iter().find(|t| t.id == id).unwrap().clone();

    // Unstarted, nothing running → ready.
    let t = by("linear:OFF-145");
    assert_eq!(t.state, BoardState::Ready);
    assert_eq!(t.source, Source::Linear);
    assert_eq!(t.identifier, "OFF-145");
    assert_eq!(t.labels, vec!["herd"]);
    assert_eq!(t.linear_team.as_deref(), Some("OFF"));
    assert_eq!(t.linear_project.as_deref(), Some("Compliance"));

    // `started` upstream with no live attempt and no PR is `ready`, not
    // `working` — the design's cancelled-task resolution.
    assert_eq!(by("linear:OFF-138").state, BoardState::Ready);
    assert_eq!(by("linear:OFF-138").upstream, UpstreamState::Started);

    // An attached pull request reaches `review` even with nothing running.
    let review = by("linear:OFF-129");
    assert_eq!(review.state, BoardState::Review);
    assert_eq!(
        review.pr_url.as_deref(),
        Some("https://github.com/offhand/tally/pull/291")
    );
    assert_eq!(review.pr_number, Some(291));

    // A completed workflow state is `done`, by type not by name.
    assert_eq!(by("linear:OFF-118").state, BoardState::Done);
    assert_eq!(by("linear:OFF-118").upstream, UpstreamState::Terminal);

    // Ingested but unroutable — it is on the board, just not dispatchable.
    let unrouted = by("linear:TAL-151");
    assert_eq!(unrouted.state, BoardState::Ready);
    assert!(
        e.cfg
            .resolve(&herdr_board::sync::route_context(&unrouted))
            .is_none(),
        "TAL-151 must not match the OFF route"
    );
}

#[test]
fn the_cycle_records_health_and_a_watermark_for_the_next_poll() {
    let h = Harness::new("watermark");
    let e = h.engine(Box::new(Recorded::new(&[
        "linear_board_page1.json",
        "linear_board_page2.json",
        "linear_empty.json",
    ])));
    e.sync_once(None).unwrap();

    assert_eq!(e.health(Source::Linear), SourceHealth::Ok);
    // GitHub was never configured, so the header omits it rather than
    // reporting it down.
    assert_eq!(e.health(Source::Github), SourceHealth::Absent);

    // The watermark is the newest updatedAt seen, so the next poll is cheap.
    assert_eq!(
        e.db.meta_get(meta::LINEAR_WATERMARK).unwrap().as_deref(),
        Some("2026-07-25T18:04:11.000Z")
    );
    assert!(e.db.meta_get(meta::LAST_SYNC).unwrap().is_some());
}

#[test]
fn a_second_cycle_asks_only_for_what_changed() {
    let h = Harness::new("incremental");
    let e = h.engine(Box::new(Recorded::new(&[
        "linear_board_page1.json",
        "linear_board_page2.json",
        "linear_empty.json",
        // Second cycle: nothing new.
        "linear_empty.json",
        "linear_empty.json",
    ])));
    e.sync_once(None).unwrap();
    e.sync_once(None).unwrap();

    // Rows survive a poll that returned nothing — the board is not rebuilt from
    // each response.
    assert_eq!(e.db.load_tasks().unwrap().len(), 5);
}

#[test]
fn an_outage_serves_stale_rows_and_marks_the_header() {
    let h = Harness::new("outage");

    // First cycle populates the board.
    {
        let e = h.engine(Box::new(Recorded::new(&[
            "linear_board_page1.json",
            "linear_board_page2.json",
            "linear_empty.json",
        ])));
        e.sync_once(None).unwrap();
        assert_eq!(e.db.load_tasks().unwrap().len(), 5);
    }

    // Second cycle, with Linear unreachable.
    let e = h.engine(Box::new(Offline));
    e.sync_once(None).unwrap();

    assert_eq!(
        e.db.load_tasks().unwrap().len(),
        5,
        "never blank the list because a poll failed"
    );
    match e.health(Source::Linear) {
        SourceHealth::Down { error, retry_in } => {
            assert!(error.contains("connection refused"), "{error}");
            assert!((1..=300).contains(&retry_in));
        }
        other => panic!("expected Down, got {other:?}"),
    }
}

#[test]
fn a_full_lifecycle_runs_through_the_cycle() {
    // dispatch → working → blocked → PR → review, as the daemon would see it.
    let h = Harness::new("lifecycle");
    let e = h.engine(Box::new(Recorded::new(&[
        "linear_board_page1.json",
        "linear_board_page2.json",
        "linear_empty.json",
    ])));
    e.sync_once(None).unwrap();

    let task = e.db.get_task("linear:OFF-145").unwrap().unwrap();
    let attempt = e
        .db
        .insert_attempt(&herdr_board::db::NewAttempt {
            task_id: task.id.clone(),
            pane_id: None,
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: Some("board/off-145".into()),
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
        })
        .unwrap();
    e.db.set_attempt_pane(attempt, "w1:p7").unwrap();

    // Working.
    e.reconcile(&[pane("w1:p7", AgentStatus::Working)]).unwrap();
    e.rederive_all().unwrap();
    assert_eq!(
        e.db.get_task("linear:OFF-145").unwrap().unwrap().state,
        BoardState::Working
    );

    // Blocked on an approval prompt.
    e.reconcile(&[pane("w1:p7", AgentStatus::Blocked)]).unwrap();
    e.rederive_all().unwrap();
    assert_eq!(
        e.db.get_task("linear:OFF-145").unwrap().unwrap().state,
        BoardState::Blocked
    );

    // Settled with no PR: still working, because a settled agent may simply be
    // between turns.
    e.reconcile(&[pane("w1:p7", AgentStatus::Idle)]).unwrap();
    e.rederive_all().unwrap();
    assert_eq!(
        e.db.get_task("linear:OFF-145").unwrap().unwrap().state,
        BoardState::Working
    );

    // A PR lands, and the next settled tick finalizes the attempt.
    e.db.set_pr(
        "linear:OFF-145",
        Some("https://github.com/offhand/tally/pull/292"),
        Some(292),
        true,
    )
    .unwrap();
    e.reconcile(&[pane("w1:p7", AgentStatus::Done)]).unwrap();
    e.rederive_all().unwrap();

    let done = e.db.get_task("linear:OFF-145").unwrap().unwrap();
    assert_eq!(done.state, BoardState::Review);
    assert_eq!(done.attempts[0].outcome, Some(Outcome::Done));

    // The Linear trail is queued exactly once.
    assert_eq!(e.db.pending_writebacks(10).unwrap().len(), 1);
}

/// AGE-21: a ticket whose work is finished and waiting on a human said
/// "In Progress" in Linear for the whole review window, because dispatch was the
/// last thing that ever moved it.
#[test]
fn work_that_is_waiting_on_a_human_is_moved_out_of_in_progress() {
    let h = Harness::new("review-state");
    let recorded = std::rc::Rc::new(Recorded::new(&[
        "linear_board_page1.json",
        "linear_board_page2.json",
        // Resolving `In Review`, then moving the issue.
        "linear_team_states.json",
        "linear_issue_update_ok.json",
    ]));
    let mut e = h.engine(Box::new(Shared(recorded.clone())));
    e.cfg.linear.review_state = Some("In Review".into());

    e.sync_once(None).unwrap();

    // OFF-129 is in review on the board *and* already In Review upstream, so
    // there is nothing to say about it.
    assert_eq!(
        e.db.get_task("linear:OFF-129").unwrap().unwrap().state,
        BoardState::Review
    );
    assert_eq!(
        e.db.pending_writebacks(10).unwrap().len(),
        0,
        "a ticket already in the review state must not be written to"
    );

    // OFF-138 is the case that was broken: In Progress upstream, and its pull
    // request has just landed.
    e.db.set_pr(
        "linear:OFF-138",
        Some("https://github.com/offhand/tally/pull/293"),
        Some(293),
        true,
    )
    .unwrap();
    e.rederive_all().unwrap();
    assert_eq!(
        e.db.get_task("linear:OFF-138").unwrap().unwrap().state,
        BoardState::Review
    );

    e.drain_writebacks();
    assert!(e.db.pending_writebacks(10).unwrap().is_empty(), "delivered");

    let sent = recorded.sent.borrow();
    let update = sent.last().expect("a mutation was sent");
    assert_eq!(
        update["variables"]["id"],
        Value::from("b1d4c9e0-0000-4000-8000-000000000138")
    );
    // The configured state — not the lowest-position `started` one, which is
    // In Progress and is where dispatch already put it.
    assert_eq!(update["variables"]["stateId"], Value::from("state-review"));
}

#[test]
fn an_orphaned_pane_needs_two_ticks_and_then_reads_as_failed() {
    let h = Harness::new("orphan");
    let e = h.engine(Box::new(Recorded::new(&[
        "linear_board_page1.json",
        "linear_board_page2.json",
        "linear_empty.json",
    ])));
    e.sync_once(None).unwrap();

    let a = e
        .db
        .insert_attempt(&herdr_board::db::NewAttempt {
            task_id: "linear:OFF-145".into(),
            pane_id: None,
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: Some("board/off-145".into()),
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
        })
        .unwrap();
    e.db.set_attempt_pane(a, "w1:p7").unwrap();

    // One tick during a live handoff must not flap the row into failed.
    e.reconcile(&[]).unwrap();
    e.rederive_all().unwrap();
    assert_ne!(
        e.db.get_task("linear:OFF-145").unwrap().unwrap().state,
        BoardState::Failed
    );

    e.reconcile(&[]).unwrap();
    e.rederive_all().unwrap();
    assert_eq!(
        e.db.get_task("linear:OFF-145").unwrap().unwrap().state,
        BoardState::Failed
    );
}

#[test]
fn a_cancelled_attempt_returns_the_row_to_ready() {
    // Design-spec gap #5, end to end: cancelling ends the attempt, not the
    // issue, and the row keeps its history.
    let h = Harness::new("cancel");
    let e = h.engine(Box::new(Recorded::new(&[
        "linear_board_page1.json",
        "linear_board_page2.json",
        "linear_empty.json",
    ])));
    e.sync_once(None).unwrap();

    let a = e
        .db
        .insert_attempt(&herdr_board::db::NewAttempt {
            task_id: "linear:OFF-138".into(),
            pane_id: None,
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: Some("board/off-138".into()),
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
        })
        .unwrap();
    e.db.close_attempt(a, Outcome::Cancelled).unwrap();
    e.rederive_all().unwrap();

    let t = e.db.get_task("linear:OFF-138").unwrap().unwrap();
    assert_eq!(t.state, BoardState::Ready);
    assert_eq!(t.upstream, UpstreamState::Started);
    assert_eq!(t.attempts.len(), 1, "the attempt history is intact");
    assert_eq!(t.attempts[0].outcome, Some(Outcome::Cancelled));
}
