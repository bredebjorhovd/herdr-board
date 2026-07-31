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
/// A board with nothing on it *because* nobody adopted the repo — the state
/// AGE-18 exists for, and the one where the empty copy and the UNADOPTED
/// section have to share a body.
pub const UNADOPTED: &str = "unadopted";
/// The board at the size that stopped being scannable: 109 rows that are not
/// done, across five routes, 83 of them from the one repo whose roadmap lives
/// as open issues.
///
/// `f` and `/` exist for this board and not for the ten-row one, so reviewing
/// them against `populated` would be reviewing them where they are not needed.
pub const CROWDED: &str = "crowded";

/// Every scenario, for `demo --list` and for the render tests.
pub const ALL: &[&str] = &[
    POPULATED,
    EMPTY,
    LINEAR_DOWN,
    SYNCD_DEAD,
    STALE_BINDING,
    UNADOPTED,
    CROWDED,
];

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
    attempt_by(id, runtime, outcome, age_secs, None, None)
}

fn attempt_by(
    id: i64,
    runtime: &str,
    outcome: Option<Outcome>,
    age_secs: i64,
    dispatched_by: Option<&str>,
    dispatched_by_pane: Option<&str>,
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
        settled_at: None,
        reopened: 0,
        agent_status: None,
        dispatched_by: dispatched_by.map(str::to_string),
        dispatched_by_pane: dispatched_by_pane.map(str::to_string),
        base_sha: Some("0000000000000000000000000000000000000000".into()),
        // Fixtures depict agents that have been running, not just-started ones.
        saw_working: true,
        // Nothing renders these; they are reconciliation's own bookkeeping.
        screen_print: None,
        screen_at: None,
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
        repo: None,
        // Fixtures already hold identifiers rather than task ids, so this is
        // the same fallback `build_views` makes: the parent's identifier when
        // the board dispatched it, the pane it is running in otherwise.
        dispatched_by: task
            .attempts
            .last()
            .and_then(|a| a.dispatched_by.clone().or_else(|| a.dispatched_by_pane.clone())),
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

    // working, released by another agent the board itself dispatched — so the
    // row can name the parent issue. This is a primary path, so the fixture
    // treats it as ordinary.
    let mut t = task("LIN-152", "Extract the BRREG client into a package", BoardState::Working);
    t.attempts = vec![attempt_by(6, "codex", None, 128, Some("LIN-138"), Some("w2:p1"))];
    out.push(view(t, Some(128)));

    // working, released by an orchestrator — a long-lived pane the operator
    // started, which owns no attempt and so has no issue to be named by. This
    // is the *common* shape of agent dispatch, and the one that used to be
    // recorded as the operator's (AGE-24).
    let mut t = task("LIN-153", "Split the Altinn poller retry test", BoardState::Working);
    t.attempts = vec![attempt_by(7, "claude-code", None, 96, None, Some("w1:p3"))];
    out.push(view(t, Some(96)));

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

/// A GitHub issue row. `gh#507` says nothing about which repo it came from, so
/// the view carries the repo the way [`build_views`](super::state::build_views)
/// derives it.
fn gh_task(repo: &str, number: u32, title: &str, state: BoardState) -> Task {
    Task {
        id: format!("gh:{repo}#{number}"),
        source: Source::Github,
        source_id: number.to_string(),
        identifier: format!("gh#{number}"),
        title: title.into(),
        body: None,
        url: format!("https://github.com/{repo}/issues/{number}"),
        labels: vec!["release-a".into()],
        state,
        source_state: None,
        linear_team: None,
        linear_project: None,
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

/// The same task, routed somewhere other than the one workspace `view` assumes.
fn routed(task: Task, route: &str, elapsed: Option<i64>) -> TaskView {
    let repo = crate::model::gh_repo_name(&task.id).map(str::to_string);
    TaskView {
        route_name: Some(route.into()),
        workspace: Some(route.into()),
        repo,
        ..view(task, elapsed)
    }
}

/// Polled, on the board, and deliberately undispatchable.
///
/// The route names the repo *and* a label, so the issues without that label
/// are still polled and still shown — with no route, which is the answer and
/// not an oversight.
fn unrouted(task: Task) -> TaskView {
    let repo = crate::model::gh_repo_name(&task.id).map(str::to_string);
    TaskView {
        has_route: false,
        route_name: None,
        runtime: None,
        workspace: None,
        // Nothing resolves a branch or a prompt for a row nothing routes.
        branch: None,
        resolved_prompt: None,
        repo,
        ..view(task, None)
    }
}

/// Enough distinct titles to fill a repo's whole open backlog without any two
/// rows reading alike — 83 identical lines would be a fixture nobody believes.
const CROWD_VERBS: &[&str] = &[
    "Retry", "Cache", "Split", "Backfill", "Rewrite", "Tidy", "Guard", "Bound",
    "Log", "Migrate",
];
const CROWD_SUBJECTS: &[&str] = &[
    "the Altinn receipt poller",
    "the BRREG orgnr lookup",
    "the MVA term split",
    "the Maskinporten token refresh",
    "the Tripletex page size",
    "the Signicat callback state",
    "the client picker query",
    "the nightly reconciliation job",
    "the ledger export writer",
    "the duplicate-invoice guard",
    "the SAF-T column mapping",
    "the bank statement importer",
    "the depreciation schedule",
];

/// Distinct for every `i` below `verbs × subjects`, which is what the callers
/// below stay inside by taking disjoint ranges.
fn crowd_title(i: usize) -> String {
    format!(
        "{} {}",
        CROWD_VERBS[i % CROWD_VERBS.len()],
        CROWD_SUBJECTS[(i / CROWD_VERBS.len()) % CROWD_SUBJECTS.len()]
    )
}

/// 129 rows that are not done, across five routes and no route at all.
///
/// The shape AGE-27 is about: one repo contributing most of them because
/// `[github] labels = []` polls every open issue, and four other routes whose
/// handful of rows are the ones you actually came to look at. That repo's
/// route then claims only the labelled ones, so 20 of its rows sit on the
/// board with no route — the group gh#39 gives `f` a position for.
fn crowded_views() -> Vec<TaskView> {
    // The ordinary board, all on one route, still on it underneath the flood.
    let mut out = populated_views();

    // The repo that arrived in a single poll.
    for i in 0..83 {
        let state = match i {
            0..=2 => BoardState::Working,
            3..=4 => BoardState::Review,
            5 => BoardState::Blocked,
            _ => BoardState::Ready,
        };
        let mut t = gh_task("Florin-AS/itsm-agent", 400 + i as u32, &crowd_title(i), state);
        if state == BoardState::Working || state == BoardState::Blocked {
            t.attempts = vec![attempt(100 + i as i64, "codex", None, 60 + i as i64 * 37)];
        }
        if state == BoardState::Review {
            t.pr_url = Some(format!("https://github.com/Florin-AS/itsm-agent/pull/{}", 400 + i));
            t.pr_number = Some(400 + i as i64);
            t.pr_open = true;
        }
        let elapsed = t.attempts.first().map(|_| 60 + i as i64 * 37);
        out.push(routed(t, "itsm-agent", elapsed));
    }

    // The same repo's other backlog: polled, on the board, and routed by
    // nothing, because the route ANDs the repo with a label these issues do
    // not carry. Not an oversight and not a handful — the group `f` had no
    // position for.
    for i in 0..20 {
        out.push(unrouted(gh_task(
            "Florin-AS/itsm-agent",
            800 + i as u32,
            &crowd_title(103 + i),
            BoardState::Ready,
        )));
    }

    // Three repos with the backlog a curated tracker actually has. Their title
    // ranges start past the flood's, so no two rows on the board read alike.
    for (route, repo, n, first, titles) in [
        ("tally", "Florin-AS/tally", 9usize, 500u32, 83usize),
        ("altinn-forms", "Florin-AS/altinn-forms", 5, 600, 92),
        ("brreg-client", "Florin-AS/brreg-client", 2, 700, 97),
    ] {
        for i in 0..n {
            let state = if i == 0 { BoardState::Working } else { BoardState::Ready };
            let mut t = gh_task(repo, first + i as u32, &crowd_title(titles + i), state);
            if state == BoardState::Working {
                t.attempts = vec![attempt(200 + i as i64, "claude-code", None, 240)];
            }
            let elapsed = t.attempts.first().map(|_| 240);
            out.push(routed(t, route, elapsed));
        }
    }

    // History, which the filter has to scope like everything else.
    for i in 0..4 {
        out.push(routed(
            gh_task("Florin-AS/itsm-agent", 300 + i, &crowd_title(99 + i as usize), BoardState::Done),
            "itsm-agent",
            None,
        ));
    }
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

/// Repos with a herdr workspace and no board config, in all three shapes:
/// nothing written at all, and each half of the half-fix.
fn unadopted_repos() -> Vec<crate::adopt::Unadopted> {
    use crate::adopt::{Missing, Unadopted};
    vec![
        Unadopted {
            label: "tripletex-mcp".into(),
            slug: "Florin-AS/tripletex-mcp".into(),
            repo_root: "/Users/b/code/tripletex-mcp".into(),
            missing: Missing::Both,
        },
        Unadopted {
            label: "brreg-client".into(),
            slug: "Florin-AS/brreg-client".into(),
            repo_root: "/Users/b/code/brreg-client".into(),
            missing: Missing::Route,
        },
        Unadopted {
            label: "altinn-forms".into(),
            slug: "Florin-AS/altinn-forms".into(),
            repo_root: "/Users/b/code/altinn-forms".into(),
            missing: Missing::Polling,
        },
    ]
}

/// What GitHub would answer for a repo that keeps its roadmap as open issues.
///
/// The shape that caused AGE-28, reproduced so the adoption screen can be
/// reviewed at the size it has to work at: a backlog far larger than the board,
/// carrying labels that would cut it down to what is current. The demo has no
/// network, so without this the screen is unreachable.
pub fn repo_preview() -> crate::adopt::RepoPreview {
    crate::adopt::RepoPreview {
        open_issues: 83,
        truncated: false,
        labels: vec![
            ("release-a".into(), 68),
            ("area:design".into(), 20),
            ("release-b".into(), 15),
            ("bug".into(), 9),
            ("needs-spec".into(), 4),
        ],
    }
}

pub fn app(scenario: &str) -> App {
    if scenario == UNADOPTED {
        // No tasks at all: nothing is polling these repos, which is the point.
        let mut app = App::new(Vec::new(), sync_ok(), CONFIG_PATH.to_string());
        app.unadopted = unadopted_repos();
        app.selected_id = app.first_actionable();
        return app;
    }
    let (views, sync) = match scenario {
        EMPTY => (Vec::new(), sync_ok()),
        CROWDED => (crowded_views(), sync_ok()),
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
    let mut app = App::new(views, sync, CONFIG_PATH.to_string());
    // The populated board carries one too: a repo going unnoticed while other
    // work is in flight is how it actually happens.
    if scenario != EMPTY {
        app.unadopted = unadopted_repos()[..1].to_vec();
    }
    app
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
            // `empty` has nothing at all by design, and `unadopted` has no
            // tasks precisely because nothing is polling the repo yet.
            if !matches!(*s, EMPTY | UNADOPTED) {
                assert!(!a.views.is_empty(), "{s} has no rows");
            }
            assert!(
                !a.rows().is_empty() || *s == EMPTY,
                "{s} draws no body at all"
            );
        }
    }

    #[test]
    fn the_unadopted_scenario_shows_all_three_shapes_of_missing_config() {
        // The two keys are independent, so a fixture that only ever shows
        // "neither is written" would never render the half-fixes.
        use crate::adopt::Missing;
        let a = app(UNADOPTED);
        for m in [Missing::Both, Missing::Route, Missing::Polling] {
            assert!(
                a.unadopted.iter().any(|u| u.missing == m),
                "no {m:?} row to look at"
            );
        }
        // And the cursor lands on a repo, not on the header: `a` is the whole
        // point of the screen.
        assert_eq!(a.selected_id, a.unadopted.first().map(|u| u.row_id()));
    }

    #[test]
    fn the_crowded_scenario_is_the_board_that_stopped_being_scannable() {
        // The shape AGE-27 is about, and the size the filter has to work at:
        // 129 rows that are not done, across five routes, 103 of them from the
        // one repo that polls every open issue it has.
        let a = app(CROWDED);
        let live = a.views.iter().filter(|v| v.state() != BoardState::Done).count();
        assert_eq!(live, 129, "the fixture is no longer the size of the problem");
        assert_eq!(
            a.routes_present(),
            vec!["altinn-forms", "brreg-client", "itsm-agent", "offhand", "tally"]
        );
        // ...and one more position after them, because a fifth of that repo's
        // backlog is polled and routed by nothing (gh#39).
        assert_eq!(a.filter_cycle().last(), Some(&Filter::NoRoute));
        assert_eq!(
            a.views.iter().filter(|v| !v.has_route).count(),
            21,
            "the deliberately undispatchable group is not a group any more"
        );
        let itsm = a
            .views
            .iter()
            .filter(|v| v.route_name.as_deref() == Some("itsm-agent"))
            .filter(|v| v.state() != BoardState::Done)
            .count();
        assert_eq!(itsm, 83);
        // No two rows read alike, or the fixture is not a board anyone believes.
        let titles: std::collections::HashSet<&str> =
            a.views.iter().map(|v| v.task.title.as_str()).collect();
        assert_eq!(titles.len(), a.views.len());
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
