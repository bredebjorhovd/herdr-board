//! What the board knows about its own throughput.
//!
//! Every attempt is already recorded with when it started, when it ended, how
//! it ended, which runtime ran it, which workspace it went to, and whether an
//! agent or the operator released it. That is enough to answer the only
//! question that matters about delegating work: whether it is actually
//! finishing, and how often it has to be done twice.
//!
//! Deliberately descriptive. It reports what happened; it does not grade it.

use crate::config::Paths;
use crate::db::Db;
use crate::log::Logger;
use crate::model::{Attempt, Outcome, Task};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    /// The window these numbers cover, in days. `None` means everything.
    pub since_days: Option<i64>,
    pub attempts: usize,
    pub tasks_touched: usize,
    pub outcomes: BTreeMap<String, usize>,
    /// Still running right now.
    pub live: usize,
    /// Minutes from dispatch to a finished attempt. Median, because one agent
    /// that ran overnight would drag a mean anywhere.
    pub median_minutes: Option<i64>,
    pub longest_minutes: Option<i64>,
    /// Attempts that ended in `done` as a share of ended attempts.
    pub completion_rate: Option<f64>,
    /// Tasks that needed more than one attempt.
    pub retried_tasks: usize,
    /// Released by an agent rather than by the operator.
    pub agent_dispatched: usize,
    pub by_workspace: BTreeMap<String, usize>,
    pub by_runtime: BTreeMap<String, usize>,
}

fn minutes(a: &Attempt) -> Option<i64> {
    let start = chrono::DateTime::parse_from_rfc3339(&a.started_at).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(a.ended_at.as_deref()?).ok()?;
    Some((end - start).num_minutes().max(0))
}

fn started_within(a: &Attempt, since_days: Option<i64>) -> bool {
    let Some(days) = since_days else {
        return true;
    };
    let Ok(start) = chrono::DateTime::parse_from_rfc3339(&a.started_at) else {
        return false;
    };
    (chrono::Utc::now() - start.with_timezone(&chrono::Utc)).num_days() < days
}

pub fn gather(tasks: &[Task], since_days: Option<i64>) -> Stats {
    let attempts: Vec<(&Task, &Attempt)> = tasks
        .iter()
        .flat_map(|t| t.attempts.iter().map(move |a| (t, a)))
        .filter(|(_, a)| started_within(a, since_days))
        .collect();

    let mut outcomes: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_workspace: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_runtime: BTreeMap<String, usize> = BTreeMap::new();
    let mut durations: Vec<i64> = Vec::new();
    let mut live = 0;
    let mut agent_dispatched = 0;

    for (_, a) in &attempts {
        match a.outcome {
            Some(o) => *outcomes.entry(o.as_str().to_string()).or_default() += 1,
            None => live += 1,
        }
        *by_workspace.entry(a.workspace.clone()).or_default() += 1;
        *by_runtime.entry(a.runtime.clone()).or_default() += 1;
        if a.dispatched_by.is_some() {
            agent_dispatched += 1;
        }
        if let Some(m) = minutes(a) {
            durations.push(m);
        }
    }
    durations.sort_unstable();

    let ended = attempts.iter().filter(|(_, a)| a.outcome.is_some()).count();
    let done = outcomes.get(Outcome::Done.as_str()).copied().unwrap_or(0);

    let touched: std::collections::HashSet<&str> =
        attempts.iter().map(|(t, _)| t.id.as_str()).collect();
    let retried_tasks = tasks
        .iter()
        .filter(|t| {
            t.attempts
                .iter()
                .filter(|a| started_within(a, since_days))
                .count()
                > 1
        })
        .count();

    Stats {
        since_days,
        attempts: attempts.len(),
        tasks_touched: touched.len(),
        outcomes,
        live,
        median_minutes: durations.get(durations.len() / 2).copied(),
        longest_minutes: durations.last().copied(),
        completion_rate: (ended > 0).then(|| done as f64 / ended as f64),
        retried_tasks,
        agent_dispatched,
        by_workspace,
        by_runtime,
    }
}

pub fn run(paths: &Paths, _log: Arc<Logger>, since_days: Option<i64>) -> Result<Stats> {
    let db = Db::open(&paths.db())?;
    Ok(gather(&db.load_tasks()?, since_days))
}

pub fn print(s: &Stats) {
    if s.attempts == 0 {
        println!("no dispatches yet");
        return;
    }
    let window = match s.since_days {
        Some(d) => format!("last {d} days"),
        None => "all time".into(),
    };
    println!("{window}");
    println!(
        "  {} dispatches across {} task(s), {} still running",
        s.attempts, s.tasks_touched, s.live
    );

    let outcomes: Vec<String> = s
        .outcomes
        .iter()
        .map(|(k, v)| format!("{v} {k}"))
        .collect();
    if !outcomes.is_empty() {
        println!("  finished: {}", outcomes.join(", "));
    }
    if let Some(rate) = s.completion_rate {
        println!("  {:.0}% of finished attempts ended in done", rate * 100.0);
    }
    if let (Some(med), Some(max)) = (s.median_minutes, s.longest_minutes) {
        println!("  {med} min median, {max} min longest");
    }
    if s.retried_tasks > 0 {
        println!(
            "  {} task(s) needed more than one go",
            s.retried_tasks
        );
    }
    if s.agent_dispatched > 0 {
        println!(
            "  {} released by an agent rather than by you",
            s.agent_dispatched
        );
    }
    let line = |label: &str, m: &BTreeMap<String, usize>| {
        if m.len() > 1 {
            let mut v: Vec<_> = m.iter().collect();
            v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
            println!(
                "  by {label}: {}",
                v.iter()
                    .map(|(k, n)| format!("{k} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    };
    line("workspace", &s.by_workspace);
    line("runtime", &s.by_runtime);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn task(id: &str, attempts: Vec<Attempt>) -> Task {
        Task {
            id: id.into(),
            source: Source::Linear,
            source_id: "u".into(),
            identifier: id.into(),
            title: "t".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            state: BoardState::Ready,
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
            synced_at: String::new(),
            attempts,
        }
    }

    fn attempt(minutes_ago: i64, ran_for: i64, outcome: Option<Outcome>, by: Option<&str>) -> Attempt {
        let start = chrono::Utc::now() - chrono::Duration::minutes(minutes_ago);
        Attempt {
            id: 0,
            task_id: String::new(),
            pane_id: None,
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: None,
            started_at: crate::db::rfc3339(start),
            ended_at: outcome
                .map(|_| crate::db::rfc3339(start + chrono::Duration::minutes(ran_for))),
            outcome,
            missing_ticks: 0,
            agent_status: None,
            dispatched_by: by.map(str::to_string),
            base_sha: None,
            saw_working: true,
        }
    }

    #[test]
    fn an_empty_board_reports_nothing_rather_than_dividing_by_zero() {
        let s = gather(&[], None);
        assert_eq!(s.attempts, 0);
        assert_eq!(s.completion_rate, None);
        assert_eq!(s.median_minutes, None);
    }

    #[test]
    fn duration_is_a_median_not_a_mean() {
        // One agent left running overnight would drag a mean anywhere.
        let t = task(
            "a",
            vec![
                attempt(600, 10, Some(Outcome::Done), None),
                attempt(500, 12, Some(Outcome::Done), None),
                attempt(400, 900, Some(Outcome::Done), None),
            ],
        );
        let s = gather(&[t], None);
        assert_eq!(s.median_minutes, Some(12));
        assert_eq!(s.longest_minutes, Some(900));
    }

    #[test]
    fn completion_counts_only_attempts_that_ended() {
        let t = task(
            "a",
            vec![
                attempt(60, 10, Some(Outcome::Done), None),
                attempt(50, 5, Some(Outcome::Failed), None),
                attempt(10, 0, None, None), // still running
            ],
        );
        let s = gather(&[t], None);
        assert_eq!(s.live, 1);
        assert_eq!(s.completion_rate, Some(0.5), "the live one is not a failure");
    }

    #[test]
    fn a_retry_is_counted_against_the_task_not_the_attempt() {
        let tasks = vec![
            task("a", vec![
                attempt(60, 5, Some(Outcome::Cancelled), None),
                attempt(50, 5, Some(Outcome::Done), None),
            ]),
            task("b", vec![attempt(40, 5, Some(Outcome::Done), None)]),
        ];
        let s = gather(&tasks, None);
        assert_eq!(s.retried_tasks, 1);
        assert_eq!(s.tasks_touched, 2);
        assert_eq!(s.attempts, 3);
    }

    #[test]
    fn provenance_shows_how_much_the_herd_released_itself() {
        let t = task(
            "a",
            vec![
                attempt(60, 5, Some(Outcome::Done), Some("linear:LIN-1")),
                attempt(50, 5, Some(Outcome::Done), None),
            ],
        );
        assert_eq!(gather(&[t], None).agent_dispatched, 1);
    }

    #[test]
    fn a_window_excludes_older_attempts() {
        let t = task(
            "a",
            vec![
                attempt(60 * 24 * 10, 5, Some(Outcome::Done), None), // 10 days ago
                attempt(30, 5, Some(Outcome::Done), None),
            ],
        );
        assert_eq!(gather(std::slice::from_ref(&t), None).attempts, 2);
        assert_eq!(gather(&[t], Some(7)).attempts, 1);
    }
}
