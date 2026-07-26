//! `gc` against a real repository.
//!
//! The unit tests decide *what* is collectable; this decides whether the removal
//! itself behaves — that the checkout goes, the branch survives it, and a
//! checkout with work in it is refused rather than thrown away. All three are
//! git's behaviour, not ours, so they are worth pinning to real git.

use herdr_board::config::Paths;
use herdr_board::db::{Db, NewAttempt, UpsertTask, rfc3339};
use herdr_board::gc;
use herdr_board::log::Logger;
use herdr_board::model::{Outcome, Source, UpstreamState};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("running git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct Fixture {
    paths: Paths,
    repo: PathBuf,
    root: PathBuf,
}

impl Fixture {
    /// A repo with one commit, and a state dir to hang worktrees off.
    fn new(name: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "hb-gc-it-{}-{name}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = root.join("repo");
        let state = root.join("state");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&state).unwrap();

        git(&repo, &["init", "-q", "--initial-branch=main", "."]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "test"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);

        Fixture {
            paths: Paths {
                config_dir: state.clone(),
                state_dir: state,
            },
            repo,
            root,
        }
    }

    /// Cut a worktree the way dispatch does, and record the attempt that owns
    /// it as closed `days_ago` with `outcome`.
    fn attempt(&self, ident: &str, branch: &str, outcome: Outcome, days_ago: i64) -> PathBuf {
        let worktree = self.paths.worktree_root().join(ident);
        std::fs::create_dir_all(self.paths.worktree_root()).unwrap();
        git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                &worktree.to_string_lossy(),
                "-b",
                branch,
            ],
        );

        let db = Db::open(&self.paths.db()).unwrap();
        let id = format!("linear:{ident}");
        db.upsert_task(&UpsertTask {
            id: id.clone(),
            source: Source::Linear,
            source_id: "u".into(),
            identifier: ident.into(),
            title: "t".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: None,
            linear_team: None,
            linear_project: None,
            // Closed upstream: `done` on the board, so the checkout is nobody's
            // to reuse.
            upstream: UpstreamState::Terminal,
            updated_at: herdr_board::db::now(),
        })
        .unwrap();
        let attempt = db
            .insert_attempt(&NewAttempt {
                task_id: id,
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: Some(worktree.to_string_lossy().into_owned()),
                branch: Some(branch.into()),
                dispatched_by: None,
            })
            .unwrap();
        db.close_attempt(attempt, outcome).unwrap();
        let ended = chrono::Utc::now() - chrono::Duration::days(days_ago);
        db.conn
            .execute(
                "UPDATE attempts SET ended_at = ?2 WHERE id = ?1",
                rusqlite::params![attempt, rfc3339(ended)],
            )
            .unwrap();
        worktree
    }

    fn gc(&self, older_than: &str, dry_run: bool) -> gc::Report {
        gc::run(
            &self.paths,
            Arc::new(Logger::new("", false)),
            older_than,
            dry_run,
        )
        .unwrap()
    }

    fn branches(&self) -> String {
        git(&self.repo, &["branch", "--list", "--format=%(refname:short)"])
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_dry_run_removes_nothing() {
    let f = Fixture::new("dry");
    let wt = f.attempt("LIN-1", "board/lin-1", Outcome::Done, 30);

    let report = f.gc("14d", true);
    assert_eq!(report.collected.len(), 1);
    assert_eq!(report.collected[0].worktree, wt);
    assert!(wt.exists(), "--dry-run must leave the checkout in place");
    assert!(f.branches().contains("board/lin-1"));
}

#[test]
fn an_aged_finished_attempt_loses_its_checkout_but_keeps_its_branch() {
    let f = Fixture::new("remove");
    let wt = f.attempt("LIN-1", "board/lin-1", Outcome::Done, 30);

    let report = f.gc("14d", false);
    assert_eq!(report.collected.len(), 1, "{report:#?}");
    assert!(report.skipped.is_empty(), "{report:#?}");
    assert!(!wt.exists(), "the checkout should be gone");
    // The whole point of removing only the checkout: the work is still there,
    // and git is free to hand the branch to a new worktree.
    assert!(
        f.branches().contains("board/lin-1"),
        "gc must not delete the branch: {}",
        f.branches()
    );
    assert!(
        !git(&f.repo, &["worktree", "list"]).contains("LIN-1"),
        "git should no longer register the worktree"
    );
}

#[test]
fn a_checkout_with_uncommitted_work_is_left_alone_and_says_so() {
    // git refuses this, and that refusal is the right answer: the age says
    // nobody is coming back, but the files say otherwise.
    let f = Fixture::new("dirty");
    let wt = f.attempt("LIN-1", "board/lin-1", Outcome::Done, 30);
    std::fs::write(wt.join("scratch.txt"), "half-finished work").unwrap();

    let report = f.gc("14d", false);
    assert!(report.collected.is_empty());
    assert_eq!(report.skipped.len(), 1, "{report:#?}");
    assert!(
        report.skipped[0].1.contains("modified or untracked"),
        "git's reason should survive into the report: {}",
        report.skipped[0].1
    );
    assert!(wt.exists());
}

#[test]
fn a_checkout_already_deleted_by_hand_is_not_an_error() {
    let f = Fixture::new("gone");
    let wt = f.attempt("LIN-1", "board/lin-1", Outcome::Done, 30);
    std::fs::remove_dir_all(&wt).unwrap();

    let report = f.gc("14d", false);
    assert!(report.collected.is_empty());
    assert!(report.skipped.is_empty(), "{report:#?}");
    assert_eq!(report.gone.len(), 1);
}

#[test]
fn a_still_retryable_task_keeps_its_checkout() {
    // Cancelled, ancient, and still not gc's business: the row is back in
    // `ready`, and a retry needs the checkout holding the branch.
    let f = Fixture::new("cancelled");
    let wt = f.attempt("LIN-1", "board/lin-1", Outcome::Cancelled, 400);
    // Cancelled ends the attempt, not the issue — so put the issue back where a
    // cancel leaves it.
    let db = Db::open(&f.paths.db()).unwrap();
    db.conn
        .execute(
            "UPDATE tasks SET upstream = 'started' WHERE id = 'linear:LIN-1'",
            [],
        )
        .unwrap();
    drop(db);

    let report = f.gc("14d", false);
    assert!(report.collected.is_empty(), "{report:#?}");
    assert_eq!(report.kept.len(), 1);
    assert!(wt.exists());
}

#[test]
fn the_worktree_root_is_scanned_for_directories_no_attempt_claims() {
    // A task reaped from the board takes its attempts with it and leaves the
    // checkout behind. gc reports those; it does not guess at them.
    let f = Fixture::new("untracked");
    f.attempt("LIN-1", "board/lin-1", Outcome::Done, 30);
    let stray = f.paths.worktree_root().join("gh-503-1");
    std::fs::create_dir_all(&stray).unwrap();

    let report = f.gc("14d", true);
    assert_eq!(report.untracked, vec![stray]);
}
