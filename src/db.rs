//! SQLite storage (impl spec §3). WAL, written atomically in transactions.
//!
//! The DB is the only bus between our processes: the daemon writes, the TUI
//! re-reads on a tick. There are no sockets between our own processes.

use crate::model::*;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub struct Db {
    pub conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        Db::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Db> {
        Db::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // A poll landing while the TUI reads should wait, not fail.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS tasks (
              id           TEXT PRIMARY KEY,
              source       TEXT NOT NULL,
              source_id    TEXT NOT NULL,
              identifier   TEXT NOT NULL,
              title        TEXT NOT NULL,
              body         TEXT,
              url          TEXT NOT NULL,
              labels       TEXT NOT NULL DEFAULT '[]',
              state        TEXT NOT NULL,
              source_state TEXT,
              -- Linear team key and project name, so routes can match on
              -- `linear_project` (the identifier prefix only gives us the team).
              linear_team    TEXT,
              linear_project TEXT,
              upstream     TEXT NOT NULL DEFAULT 'unstarted',
              local_done   INTEGER NOT NULL DEFAULT 0,
              pr_url       TEXT,
              pr_number    INTEGER,
              pr_open      INTEGER NOT NULL DEFAULT 0,
              pr_merged    INTEGER NOT NULL DEFAULT 0,
              -- GitHub's mergeable_state: clean, behind, dirty, blocked…
              pr_mergeable TEXT,
              updated_at   TEXT NOT NULL,
              synced_at    TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS attempts (
              id           INTEGER PRIMARY KEY,
              task_id      TEXT NOT NULL REFERENCES tasks(id),
              pane_id      TEXT,
              workspace    TEXT NOT NULL,
              runtime      TEXT NOT NULL,
              worktree     TEXT,
              branch       TEXT,
              started_at   TEXT NOT NULL,
              ended_at     TEXT,
              outcome      TEXT,
              missing_ticks INTEGER NOT NULL DEFAULT 0,
              -- Task id of the parent that released this one, when the board
              -- dispatched that parent too. NULL on its own does not mean
              -- "you": an orchestrator pane has no attempt and so no task id.
              dispatched_by TEXT,
              -- The pane the dispatch ran from, when an agent was in it. Both
              -- NULL is what "you" means; this one is set for every agent,
              -- since HERDR_PANE_ID is carried whether or not the board
              -- started the pane.
              dispatched_by_pane TEXT,
              -- Last agent status herdr reported for this attempt's pane. The
              -- TUI reads it to render the dim `idle` marker without having to
              -- shell out to herdr itself.
              agent_status TEXT
            );

            -- Impl spec §7: the duplicate-dispatch guard. A second concurrent
            -- dispatch (double enter, or pane racing the picker) fails this
            -- constraint instead of spawning a second pane.
            CREATE UNIQUE INDEX IF NOT EXISTS attempts_one_live
              ON attempts(task_id) WHERE outcome IS NULL;

            CREATE INDEX IF NOT EXISTS attempts_by_task ON attempts(task_id);

            CREATE TABLE IF NOT EXISTS writeback_queue (
              id         INTEGER PRIMARY KEY,
              task_id    TEXT NOT NULL,
              kind       TEXT NOT NULL,
              payload    TEXT NOT NULL,
              idem_key   TEXT NOT NULL,
              created_at TEXT NOT NULL,
              attempts   INTEGER NOT NULL DEFAULT 0,
              next_try_at TEXT,
              last_error TEXT,
              done       INTEGER NOT NULL DEFAULT 0
            );

            -- Writeback idempotency: the same logical effect can be enqueued
            -- repeatedly (retried sync, restarted daemon) without duplicating
            -- comments upstream.
            CREATE UNIQUE INDEX IF NOT EXISTS writeback_idem
              ON writeback_queue(idem_key);

            CREATE TABLE IF NOT EXISTS meta (
              key TEXT PRIMARY KEY, value TEXT NOT NULL
            );
            "#,
        )?;

        // `CREATE TABLE IF NOT EXISTS` does nothing to a table that already
        // exists, so every column added after the first release has to be
        // applied to existing databases explicitly. Without this an upgrade
        // leaves `load_tasks` failing on a missing column — which takes the
        // board pane down with it.
        self.add_missing_columns(
            "tasks",
            &[
                ("linear_team", "TEXT"),
                ("linear_project", "TEXT"),
                ("local_done", "INTEGER NOT NULL DEFAULT 0"),
                ("pr_url", "TEXT"),
                ("pr_number", "INTEGER"),
                ("pr_open", "INTEGER NOT NULL DEFAULT 0"),
                ("pr_merged", "INTEGER NOT NULL DEFAULT 0"),
                ("pr_mergeable", "TEXT"),
                ("upstream", "TEXT NOT NULL DEFAULT 'unstarted'"),
            ],
        )?;
        self.add_missing_columns(
            "attempts",
            &[
                ("missing_ticks", "INTEGER NOT NULL DEFAULT 0"),
                ("agent_status", "TEXT"),
                ("dispatched_by", "TEXT"),
                // Provenance's actual answer for the common case: the pane the
                // dispatch came from. Existing rows keep NULL — they were
                // recorded when only board-dispatched parents could be seen at
                // all, so their "released by you" was never checked (AGE-24).
                ("dispatched_by_pane", "TEXT"),
                // The commit the attempt branched from. Without it, "did the
                // agent produce anything" has to be measured against
                // origin/HEAD, which counts the operator's own unpushed work
                // as the agent's — see AGE-19.
                ("base_sha", "TEXT"),
                // Has this attempt ever been observed working? An agent that
                // was never seen working cannot have finished.
                ("saw_working", "INTEGER NOT NULL DEFAULT 0"),
            ],
        )?;
        self.add_missing_columns(
            "writeback_queue",
            &[
                ("idem_key", "TEXT NOT NULL DEFAULT ''"),
                ("next_try_at", "TEXT"),
                ("last_error", "TEXT"),
            ],
        )?;
        Ok(())
    }

    /// Add any of `columns` the table does not already have.
    fn add_missing_columns(&self, table: &str, columns: &[(&str, &str)]) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))?;
        let existing: std::collections::HashSet<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<_>>()?;
        for (name, ty) in columns {
            if !existing.contains(*name) {
                self.conn
                    .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} {ty}"))
                    .with_context(|| format!("adding {table}.{name}"))?;
            }
        }
        Ok(())
    }

    // ---- meta -----------------------------------------------------------

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- tasks ----------------------------------------------------------

    /// Insert or refresh a task from a source poll.
    ///
    /// Deliberately does **not** touch `state` (it is derived on every read) or
    /// `local_done` (an operator decision a poll must not clobber).
    pub fn upsert_task(&self, t: &UpsertTask) -> Result<()> {
        let labels = serde_json::to_string(&t.labels)?;
        self.conn.execute(
            "INSERT INTO tasks
               (id, source, source_id, identifier, title, body, url, labels,
                state, source_state, linear_team, linear_project, upstream,
                updated_at, synced_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
             ON CONFLICT(id) DO UPDATE SET
               title        = excluded.title,
               body         = excluded.body,
               url          = excluded.url,
               labels       = excluded.labels,
               source_state = excluded.source_state,
               linear_team    = excluded.linear_team,
               linear_project = excluded.linear_project,
               upstream     = excluded.upstream,
               updated_at   = excluded.updated_at,
               synced_at    = excluded.synced_at",
            params![
                t.id,
                t.source.as_str(),
                t.source_id,
                t.identifier,
                t.title,
                t.body,
                t.url,
                labels,
                // Seed value only; every read re-derives.
                BoardState::Ready.as_str(),
                t.source_state,
                t.linear_team,
                t.linear_project,
                t.upstream.as_str(),
                t.updated_at,
                now(),
            ],
        )?;
        Ok(())
    }

    /// Retire a task the source no longer returns.
    ///
    /// Two outcomes, decided by whether anyone ever worked on it (AGE-6):
    ///
    /// - **Nothing was ever dispatched** — the row is noise, an issue created and
    ///   deleted again, and it is forgotten outright.
    /// - **It has attempts** — the row stays, marked `gone` upstream. Deleting it
    ///   threw away the record of which agent ran, on what branch, and how it
    ///   ended; worse, it orphaned the attempt's checkout, leaving `gc` with a
    ///   directory it could not attribute to a repo and so refused to touch. The
    ///   kept row is what `gc` still collects by: `gone` is terminal, so the
    ///   checkout ages out normally instead of leaking.
    ///
    /// Queued writebacks go either way: there is no issue left to comment on, and
    /// a comment aimed at a deleted issue would fail and back off forever.
    /// Delivered ones stay, so their idempotency keys still hold if the task
    /// reappears.
    pub fn reap_task(&self, task_id: &str) -> Result<Reaped> {
        let tx = self.conn.unchecked_transaction()?;
        let attempts: i64 = tx.query_row(
            "SELECT COUNT(*) FROM attempts WHERE task_id = ?1",
            params![task_id],
            |r| r.get(0),
        )?;
        if attempts == 0 {
            tx.execute(
                "DELETE FROM writeback_queue WHERE task_id = ?1",
                params![task_id],
            )?;
            tx.execute("DELETE FROM tasks WHERE id = ?1", params![task_id])?;
            tx.commit()?;
            return Ok(Reaped::Forgotten);
        }
        tx.execute(
            "DELETE FROM writeback_queue WHERE task_id = ?1 AND done = 0",
            params![task_id],
        )?;
        // `state` is derived on every read, but it is also a stored column other
        // readers use — so it is set here rather than left claiming `ready` until
        // the next derivation pass.
        tx.execute(
            "UPDATE tasks SET upstream = ?2, state = ?3 WHERE id = ?1",
            params![
                task_id,
                UpstreamState::Gone.as_str(),
                BoardState::Done.as_str()
            ],
        )?;
        tx.commit()?;
        Ok(Reaped::Kept {
            attempts: attempts as usize,
        })
    }

    /// Task ids from one source that reaping still has something to do to.
    ///
    /// Tasks already marked `gone` are left out: they have been reaped once, and
    /// re-reaping them every sweep would say so in the log every two minutes.
    pub fn reapable_task_ids(&self, source: Source) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM tasks WHERE source = ?1 AND upstream <> ?2")?;
        let rows = stmt.query_map(
            params![source.as_str(), UpstreamState::Gone.as_str()],
            |r| r.get::<_, String>(0),
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_pr(
        &self,
        task_id: &str,
        url: Option<&str>,
        number: Option<i64>,
        open: bool,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET pr_url = ?2, pr_number = ?3, pr_open = ?4 WHERE id = ?1",
            params![task_id, url, number, open as i64],
        )?;
        Ok(())
    }

    pub fn set_pr_mergeable(&self, task_id: &str, state: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET pr_mergeable = ?2 WHERE id = ?1",
            params![task_id, state],
        )?;
        Ok(())
    }

    pub fn set_pr_merged(&self, task_id: &str, merged: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET pr_merged = ?2 WHERE id = ?1",
            params![task_id, merged as i64],
        )?;
        Ok(())
    }

    pub fn set_local_done(&self, task_id: &str, done: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET local_done = ?2 WHERE id = ?1",
            params![task_id, done as i64],
        )?;
        Ok(())
    }

    /// Persist the derived state so external readers (and the `state` column in
    /// the spec's schema) stay meaningful. Reads still derive.
    pub fn store_derived_state(&self, task_id: &str, state: BoardState) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET state = ?2 WHERE id = ?1",
            params![task_id, state.as_str()],
        )?;
        Ok(())
    }

    pub fn load_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source, source_id, identifier, title, body, url, labels,
                    state, source_state, linear_team, linear_project, upstream,
                    local_done, pr_url, pr_number, pr_open, pr_merged,
                    pr_mergeable, updated_at, synced_at
             FROM tasks",
        )?;
        let rows = stmt.query_map([], |r| {
            let labels: String = r.get(7)?;
            Ok(Task {
                id: r.get(0)?,
                source: Source::parse(&r.get::<_, String>(1)?).unwrap_or(Source::Linear),
                source_id: r.get(2)?,
                identifier: r.get(3)?,
                title: r.get(4)?,
                body: r.get(5)?,
                url: r.get(6)?,
                labels: serde_json::from_str(&labels).unwrap_or_default(),
                state: BoardState::parse(&r.get::<_, String>(8)?).unwrap_or(BoardState::Ready),
                source_state: r.get(9)?,
                linear_team: r.get(10)?,
                linear_project: r.get(11)?,
                upstream: UpstreamState::parse(&r.get::<_, String>(12)?)
                    .unwrap_or(UpstreamState::Unstarted),
                local_done: r.get::<_, i64>(13)? != 0,
                pr_url: r.get(14)?,
                pr_number: r.get(15)?,
                pr_open: r.get::<_, i64>(16)? != 0,
                pr_merged: r.get::<_, i64>(17)? != 0,
                pr_mergeable: r.get(18)?,
                updated_at: r.get(19)?,
                synced_at: r.get(20)?,
                attempts: Vec::new(),
            })
        })?;
        let mut tasks: Vec<Task> = rows.collect::<rusqlite::Result<_>>()?;
        for t in &mut tasks {
            t.attempts = self.attempts_for(&t.id)?;
        }
        Ok(tasks)
    }

    pub fn get_task(&self, id: &str) -> Result<Option<Task>> {
        Ok(self.load_tasks()?.into_iter().find(|t| t.id == id))
    }

    // ---- attempts -------------------------------------------------------

    pub fn attempts_for(&self, task_id: &str) -> Result<Vec<Attempt>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, pane_id, workspace, runtime, worktree, branch,
                    started_at, ended_at, outcome, missing_ticks, agent_status,
                    dispatched_by, dispatched_by_pane, base_sha, saw_working
             FROM attempts WHERE task_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![task_id], |r| {
            Ok(Attempt {
                id: r.get(0)?,
                task_id: r.get(1)?,
                pane_id: r.get(2)?,
                workspace: r.get(3)?,
                runtime: r.get(4)?,
                worktree: r.get(5)?,
                branch: r.get(6)?,
                started_at: r.get(7)?,
                ended_at: r.get(8)?,
                outcome: r
                    .get::<_, Option<String>>(9)?
                    .and_then(|s| Outcome::parse(&s)),
                missing_ticks: r.get(10)?,
                agent_status: r
                    .get::<_, Option<String>>(11)?
                    .map(|s| AgentStatus::parse(&s)),
                dispatched_by: r.get(12)?,
                dispatched_by_pane: r.get(13)?,
                base_sha: r.get(14)?,
                saw_working: r.get::<_, i64>(15)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Insert a live attempt. Fails cleanly if one is already open for this task
    /// — that is the duplicate-dispatch guard, enforced by a partial unique
    /// index rather than a read-then-write race.
    pub fn insert_attempt(&self, a: &NewAttempt) -> Result<i64> {
        let res = self.conn.execute(
            "INSERT INTO attempts
               (task_id, pane_id, workspace, runtime, worktree, branch,
                dispatched_by, dispatched_by_pane, started_at, base_sha)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                a.task_id,
                a.pane_id,
                a.workspace,
                a.runtime,
                a.worktree,
                a.branch,
                a.dispatched_by,
                a.dispatched_by_pane,
                now(),
                a.base_sha
            ],
        );
        match res {
            Ok(_) => Ok(self.conn.last_insert_rowid()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                anyhow::bail!("{} already has a live attempt", a.task_id)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_attempt_pane(&self, attempt_id: i64, pane_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET pane_id = ?2 WHERE id = ?1",
            params![attempt_id, pane_id],
        )?;
        Ok(())
    }

    pub fn close_attempt(&self, attempt_id: i64, outcome: Outcome) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET outcome = ?2, ended_at = ?3 WHERE id = ?1 AND outcome IS NULL",
            params![attempt_id, outcome.as_str(), now()],
        )?;
        Ok(())
    }

    pub fn set_attempt_status(&self, attempt_id: i64, status: AgentStatus) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET agent_status = ?2 WHERE id = ?1",
            params![attempt_id, status.as_str()],
        )?;
        Ok(())
    }

    /// Correct the attempt's starting commit once its checkout exists.
    ///
    /// Recorded provisionally from the repo's HEAD before dispatch, because the
    /// row is inserted before the worktree is cut — but a retry reuses a branch
    /// that already carries the previous attempt's commits, and measuring from
    /// the repo HEAD counts those as this attempt's output.
    pub fn set_attempt_base_sha(&self, attempt_id: i64, sha: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET base_sha = ?2 WHERE id = ?1",
            params![attempt_id, sha],
        )?;
        Ok(())
    }

    /// Latch that this attempt has been seen working. Never cleared: it records
    /// that the agent got going at all, not what it is doing now.
    pub fn set_saw_working(&self, attempt_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET saw_working = 1 WHERE id = ?1",
            params![attempt_id],
        )?;
        Ok(())
    }

    pub fn set_missing_ticks(&self, attempt_id: i64, ticks: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET missing_ticks = ?2 WHERE id = ?1",
            params![attempt_id, ticks],
        )?;
        Ok(())
    }

    /// Live attempts across all tasks, for reconciliation and concurrency caps.
    pub fn live_attempts(&self) -> Result<Vec<Attempt>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, pane_id, workspace, runtime, worktree, branch,
                    started_at, ended_at, outcome, missing_ticks, agent_status,
                    dispatched_by, dispatched_by_pane, base_sha, saw_working
             FROM attempts WHERE outcome IS NULL ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Attempt {
                id: r.get(0)?,
                task_id: r.get(1)?,
                pane_id: r.get(2)?,
                workspace: r.get(3)?,
                runtime: r.get(4)?,
                worktree: r.get(5)?,
                branch: r.get(6)?,
                started_at: r.get(7)?,
                ended_at: r.get(8)?,
                outcome: None,
                missing_ticks: r.get(10)?,
                agent_status: r
                    .get::<_, Option<String>>(11)?
                    .map(|s| AgentStatus::parse(&s)),
                dispatched_by: r.get(12)?,
                dispatched_by_pane: r.get(13)?,
                base_sha: r.get(14)?,
                saw_working: r.get::<_, i64>(15)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The live attempt that owns a pane, if any.
    ///
    /// This names a *board-dispatched* agent by its task. It is not how an
    /// agent is recognised — an orchestrator pane owns no attempt and would
    /// answer `None` here — only how one that has a task on the board gets the
    /// richer label. See `dispatch::dispatcher_from`.
    pub fn live_attempt_for_pane(&self, pane_id: &str) -> Result<Option<Attempt>> {
        Ok(self
            .live_attempts()?
            .into_iter()
            .find(|a| a.pane_id.as_deref() == Some(pane_id)))
    }

    /// Live attempts in a workspace — what `max_concurrent_per_workspace`
    /// counts. `blocked` is included because it still holds a pane.
    pub fn live_count_in_workspace(&self, workspace: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM attempts WHERE outcome IS NULL AND workspace = ?1",
            params![workspace],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    // ---- writeback queue ------------------------------------------------

    /// Enqueue a writeback. Returns `false` when this exact effect is already
    /// queued or already delivered — idempotency lives in the index, so a
    /// retried sync cannot double-comment upstream.
    pub fn enqueue_writeback(&self, w: &NewWriteback) -> Result<bool> {
        let res = self.conn.execute(
            "INSERT INTO writeback_queue(task_id, kind, payload, idem_key, created_at)
             VALUES(?1,?2,?3,?4,?5)",
            params![w.task_id, w.kind, w.payload, w.idem_key, now()],
        );
        match res {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn pending_writebacks(&self, limit: usize) -> Result<Vec<Writeback>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, kind, payload, idem_key, attempts, next_try_at
             FROM writeback_queue
             WHERE done = 0 AND (next_try_at IS NULL OR next_try_at <= ?1)
             ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![now(), limit as i64], |r| {
            Ok(Writeback {
                id: r.get(0)?,
                task_id: r.get(1)?,
                kind: r.get(2)?,
                payload: r.get(3)?,
                idem_key: r.get(4)?,
                attempts: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mark_writeback_done(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE writeback_queue SET done = 1, last_error = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Record a failure and schedule the next try with exponential backoff,
    /// capped at 5 minutes (impl spec §7).
    pub fn defer_writeback(&self, id: i64, attempts: i64, err: &str) -> Result<()> {
        let delay = backoff_secs(attempts + 1);
        let next = chrono::Utc::now() + chrono::Duration::seconds(delay as i64);
        self.conn.execute(
            "UPDATE writeback_queue
               SET attempts = ?2, next_try_at = ?3, last_error = ?4
             WHERE id = ?1",
            params![id, attempts + 1, rfc3339(next), err],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn pending_writeback_count(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM writeback_queue WHERE done = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }
}

/// Exponential backoff capped at 5 minutes.
pub fn backoff_secs(attempt: i64) -> u64 {
    let base = 2u64.saturating_pow(attempt.clamp(0, 16) as u32) * 5;
    base.min(300)
}

/// What reaping did to one task — see [`Db::reap_task`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaped {
    /// Never dispatched, so there was nothing to keep. Row and all.
    Forgotten,
    /// Kept, marked `gone` upstream, with this many attempts still on it.
    Kept { attempts: usize },
}

pub struct UpsertTask {
    pub id: String,
    pub source: Source,
    pub source_id: String,
    pub identifier: String,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    pub labels: Vec<String>,
    pub source_state: Option<String>,
    pub linear_team: Option<String>,
    pub linear_project: Option<String>,
    pub upstream: UpstreamState,
    pub updated_at: String,
}

pub struct NewAttempt {
    pub task_id: String,
    pub pane_id: Option<String>,
    pub workspace: String,
    pub runtime: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    /// Parent task id when the board dispatched the releasing agent too.
    /// `None` here does not mean the operator — see `dispatched_by_pane`.
    pub dispatched_by: Option<String>,
    /// The pane the dispatch ran from, when an agent was in it. Both fields
    /// `None` is the operator.
    pub dispatched_by_pane: Option<String>,
    /// The commit the attempt's branch was cut from, so "what did the agent
    /// produce" can be measured against the attempt's own starting point
    /// rather than against the remote.
    pub base_sha: Option<String>,
}

pub struct NewWriteback {
    pub task_id: String,
    pub kind: String,
    pub payload: String,
    pub idem_key: String,
}

#[derive(Debug, Clone)]
pub struct Writeback {
    pub id: i64,
    pub task_id: String,
    pub kind: String,
    pub payload: String,
    pub idem_key: String,
    pub attempts: i64,
}

pub fn now() -> String {
    rfc3339(chrono::Utc::now())
}

/// All timestamps are UTC RFC3339 (impl spec §7).
pub fn rfc3339(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn seed(db: &Db, id: &str) {
        db.upsert_task(&UpsertTask {
            id: id.into(),
            source: Source::Linear,
            source_id: "uuid".into(),
            identifier: "LIN-142".into(),
            title: "Add retry".into(),
            body: None,
            url: "https://linear.app/x/issue/LIN-142".into(),
            labels: vec!["herd".into()],
            source_state: Some("Todo".into()),
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: now(),
        })
        .unwrap();
    }

    fn attempt(task: &str) -> NewAttempt {
        NewAttempt {
            task_id: task.into(),
            pane_id: None,
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: Some("board/lin-142".into()),
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
        }
    }

    #[test]
    fn an_older_database_gains_the_columns_it_is_missing() {
        // The upgrade path: a database created before a column existed must not
        // take the board down on the next read.
        let dir = std::env::temp_dir().join(format!("hb-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        let _ = std::fs::remove_file(&path);

        // A first-release schema: no dispatched_by, no agent_status, no
        // linear_team/linear_project.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tasks (
                   id TEXT PRIMARY KEY, source TEXT NOT NULL, source_id TEXT NOT NULL,
                   identifier TEXT NOT NULL, title TEXT NOT NULL, body TEXT,
                   url TEXT NOT NULL, labels TEXT NOT NULL DEFAULT '[]',
                   state TEXT NOT NULL, source_state TEXT,
                   updated_at TEXT NOT NULL, synced_at TEXT NOT NULL);
                 CREATE TABLE attempts (
                   id INTEGER PRIMARY KEY, task_id TEXT NOT NULL,
                   pane_id TEXT, workspace TEXT NOT NULL, runtime TEXT NOT NULL,
                   worktree TEXT, branch TEXT, started_at TEXT NOT NULL,
                   ended_at TEXT, outcome TEXT);
                 INSERT INTO tasks VALUES('linear:LIN-1','linear','u','LIN-1','t',NULL,
                   'url','[]','ready',NULL,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z');
                 INSERT INTO attempts VALUES(1,'linear:LIN-1','w1:p1','ws','claude-code',
                   NULL,NULL,'2026-01-01T00:00:00Z',NULL,NULL);",
            )
            .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let tasks = db.load_tasks().unwrap();
        assert_eq!(tasks.len(), 1, "the existing row survives the upgrade");
        assert_eq!(tasks[0].attempts.len(), 1);
        assert_eq!(tasks[0].attempts[0].dispatched_by, None);
        assert!(!tasks[0].local_done);

        // And it is idempotent — opening again must not try to add them twice.
        let db = Db::open(&path).unwrap();
        assert_eq!(db.load_tasks().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_is_idempotent_and_refreshes_fields() {
        let db = db();
        seed(&db, "linear:LIN-142");
        seed(&db, "linear:LIN-142");
        assert_eq!(db.load_tasks().unwrap().len(), 1);
    }

    #[test]
    fn a_poll_cannot_clobber_local_done() {
        // Otherwise `d mark done` undoes itself on the next sync.
        let db = db();
        seed(&db, "linear:LIN-142");
        db.set_local_done("linear:LIN-142", true).unwrap();
        seed(&db, "linear:LIN-142");
        assert!(db.load_tasks().unwrap()[0].local_done);
    }

    #[test]
    fn second_live_attempt_is_refused() {
        // Impl spec §7 duplicate dispatch: double enter, or pane racing picker.
        let db = db();
        seed(&db, "linear:LIN-142");
        db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        let err = db
            .insert_attempt(&attempt("linear:LIN-142"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("already has a live attempt"), "{err}");
    }

    #[test]
    fn a_new_attempt_is_allowed_once_the_last_one_closed() {
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.close_attempt(a, Outcome::Failed).unwrap();
        db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        assert_eq!(db.attempts_for("linear:LIN-142").unwrap().len(), 2);
    }

    #[test]
    fn concurrency_counts_only_live_attempts_in_that_workspace() {
        let db = db();
        seed(&db, "linear:LIN-1");
        seed(&db, "linear:LIN-2");
        let a1 = db.insert_attempt(&attempt("linear:LIN-1")).unwrap();
        db.insert_attempt(&attempt("linear:LIN-2")).unwrap();
        assert_eq!(db.live_count_in_workspace("offhand").unwrap(), 2);
        assert_eq!(db.live_count_in_workspace("fintech").unwrap(), 0);
        db.close_attempt(a1, Outcome::Done).unwrap();
        assert_eq!(db.live_count_in_workspace("offhand").unwrap(), 1);
    }

    #[test]
    fn writeback_enqueue_is_idempotent() {
        let db = db();
        seed(&db, "linear:LIN-142");
        let w = NewWriteback {
            task_id: "linear:LIN-142".into(),
            kind: "comment".into(),
            payload: "{}".into(),
            idem_key: "linear:LIN-142:dispatch:1".into(),
        };
        assert!(db.enqueue_writeback(&w).unwrap());
        // Same logical effect enqueued again — a retried sync must not produce
        // a second comment upstream.
        assert!(!db.enqueue_writeback(&w).unwrap());
        assert_eq!(db.pending_writebacks(10).unwrap().len(), 1);
    }

    #[test]
    fn a_delivered_writeback_is_never_re_enqueued() {
        let db = db();
        seed(&db, "linear:LIN-142");
        let w = NewWriteback {
            task_id: "linear:LIN-142".into(),
            kind: "comment".into(),
            payload: "{}".into(),
            idem_key: "k1".into(),
        };
        db.enqueue_writeback(&w).unwrap();
        let pending = db.pending_writebacks(10).unwrap();
        db.mark_writeback_done(pending[0].id).unwrap();
        assert!(!db.enqueue_writeback(&w).unwrap());
        assert!(db.pending_writebacks(10).unwrap().is_empty());
    }

    #[test]
    fn deferred_writebacks_leave_the_ready_set_then_come_back() {
        let db = db();
        seed(&db, "linear:LIN-142");
        db.enqueue_writeback(&NewWriteback {
            task_id: "linear:LIN-142".into(),
            kind: "comment".into(),
            payload: "{}".into(),
            idem_key: "k1".into(),
        })
        .unwrap();
        let w = db.pending_writebacks(10).unwrap().remove(0);
        db.defer_writeback(w.id, w.attempts, "linear unreachable")
            .unwrap();
        // Backed off into the future, so it is not ready now...
        assert!(db.pending_writebacks(10).unwrap().is_empty());
        // ...but it is still pending and will drain when the source returns.
        assert_eq!(db.pending_writeback_count().unwrap(), 1);
    }

    #[test]
    fn backoff_grows_and_caps_at_five_minutes() {
        assert!(backoff_secs(1) < backoff_secs(3));
        assert_eq!(backoff_secs(20), 300);
    }

    #[test]
    fn reaping_a_task_nobody_worked_on_forgets_it_entirely() {
        // An issue created and deleted again five minutes later was noise; there
        // is no history to protect.
        let db = db();
        seed(&db, "linear:LIN-142");
        db.enqueue_writeback(&NewWriteback {
            task_id: "linear:LIN-142".into(),
            kind: "comment".into(),
            payload: "{}".into(),
            idem_key: "k".into(),
        })
        .unwrap();

        assert_eq!(db.reap_task("linear:LIN-142").unwrap(), Reaped::Forgotten);
        assert!(db.load_tasks().unwrap().is_empty());
        assert_eq!(db.pending_writeback_count().unwrap(), 0);
    }

    #[test]
    fn reaping_a_task_that_was_worked_on_keeps_its_attempts() {
        // AGE-6: the record of which agent ran, on what branch and how it ended
        // is the whole point — and without the attempt row, `gc` can no longer
        // attribute the checkout it left behind.
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.conn
            .execute(
                "UPDATE attempts SET worktree = '/wt/lin-142-1' WHERE id = ?1",
                params![a],
            )
            .unwrap();
        db.close_attempt(a, Outcome::Done).unwrap();

        assert_eq!(
            db.reap_task("linear:LIN-142").unwrap(),
            Reaped::Kept { attempts: 1 }
        );
        let t = db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(t.upstream, UpstreamState::Gone);
        assert_eq!(t.state, BoardState::Done, "stored state is set, not stale");
        assert_eq!(t.attempts.len(), 1);
        assert_eq!(t.attempts[0].worktree.as_deref(), Some("/wt/lin-142-1"));
        assert_eq!(t.attempts[0].branch.as_deref(), Some("board/lin-142"));
    }

    #[test]
    fn reaping_drops_queued_writebacks_but_keeps_delivered_ones() {
        // Nothing upstream to comment on any more, so a queued comment would
        // fail and back off forever. The delivered rows are the idempotency
        // ledger and cost nothing to keep.
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.close_attempt(a, Outcome::Done).unwrap();
        for key in ["delivered", "queued"] {
            db.enqueue_writeback(&NewWriteback {
                task_id: "linear:LIN-142".into(),
                kind: "comment".into(),
                payload: "{}".into(),
                idem_key: key.into(),
            })
            .unwrap();
        }
        let delivered = db.pending_writebacks(10).unwrap()[0].id;
        db.mark_writeback_done(delivered).unwrap();

        db.reap_task("linear:LIN-142").unwrap();
        assert_eq!(db.pending_writeback_count().unwrap(), 0);
        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM writeback_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 1, "the delivered row survives");
    }

    #[test]
    fn an_already_reaped_task_is_not_reaped_again() {
        // Otherwise every sweep re-reaps it and says so in the log, forever.
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.close_attempt(a, Outcome::Done).unwrap();
        db.reap_task("linear:LIN-142").unwrap();
        assert!(db.reapable_task_ids(Source::Linear).unwrap().is_empty());
    }

    #[test]
    fn task_ids_are_listed_per_source() {
        let db = db();
        seed(&db, "linear:LIN-142");
        assert_eq!(
            db.reapable_task_ids(Source::Linear).unwrap(),
            vec!["linear:LIN-142"]
        );
        assert!(db.reapable_task_ids(Source::Github).unwrap().is_empty());
    }

    #[test]
    fn meta_round_trips() {
        let db = db();
        assert_eq!(db.meta_get("cursor").unwrap(), None);
        db.meta_set("cursor", "2026-07-25T00:00:00Z").unwrap();
        db.meta_set("cursor", "2026-07-26T00:00:00Z").unwrap();
        assert_eq!(
            db.meta_get("cursor").unwrap().as_deref(),
            Some("2026-07-26T00:00:00Z")
        );
    }

    #[test]
    fn attempts_load_in_order_with_their_task() {
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.set_attempt_pane(a, "w1:p4").unwrap();
        db.close_attempt(a, Outcome::Cancelled).unwrap();
        db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        let t = db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(t.attempts.len(), 2);
        assert_eq!(t.attempts[0].outcome, Some(Outcome::Cancelled));
        assert_eq!(t.attempts[0].pane_id.as_deref(), Some("w1:p4"));
        assert!(t.live_attempt().is_some());
    }
}
