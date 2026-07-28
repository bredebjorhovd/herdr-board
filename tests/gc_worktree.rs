//! `gc` against a real repository.
//!
//! The unit tests decide *what* is collectable; this decides whether the removal
//! itself behaves — that the checkout goes, the branch survives it, and a
//! checkout with work in it is refused rather than thrown away. All three are
//! git's behaviour, not ours, so they are worth pinning to real git.
//!
//! herdr is faked, and deliberately: these are about git. What herdr contributes
//! is *which* of the two removal paths a checkout takes, and the fake is how a
//! test picks one — [`Fake::orphaned`] for a checkout no workspace holds, which
//! is what a cancelled attempt leaves, and [`Fake::open`] for one herdr still
//! has open.

use herdr_board::config::{Paths, RoutingConfig};
use herdr_board::db::{Db, NewAttempt, Reaped, UpsertTask, rfc3339};
use herdr_board::gc::{self, Worktrees};
use herdr_board::herdr::WorktreeInfo;
use herdr_board::log::Logger;
use herdr_board::model::{Outcome, Source, UpstreamState};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// The branch a checkout has, or `None` once it is off disk.
fn branch_of(worktree: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A herdr that reports the checkouts it was handed.
struct Fake {
    worktrees: Vec<WorktreeInfo>,
    /// Workspaces `gc` asked herdr to remove.
    removed: RefCell<Vec<String>>,
    /// Where the real removal happens for an open workspace: herdr runs
    /// `git worktree remove` itself, and this stands in for that.
    repo: PathBuf,
}

impl Fake {
    /// Checkouts on disk that no workspace holds — what cancelling leaves
    /// behind, and the case git has to handle on its own.
    fn orphaned(repo: &Path, paths: &[&Path]) -> Fake {
        Fake::new(repo, paths.iter().map(|p| (*p, None)).collect())
    }

    /// A checkout herdr still has open as a workspace.
    fn open(repo: &Path, path: &Path, workspace: &str) -> Fake {
        Fake::new(repo, vec![(path, Some(workspace))])
    }

    fn new(repo: &Path, worktrees: Vec<(&Path, Option<&str>)>) -> Fake {
        Fake {
            repo: repo.to_path_buf(),
            removed: RefCell::new(Vec::new()),
            worktrees: worktrees
                .into_iter()
                .map(|(path, workspace)| WorktreeInfo {
                    path: path.to_path_buf(),
                    // The real thing reports the checked-out branch, and gc
                    // uses it to tell the board's own strays from your work.
                    branch: branch_of(path),
                    is_linked_worktree: true,
                    is_prunable: !path.exists(),
                    open_workspace_id: workspace.map(str::to_string),
                })
                .collect(),
        }
    }
}

impl Worktrees for Fake {
    fn list(&self, _repo: &Path) -> anyhow::Result<Vec<WorktreeInfo>> {
        Ok(self.worktrees.clone())
    }

    /// As herdr does it: the checkout goes *and* the workspace closes.
    fn remove(&self, workspace_id: &str) -> anyhow::Result<()> {
        self.removed.borrow_mut().push(workspace_id.to_string());
        let wt = self
            .worktrees
            .iter()
            .find(|w| w.open_workspace_id.as_deref() == Some(workspace_id))
            .expect("gc named a workspace herdr never reported");
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "remove"])
            .arg(&wt.path)
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(())
    }
}

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
    /// Every checkout cut in this fixture, so the fake herdr can report the
    /// same set git has.
    checkouts: RefCell<Vec<PathBuf>>,
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
            checkouts: RefCell::new(Vec::new()),
        }
    }

    /// Where herdr puts a checkout: its own worktrees directory, grouped by
    /// repo — deliberately *not* under `$STATE/wt`, which is the directory gc
    /// used to walk and which nothing writes to any more.
    fn worktrees_dir(&self) -> PathBuf {
        self.root.join("herdr-worktrees").join("repo")
    }

    /// Cut a checkout where herdr would, and register it.
    fn checkout(&self, slug: &str, branch: &str) -> PathBuf {
        let worktree = self.worktrees_dir().join(slug);
        std::fs::create_dir_all(self.worktrees_dir()).unwrap();
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
        self.checkouts.borrow_mut().push(worktree.clone());
        worktree
    }

    /// Cut a checkout the way dispatch does, and record the attempt that owns
    /// it as closed `days_ago` with `outcome`.
    fn attempt(&self, ident: &str, branch: &str, outcome: Outcome, days_ago: i64) -> PathBuf {
        let worktree = self.checkout(ident, branch);

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
            base_sha: None,
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

    /// A routing config pointing at the fixture repo, so `gc` has a repository
    /// to ask herdr about.
    fn cfg(&self) -> RoutingConfig {
        toml::from_str(&format!(
            r#"
[[route]]
match = {{ linear_team = "LIN" }}
workspace = "offhand"
repo = "{}"
runtime = "claude-code"
"#,
            self.repo.display()
        ))
        .unwrap()
    }

    fn gc_with(&self, herdr: &dyn Worktrees, older_than: &str, dry_run: bool) -> gc::Report {
        gc::run(
            &self.paths,
            &self.cfg(),
            herdr,
            Arc::new(Logger::new("", false)),
            older_than,
            dry_run,
        )
        .unwrap()
    }

    /// The ordinary case: the checkouts are there, and no workspace holds any
    /// of them.
    fn gc(&self, older_than: &str, dry_run: bool) -> gc::Report {
        let all: Vec<PathBuf> = self.checkouts.borrow().clone();
        let refs: Vec<&Path> = all.iter().map(PathBuf::as_path).collect();
        self.gc_with(&Fake::orphaned(&self.repo, &refs), older_than, dry_run)
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
fn a_reaped_task_still_gives_up_its_checkout() {
    // AGE-6, end to end, and the reason it was more than record-keeping: AGE-2
    // and AGE-4 were closed in Linear once their work merged, the next sweep
    // reaped them, and the attempts went with the rows. gc then had a directory
    // it could not attribute to any repo and — correctly — refused to touch it,
    // so the checkouts leaked until they were removed by hand.
    //
    // Now the row survives as `gone`, which keeps the worktree path and the repo
    // behind it, and `gone` is terminal — so the checkout ages out the ordinary
    // way instead of becoming untracked.
    let f = Fixture::new("reaped");
    let wt = f.attempt("LIN-1", "board/lin-1", Outcome::Done, 30);
    let db = Db::open(&f.paths.db()).unwrap();
    assert_eq!(
        db.reap_task("linear:LIN-1").unwrap(),
        Reaped::Kept { attempts: 1 },
        "a task that was worked on is kept, not deleted"
    );
    drop(db);

    let report = f.gc("14d", false);
    assert!(
        report.untracked.is_empty(),
        "the checkout must still be attributable: {report:#?}"
    );
    assert_eq!(report.collected.len(), 1, "{report:#?}");
    assert_eq!(report.collected[0].worktree, wt);
    assert!(!wt.exists(), "the checkout should be collected, not leaked");
    assert!(
        f.branches().contains("board/lin-1"),
        "and the branch still survives it: {}",
        f.branches()
    );
}

#[test]
fn herdrs_listing_is_what_turns_up_a_checkout_no_attempt_claims() {
    // What is left once reaping no longer strands checkouts (AGE-6): a checkout
    // that was never ours — hand-made, or from a database that was thrown away.
    // gc reports those; it does not guess at them. And it finds them by asking
    // herdr, because there is no directory of ours left to walk.
    let f = Fixture::new("untracked");
    f.attempt("LIN-1", "board/lin-1", Outcome::Done, 30);
    let stray = f.checkout("gh-503-1", "board/gh-503");

    // A repository the board works in is one you work in too, and herdr lists
    // every checkout of it — so a worktree of your own must not be reported.
    let mine = f.checkout("altinn-innboks", "feat/altinn-innboks");

    let report = f.gc("14d", true);
    assert_eq!(report.untracked.len(), 1, "{report:#?}");
    assert_eq!(report.untracked[0].path, stray);
    assert_eq!(report.untracked[0].branch.as_deref(), Some("board/gh-503"));
    assert!(stray.exists(), "reported, never removed");
    assert!(mine.exists());
}

#[test]
fn a_checkout_outside_the_old_state_directory_is_still_collected() {
    // The regression this all exists for: dispatch stopped putting checkouts
    // under `$STATE/wt`, gc kept walking it, and every real checkout went
    // uncollected while the directory it watched stayed empty.
    let f = Fixture::new("elsewhere");
    let wt = f.attempt("LIN-1", "board/lin-1", Outcome::Done, 30);
    assert!(
        !wt.starts_with(f.paths.worktree_root()),
        "the fixture must cut checkouts where herdr does, not where gc used to look"
    );

    let report = f.gc("14d", false);
    assert_eq!(report.collected.len(), 1, "{report:#?}");
    assert!(!wt.exists());
}

#[test]
fn a_checkout_herdr_still_has_open_is_removed_through_herdr() {
    // Removing it with git alone would leave herdr showing a workspace whose
    // directory is gone; `herdr worktree remove` does both halves.
    let f = Fixture::new("workspace");
    let wt = f.attempt("LIN-1", "board/lin-1", Outcome::Done, 30);
    let herdr = Fake::open(&f.repo, &wt, "wE");

    let report = f.gc_with(&herdr, "14d", false);
    assert_eq!(report.collected.len(), 1, "{report:#?}");
    assert_eq!(
        herdr.removed.borrow().as_slice(),
        ["wE"],
        "the workspace has to be closed with the checkout"
    );
    assert!(!wt.exists());
    assert!(f.branches().contains("board/lin-1"), "and the branch stays");
    assert_eq!(
        report.collected[0].herdr_workspace.as_deref(),
        Some("wE"),
        "the report should say which route it went by"
    );
}

#[test]
fn a_checkout_whose_workspace_is_gone_is_removed_with_git() {
    // The common case, and the one that used to need doing by hand: a workspace
    // whose last pane closes is dropped, so cancelling an attempt takes the
    // workspace with it and leaves the checkout holding its branch.
    let f = Fixture::new("orphan");
    let wt = f.attempt("LIN-1", "board/lin-1", Outcome::Done, 30);
    let herdr = Fake::orphaned(&f.repo, &[&wt]);

    let report = f.gc_with(&herdr, "14d", false);
    assert_eq!(report.collected.len(), 1, "{report:#?}");
    assert!(
        herdr.removed.borrow().is_empty(),
        "there is no workspace to name — asking herdr would only fail"
    );
    assert!(!wt.exists());
    assert!(report.collected[0].herdr_workspace.is_none());
    assert!(f.branches().contains("board/lin-1"));
}

#[test]
fn a_checkout_deleted_by_hand_has_its_stale_entry_pruned() {
    // git keeps the administrative entry when the directory goes behind its
    // back, and that entry still holds the branch — so a retry cannot cut a
    // fresh checkout of it until the entry is pruned.
    let f = Fixture::new("prune");
    let wt = f.attempt("LIN-1", "board/lin-1", Outcome::Done, 30);
    std::fs::remove_dir_all(&wt).unwrap();
    assert!(
        git(&f.repo, &["worktree", "list"]).contains("LIN-1"),
        "git should still be holding the entry before gc runs"
    );

    let report = f.gc("14d", false);
    assert_eq!(report.gone.len(), 1, "{report:#?}");
    assert_eq!(report.pruned, vec![f.repo.clone()]);
    assert!(
        !git(&f.repo, &["worktree", "list"]).contains("LIN-1"),
        "the stale entry should be gone: {}",
        git(&f.repo, &["worktree", "list"])
    );
    // Pruning frees the branch without deleting it — which is the point.
    assert!(f.branches().contains("board/lin-1"));
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "-q",
            &f.worktrees_dir().join("retry").to_string_lossy(),
            "board/lin-1",
        ],
    );
}
