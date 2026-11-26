//! Sync sources and jobs run from the web panel.
use crate::{Result, Tx};
use rusqlite::{params, OptionalExtension};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRow {
    pub repo: String,
    pub spec: String,
    pub updated_at: i64,
    pub updated_by: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRow {
    pub id: i64,
    pub kind: String,
    pub target: String,
    /// JSON array: the command line the job runs, for the record.
    pub args: String,
    pub state: String,
    pub actor: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i64>,
    pub log: String,
}

/// The newest part of a job log that is kept.
pub const MAX_JOB_LOG: usize = 256 << 10;

const JOB_COLS: &str = "id, kind, target, args, state, actor, created_at, started_at, finished_at, exit_code, log";

fn job_row(r: &rusqlite::Row) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        target: r.get(2)?,
        args: r.get(3)?,
        state: r.get(4)?,
        actor: r.get(5)?,
        created_at: r.get(6)?,
        started_at: r.get(7)?,
        finished_at: r.get(8)?,
        exit_code: r.get(9)?,
        log: r.get(10)?,
    })
}

/// Keep the end of a log, cut at a character boundary.
pub fn clip_log(log: &str) -> &str {
    if log.len() <= MAX_JOB_LOG {
        return log;
    }
    let mut start = log.len() - MAX_JOB_LOG;
    while !log.is_char_boundary(start) {
        start += 1;
    }
    &log[start..]
}

impl Tx<'_> {
    pub fn source_get(&self, repo_id: i64) -> Result<Option<SourceRow>> {
        Ok(self
            .c
            .query_row(
                "SELECT r.name, s.spec, s.updated_at, s.updated_by FROM sync_source s JOIN repo r ON r.id = s.repo_id WHERE s.repo_id = ?1",
                params![repo_id],
                |r| Ok(SourceRow { repo: r.get(0)?, spec: r.get(1)?, updated_at: r.get(2)?, updated_by: r.get(3)? }),
            )
            .optional()?)
    }

    pub fn source_set(&self, repo_id: i64, spec: &str, now: i64, actor: &str) -> Result<()> {
        self.c.execute(
            "INSERT INTO sync_source (repo_id, spec, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(repo_id) DO UPDATE SET spec = excluded.spec, updated_at = excluded.updated_at, updated_by = excluded.updated_by",
            params![repo_id, spec, now, actor],
        )?;
        Ok(())
    }

    pub fn job_create(&self, kind: &str, target: &str, args: &str, actor: &str, now: i64) -> Result<i64> {
        self.c.execute(
            "INSERT INTO job (kind, target, args, state, actor, created_at) VALUES (?1, ?2, ?3, 'queued', ?4, ?5)",
            params![kind, target, args, actor, now],
        )?;
        Ok(self.c.last_insert_rowid())
    }

    pub fn job_get(&self, id: i64) -> Result<Option<JobRow>> {
        Ok(self.c.query_row(&format!("SELECT {JOB_COLS} FROM job WHERE id = ?1"), params![id], job_row).optional()?)
    }

    /// Newest first, without logs.
    pub fn job_list(&self, limit: u32) -> Result<Vec<JobRow>> {
        let mut stmt = self.c.prepare(&format!("SELECT {} FROM job ORDER BY id DESC LIMIT ?1", JOB_COLS.replace(", log", ", ''")))?;
        let rows = stmt.query_map(params![limit], job_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The oldest queued job, marked running.
    pub fn job_take(&self, now: i64) -> Result<Option<JobRow>> {
        let next: Option<i64> = self.c.query_row("SELECT id FROM job WHERE state = 'queued' ORDER BY id LIMIT 1", [], |r| r.get(0)).optional()?;
        let Some(id) = next else { return Ok(None) };
        self.c.execute("UPDATE job SET state = 'running', started_at = ?2 WHERE id = ?1", params![id, now])?;
        self.job_get(id)
    }

    pub fn job_log(&self, id: i64, log: &str) -> Result<()> {
        self.c.execute("UPDATE job SET log = ?2 WHERE id = ?1", params![id, clip_log(log)])?;
        Ok(())
    }

    pub fn job_finish(&self, id: i64, state: &str, exit_code: Option<i64>, log: &str, now: i64) -> Result<()> {
        self.c.execute(
            "UPDATE job SET state = ?2, exit_code = ?3, log = ?4, finished_at = ?5 WHERE id = ?1",
            params![id, state, exit_code, clip_log(log), now],
        )?;
        Ok(())
    }

    /// A queued job that has not started can be cancelled here; a running one by its runner.
    pub fn job_cancel_queued(&self, id: i64, now: i64) -> Result<bool> {
        Ok(self.c.execute("UPDATE job SET state = 'cancelled', finished_at = ?2 WHERE id = ?1 AND state = 'queued'", params![id, now])? == 1)
    }

    /// After a restart, jobs that were running did not finish.
    pub fn jobs_interrupt_running(&self, now: i64) -> Result<usize> {
        Ok(self.c.execute("UPDATE job SET state = 'interrupted', finished_at = ?1 WHERE state = 'running'", params![now])?)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleRow {
    pub id: i64,
    /// JSON array of repository names; empty: every repository with a source.
    pub repos: String,
    /// JSON rule, checked by the server.
    pub rule: String,
    pub seal: bool,
    pub enabled: bool,
    pub next_at: Option<i64>,
    pub last_at: Option<i64>,
    /// JSON array of the job numbers the last run queued.
    pub last_jobs: String,
    /// What the last run left out, and why.
    pub last_note: String,
    pub updated_at: i64,
    pub updated_by: String,
}

const SCHEDULE_COLS: &str = "id, repos, rule, seal, enabled, next_at, last_at, last_jobs, last_note, updated_at, updated_by";

fn schedule_row(r: &rusqlite::Row) -> rusqlite::Result<ScheduleRow> {
    Ok(ScheduleRow {
        id: r.get(0)?,
        repos: r.get(1)?,
        rule: r.get(2)?,
        seal: r.get::<_, i64>(3)? != 0,
        enabled: r.get::<_, i64>(4)? != 0,
        next_at: r.get(5)?,
        last_at: r.get(6)?,
        last_jobs: r.get(7)?,
        last_note: r.get(8)?,
        updated_at: r.get(9)?,
        updated_by: r.get(10)?,
    })
}

/// What a schedule is set to; the rest of [`ScheduleRow`] is kept by the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleSpec<'a> {
    pub repos: &'a str,
    pub rule: &'a str,
    pub seal: bool,
    pub enabled: bool,
    pub next_at: Option<i64>,
}

impl Tx<'_> {
    pub fn schedule_list(&self) -> Result<Vec<ScheduleRow>> {
        let mut stmt = self.c.prepare(&format!("SELECT {SCHEDULE_COLS} FROM schedule ORDER BY id"))?;
        let rows = stmt.query_map([], schedule_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn schedule_get(&self, id: i64) -> Result<Option<ScheduleRow>> {
        Ok(self.c.query_row(&format!("SELECT {SCHEDULE_COLS} FROM schedule WHERE id = ?1"), params![id], schedule_row).optional()?)
    }

    pub fn schedule_create(&self, s: &ScheduleSpec, now: i64, actor: &str) -> Result<i64> {
        self.c.execute(
            "INSERT INTO schedule (repos, rule, seal, enabled, next_at, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![s.repos, s.rule, s.seal as i64, s.enabled as i64, s.next_at, now, actor],
        )?;
        Ok(self.c.last_insert_rowid())
    }

    /// Returns whether the schedule exists.
    pub fn schedule_update(&self, id: i64, s: &ScheduleSpec, now: i64, actor: &str) -> Result<bool> {
        Ok(self.c.execute(
            "UPDATE schedule SET repos = ?2, rule = ?3, seal = ?4, enabled = ?5, next_at = ?6, updated_at = ?7, updated_by = ?8 WHERE id = ?1",
            params![id, s.repos, s.rule, s.seal as i64, s.enabled as i64, s.next_at, now, actor],
        )? == 1)
    }

    pub fn schedule_delete(&self, id: i64) -> Result<bool> {
        Ok(self.c.execute("DELETE FROM schedule WHERE id = ?1", params![id])? == 1)
    }

    /// Enabled schedules whose time has come.
    pub fn schedules_due(&self, now: i64) -> Result<Vec<ScheduleRow>> {
        let mut stmt = self.c.prepare(&format!("SELECT {SCHEDULE_COLS} FROM schedule WHERE enabled = 1 AND next_at IS NOT NULL AND next_at <= ?1 ORDER BY id"))?;
        let rows = stmt.query_map(params![now], schedule_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Record a run. `next_at` None leaves the next time as it is (a run by hand).
    pub fn schedule_ran(&self, id: i64, now: i64, next_at: Option<i64>, jobs: &str, note: &str) -> Result<()> {
        self.c.execute(
            "UPDATE schedule SET last_at = ?2, next_at = COALESCE(?3, next_at), last_jobs = ?4, last_note = ?5 WHERE id = ?1",
            params![id, now, next_at, jobs, note],
        )?;
        Ok(())
    }

    /// Whether a job of this kind for this target is waiting or running.
    pub fn job_active(&self, kind: &str, target: &str) -> Result<bool> {
        Ok(self
            .c
            .query_row("SELECT 1 FROM job WHERE kind = ?1 AND target = ?2 AND state IN ('queued', 'running') LIMIT 1", params![kind, target], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Whether the job queued last is of this kind: a seal right behind
    /// another seal adds nothing.
    pub fn job_last_queued_is(&self, kind: &str) -> Result<bool> {
        let last: Option<String> = self.c.query_row("SELECT kind FROM job WHERE state = 'queued' ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).optional()?;
        Ok(last.as_deref() == Some(kind))
    }

    /// Forget finished jobs beyond the newest `keep`. Returns how many went.
    pub fn jobs_prune(&self, keep: u32) -> Result<usize> {
        Ok(self.c.execute(
            "DELETE FROM job WHERE state NOT IN ('queued', 'running') AND id <= (SELECT id FROM job ORDER BY id DESC LIMIT 1 OFFSET ?1)",
            params![keep],
        )?)
    }

    /// Repositories that have a source, by name.
    pub fn sources_list(&self) -> Result<Vec<SourceRow>> {
        let mut stmt = self.c.prepare("SELECT r.name, s.spec, s.updated_at, s.updated_by FROM sync_source s JOIN repo r ON r.id = s.repo_id ORDER BY r.name")?;
        let rows = stmt
            .query_map([], |r| Ok(SourceRow { repo: r.get(0)?, spec: r.get(1)?, updated_at: r.get(2)?, updated_by: r.get(3)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
