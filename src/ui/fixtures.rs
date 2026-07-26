//! Demo fixtures — every state the board can reach, without a network, a
//! database, or a running herdr. `herdr-board demo` renders these, and the
//! render tests assert against them.

use super::state::*;
use crate::model::*;
use crate::sync::SourceHealth;

pub const POPULATED: &str = "populated";
pub const EMPTY: &str = "empty";
pub const LINEAR_DOWN: &str = "linear-down";
pub const SYNCD_DEAD: &str = "syncd-dead";
pub const STALE_BINDING: &str = "stale-binding";

/// Every scenario, for `demo --list` and for the render tests.
pub const ALL: &[&str] = &[POPULATED, EMPTY, LINEAR_DOWN, SYNCD_DEAD, STALE_BINDING];

const CONFIG_PATH: &str = "~/.config/herdr/plugins/config/board/routing.toml";

fn task(identifier: &str, title: &str, state: BoardState) -> Task {
    Task {
        id: format!("linear:{identifier}"),
        source: Source::Linear,
        source_id: format!("uuid-{identifier}"),
        identifier: identifier.into(),
        title: title.into(),
        body: Some(
            "The poller gives up on the first 502 from Altinn and never retries, \
             so an overnight run loses the whole batch."
                .into(),
        ),
        url: format!("https://linear.app/offhand/issue/{identifier}"),
        labels: vec!["herd".into()],
        state,
        source_state: None,
        linear_team: Some("LIN".into()),
        linear_project: Some("Compliance".into()),
        upstream: UpstreamState::Unstarted,
        local_done: false,
        pr_url: None,
        pr_number: None,
        pr_open: false,
        pr_merged: false,
        pr_mergeable: None,
        updated_at: crate::db::now(),
        synced_at: crate::db::now(),
        attempts: vec![],
    }
}

fn attempt(id: i64, runtime: &str, outcome: Option<Outcome>, age_secs: i64) -> Attempt {
    attempt_by(id, runtime, outcome, age_secs, None)
}

fn attempt_by(
    id: i64,
    runtime: &str,
    outcome: Option<Outcome>,
    age_secs: i64,
    dispatched_by: Option<&str>,
) -> Attempt {
    let started = chrono::Utc::now() - chrono::Duration::seconds(age_secs);
    Attempt {
        id,
        task_id: String::new(),
        pane_id: Some("w3:p2".into()),
        workspace: "offhand".into(),
        runtime: runtime.into(),
        worktree: Some("/state/wt/lin-138-1".into()),
        branch: Some("board/lin-138".into()),
        started_at: crate::db::rfc3339(started),
        ended_at: outcome.map(|_| crate::db::now()),
        outcome,
        missing_ticks: 0,
        agent_status: None,
        dispatched_by: dispatched_by.map(str::to_string),
    }
}

fn view(task: Task, elapsed: Option<i64>) -> TaskView {
    TaskView {
        has_route: true,
        route_name: Some("offhand".into()),
        // Take the runtime from the attempt when there is one, as build_views
        // does — an agent that dispatched with a different runtime must show it.
        runtime: Some(
            task.attempts
                .last()
                .map(|a| a.runtime.clone())
                .unwrap_or_else(|| "claude-code".into()),
        ),
        workspace: Some("offhand".into()),
        elapsed_secs: elapsed,
        idle: false,
        pane_id: Some("w3:p2".into()),
        branch: Some(format!("board/{}", task.identifier.to_lowercase())),
        dispatched_by: task
            .attempts
            .last()
            .and_then(|a| a.dispatched_by.clone()),
        resolved_prompt: Some(format!(
            "You are working on: {} ({})\n\n{}\n\nWork in this worktree. \
             Open a PR when done; branch is prepared.",
            task.title,
            task.identifier,
            task.body.clone().unwrap_or_default()
        )),
        task,
    }
}

fn populated_views() -> Vec<TaskView> {
    let mut out = Vec::new();

    // blocked — holds a pane, so it counts against the concurrency cap.
    let mut t = task("LIN-131", "Signicat callback drops the state param", BoardState::Blocked);
    t.attempts = vec![attempt(1, "claude-code", None, 412)];
    out.push(view(t, Some(412)));

    // working, ticking
    let mut t = task("LIN-138", "Rewrite the Tripletex sync cursor", BoardState::Working);
    t.attempts = vec![attempt(2, "claude-code", None, 544)];
    out.push(view(t, Some(544)));

    // working, but the agent has settled between turns — still `working`.
    let mut t = task("LIN-140", "Backfill missing orgnr on legacy clients", BoardState::Working);
    t.attempts = vec![attempt(3, "claude-code", None, 666)];
    let mut v = view(t, Some(666));
    v.idle = true;
    out.push(v);

    // working, released by another agent rather than by the operator. This is a
    // primary path, so the fixture treats it as ordinary.
    let mut t = task("LIN-152", "Extract the BRREG client into a package", BoardState::Working);
    t.attempts = vec![attempt_by(6, "codex", None, 128, Some("LIN-138"))];
    out.push(view(t, Some(128)));

    // ready, routed
    out.push(view(
        task("LIN-145", "Add retry to Altinn poller", BoardState::Ready),
        None,
    ));
    out.push(view(
        task("LIN-146", "Cache BRREG lookups for the client picker", BoardState::Ready),
        None,
    ));

    // ready, but nothing routes it — a property of the issue, shown on every
    // such row.
    let mut v = view(
        task("LIN-151", "Tidy the changelog script", BoardState::Ready),
        None,
    );
    v.has_route = false;
    v.route_name = None;
    v.runtime = None;
    v.workspace = None;
    v.resolved_prompt = None;
    out.push(v);

    // review — an open PR waiting on the operator.
    let mut t = task("LIN-129", "Split the MVA report by term", BoardState::Review);
    t.pr_url = Some("https://github.com/offhand/tally/pull/291".into());
    t.pr_number = Some(291);
    t.pr_open = true;
    t.attempts = vec![attempt(4, "claude-code", Some(Outcome::Done), 5400)];
    out.push(view(t, None));

    // failed — the pane vanished.
    let mut t = task("LIN-122", "Migrate the Maskinporten client id", BoardState::Failed);
    t.attempts = vec![attempt(5, "claude-code", Some(Outcome::Orphaned), 900)];
    out.push(view(t, None));

    // done — collapsed to one dim line.
    out.push(view(
        task("LIN-118", "Bump rusqlite to 0.40", BoardState::Done),
        None,
    ));

    out
}

fn sync_ok() -> SyncStatus {
    SyncStatus {
        linear: SourceHealth::Ok,
        github: SourceHealth::Ok,
        last_cycle_secs: Some(12),
        last_source_ok_secs: Some(12),
        interval_secs: 30,
    }
}

pub fn app(scenario: &str) -> App {
    let (views, sync) = match scenario {
        EMPTY => (Vec::new(), sync_ok()),
        LINEAR_DOWN => (
            populated_views(),
            SyncStatus {
                linear: SourceHealth::Down {
                    error: "connection refused".into(),
                    retry_in: 30,
                },
                // The daemon is still cycling; only Linear has gone stale.
                last_source_ok_secs: Some(240),
                ..sync_ok()
            },
        ),
        SYNCD_DEAD => (
            populated_views(),
            SyncStatus {
                last_cycle_secs: Some(240),
                ..sync_ok()
            },
        ),
        STALE_BINDING => {
            // The pane is gone but the row still claimed to be working: it
            // belongs in FAILED, styled ✕, stating the fact.
            let mut views = populated_views();
            let mut t = task("LIN-138", "Rewrite the Tripletex sync cursor", BoardState::Failed);
            t.attempts = vec![attempt(2, "claude-code", Some(Outcome::Orphaned), 900)];
            views.retain(|v| v.task.identifier != "LIN-138");
            views.push(view(t, None));
            (views, sync_ok())
        }
        _ => (populated_views(), sync_ok()),
    };
    App::new(views, sync, CONFIG_PATH.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_populated_fixture_reaches_every_state() {
        let a = app(POPULATED);
        for state in BoardState::SECTION_ORDER {
            assert!(
                a.views.iter().any(|v| v.state() == state),
                "no fixture row is {state}"
            );
        }
    }

    #[test]
    fn the_populated_fixture_has_the_awkward_rows() {
        let a = app(POPULATED);
        assert!(a.views.iter().any(|v| !v.has_route), "no `no route` row");
        assert!(a.views.iter().any(|v| v.idle), "no idle working row");
        assert!(
            a.views.iter().any(|v| v.task.pr_number.is_some()),
            "no row with a PR"
        );
    }

    #[test]
    fn every_scenario_builds() {
        for s in ALL {
            let a = app(s);
            assert_eq!(a.screen, Screen::List);
            if *s != EMPTY {
                assert!(!a.views.is_empty(), "{s} has no rows");
            }
        }
    }

    #[test]
    fn the_syncd_dead_scenario_reads_as_a_dead_daemon_not_an_outage() {
        assert!(app(SYNCD_DEAD).sync.syncd_dead());
        assert_eq!(app(SYNCD_DEAD).sync.linear, SourceHealth::Ok);
    }

    #[test]
    fn the_stale_binding_scenario_puts_the_row_in_failed() {
        let a = app(STALE_BINDING);
        let v = a
            .views
            .iter()
            .find(|v| v.task.identifier == "LIN-138")
            .unwrap();
        assert_eq!(v.state(), BoardState::Failed);
    }
}
