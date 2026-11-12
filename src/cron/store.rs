//! This machine's cron state, in `$XDG_STATE_HOME/nibble/cron/cron.db`
//! (default `~/.local/state/nibble/cron`), mode 0700.
//!
//! It is kept out of `~/.nibble` on purpose: every sandbox mounts that
//! directory read-write, and a sandboxed agent must not be able to register
//! another repo, or change when or where jobs run. Job definitions are
//! read from each repo's files on every pass; what is kept here is the
//! snapshot of each definition you approved, to tell when a file changed.

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

/// `$XDG_STATE_HOME/nibble/cron`, next to `nibble rc`'s private state.
pub fn state_dir() -> PathBuf {
    crate::rc::store::state_dir().with_file_name("cron")
}

/// One job's runtime state on this machine.
#[derive(Debug, Clone, PartialEq)]
pub struct JobState {
    pub repo_path: String,
    pub name: String,
    /// The schedule `next_run` was computed from; a changed file
    /// recomputes it.
    pub schedule: Option<String>,
    pub next_run: Option<DateTime<Utc>>,
    pub last_run: Option<DateTime<Utc>>,
    /// Paused here with `nibble cron stop` (the file is untouched).
    pub paused: bool,
    pub running: bool,
    /// The definition you approved on this machine, as [`super::format_job`]
    /// writes it. Only an approved definition runs: job files can be
    /// written from the repo's sandbox.
    pub approved: Option<String>,
}

impl JobState {
    pub fn new(repo_path: &str, name: &str) -> Self {
        JobState {
            repo_path: repo_path.to_string(),
            name: name.to_string(),
            schedule: None,
            next_run: None,
            last_run: None,
            paused: false,
            running: false,
            approved: None,
        }
    }
}

pub struct CronStore {
    conn: Connection,
}

/// Schema changes, in order; `PRAGMA user_version` counts those applied.
/// Append to change the schema, never edit an entry.
const MIGRATIONS: &[&str] = &[
    "
CREATE TABLE repos (
    repo_path TEXT PRIMARY KEY,
    added_at INTEGER NOT NULL
);
CREATE TABLE jobs (
    repo_path TEXT NOT NULL,
    name TEXT NOT NULL,
    schedule TEXT,
    next_run INTEGER,
    last_run INTEGER,
    paused INTEGER NOT NULL DEFAULT 0,
    running INTEGER NOT NULL DEFAULT 0,
    approved TEXT,
    PRIMARY KEY (repo_path, name)
);
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
",
    "
ALTER TABLE jobs ADD COLUMN owner_pid INTEGER;
CREATE TABLE runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_path TEXT NOT NULL,
    name TEXT NOT NULL,
    label TEXT NOT NULL,
    trigger TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    status TEXT NOT NULL,
    summary TEXT,
    report TEXT,
    session_id TEXT,
    cost_usd REAL,
    error TEXT,
    owner_pid INTEGER,
    container TEXT,
    task_id TEXT
);
CREATE INDEX idx_runs_job ON runs(repo_path, name, id);
",
];

/// Runs kept per job; older ones are deleted as new ones start.
const RUNS_KEPT: i64 = 200;

/// Whether process `pid` still exists: a run's owner that died (Ctrl-C,
/// a crash, a stopped service) leaves its claim behind. A reused pid can
/// keep a dead claim alive until that process ends; no worse than a skip.
pub fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; it only checks the process exists.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// How a run ended (or that it has not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    /// The report found nothing wrong.
    Ok,
    Warn,
    Critical,
    /// The run broke before reporting (no sandbox, bad output, ...).
    Failed,
    TimedOut,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Ok => "ok",
            RunStatus::Warn => "warn",
            RunStatus::Critical => "critical",
            RunStatus::Failed => "failed",
            RunStatus::TimedOut => "timeout",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "running" => RunStatus::Running,
            "ok" => RunStatus::Ok,
            "warn" => RunStatus::Warn,
            "critical" => RunStatus::Critical,
            "timeout" => RunStatus::TimedOut,
            _ => RunStatus::Failed,
        }
    }
}

/// One run of a job, as recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub id: i64,
    pub repo_path: String,
    pub name: String,
    pub label: String,
    /// `schedule` or `manual`.
    pub trigger: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: RunStatus,
    pub summary: Option<String>,
    pub report: Option<super::run::Report>,
    /// The run's Claude session, to resume it.
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
    pub error: Option<String>,
}

/// How a finished run is recorded.
pub struct RunEnd<'a> {
    pub status: RunStatus,
    pub report: Option<&'a super::run::Report>,
    pub session_id: Option<&'a str>,
    pub cost_usd: Option<f64>,
    pub error: Option<&'a str>,
}

/// Bring the schema up to date, one migration per transaction.
fn migrate(conn: &mut Connection) -> Result<()> {
    let done: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let done = usize::try_from(done).unwrap_or(0);
    if done > MIGRATIONS.len() {
        anyhow::bail!(
            "cron.db is from a newer nibble (schema {done}, this one knows {}); upgrade nibble",
            MIGRATIONS.len()
        );
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(done) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("cron.db migration {} failed", i + 1))?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

/// Set once the jobs an older nibble kept in `~/.nibble/tasks.db` were
/// written out as files.
const LEGACY_EXPORTED: &str = "legacy_exported";

fn ts(t: Option<i64>) -> Option<DateTime<Utc>> {
    t.and_then(|t| Utc.timestamp_opt(t, 0).single())
}

impl CronStore {
    /// Open (creating, mode 0700) the store in [`state_dir`].
    pub fn open() -> Result<Self> {
        Self::open_in(&state_dir())
    }

    /// [`open`](Self::open) in `dir`.
    pub fn open_in(dir: &Path) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to restrict {}", dir.display()))?;
        let mut conn = Connection::open(dir.join("cron.db")).context("failed to open cron.db")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        migrate(&mut conn)?;
        Ok(CronStore { conn })
    }

    pub fn add_repo(&self, repo: &str) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO repos (repo_path, added_at) VALUES (?1, ?2)",
            params![repo, Utc::now().timestamp()],
        )?;
        Ok(n > 0)
    }

    /// Unregister a repo and drop its jobs' state.
    pub fn remove_repo(&self, repo: &str) -> Result<bool> {
        self.conn
            .execute("DELETE FROM jobs WHERE repo_path = ?1", params![repo])?;
        let n = self
            .conn
            .execute("DELETE FROM repos WHERE repo_path = ?1", params![repo])?;
        Ok(n > 0)
    }

    pub fn repos(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT repo_path FROM repos ORDER BY repo_path")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<Result<_, _>>().map_err(Into::into)
    }

    /// The job's state, or a fresh one if it has none yet.
    pub fn job(&self, repo: &str, name: &str) -> Result<JobState> {
        Ok(self
            .conn
            .query_row(
                "SELECT schedule, next_run, last_run, paused, running, approved
                 FROM jobs WHERE repo_path = ?1 AND name = ?2",
                params![repo, name],
                |r| {
                    Ok(JobState {
                        repo_path: repo.to_string(),
                        name: name.to_string(),
                        schedule: r.get(0)?,
                        next_run: ts(r.get(1)?),
                        last_run: ts(r.get(2)?),
                        paused: r.get::<_, i64>(3)? != 0,
                        running: r.get::<_, i64>(4)? != 0,
                        approved: r.get(5)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_else(|| JobState::new(repo, name)))
    }

    /// Set one column of a job's row, creating the row if needed. Writes go
    /// column by column, so concurrent writers (the daemon's pass, a run
    /// finishing, `nibble cron stop`) never undo each other.
    fn set(&self, repo: &str, name: &str, column: &str, value: &dyn rusqlite::ToSql) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO jobs (repo_path, name) VALUES (?1, ?2)",
            params![repo, name],
        )?;
        self.conn.execute(
            &format!("UPDATE jobs SET {column} = ?3 WHERE repo_path = ?1 AND name = ?2"),
            params![repo, name, value],
        )?;
        Ok(())
    }

    /// Record the schedule `next_run` was computed from, and the time.
    pub fn set_schedule(
        &self,
        repo: &str,
        name: &str,
        schedule: &str,
        next: DateTime<Utc>,
    ) -> Result<()> {
        self.set(repo, name, "schedule", &schedule)?;
        self.set_next_run(repo, name, next)
    }

    pub fn set_next_run(&self, repo: &str, name: &str, next: DateTime<Utc>) -> Result<()> {
        self.set(repo, name, "next_run", &next.timestamp())
    }

    pub fn set_last_run(&self, repo: &str, name: &str, at: DateTime<Utc>) -> Result<()> {
        self.set(repo, name, "last_run", &at.timestamp())
    }

    pub fn set_paused(&self, repo: &str, name: &str, paused: bool) -> Result<()> {
        self.set(repo, name, "paused", &(paused as i64))
    }

    /// Approve a definition (its [`super::format_job`] text) to run here.
    pub fn set_approved(&self, repo: &str, name: &str, text: &str) -> Result<()> {
        self.set(repo, name, "approved", &text)
    }

    #[cfg(test)]
    pub fn put_job(&self, s: &JobState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO jobs (repo_path, name, schedule, next_run, last_run, paused, running,
                               approved)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (repo_path, name) DO UPDATE SET
               schedule = ?3, next_run = ?4, last_run = ?5, paused = ?6, running = ?7,
               approved = ?8",
            params![
                s.repo_path,
                s.name,
                s.schedule,
                s.next_run.map(|t| t.timestamp()),
                s.last_run.map(|t| t.timestamp()),
                s.paused as i64,
                s.running as i64,
                s.approved,
            ],
        )?;
        Ok(())
    }

    pub fn delete_job(&self, repo: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM jobs WHERE repo_path = ?1 AND name = ?2",
            params![repo, name],
        )?;
        Ok(())
    }

    /// Drop the state of `repo`'s jobs whose files are gone. `keep` is
    /// every job file present, valid or not. A paused job keeps its state
    /// either way: a file that vanishes for a while (a branch switch) must
    /// not come back unpaused.
    pub fn prune_jobs(&self, repo: &str, keep: &[String]) -> Result<()> {
        // A paused or approved job keeps its row: a file that is away for a
        // while (a branch switch) must come back paused and approved, and a
        // returning file is still compared against the approval.
        let mut stmt = self.conn.prepare(
            "SELECT name FROM jobs WHERE repo_path = ?1 AND paused = 0 AND approved IS NULL",
        )?;
        let names: Vec<String> = stmt
            .query_map(params![repo], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for name in names.iter().filter(|n| !keep.contains(n)) {
            self.delete_job(repo, name)?;
        }
        Ok(())
    }

    /// Claim the job for a run by process `pid`. False when a live run
    /// holds it and overlapping is not allowed. A claim whose owner died is
    /// taken over. Compare-and-swap, so two claimers never both win.
    /// `running` counts the job's runs in flight, so with overlapping runs
    /// the first to finish does not clear the others'.
    pub fn claim(&self, repo: &str, name: &str, pid: u32, overlap: bool) -> Result<bool> {
        self.conn.execute(
            "INSERT OR IGNORE INTO jobs (repo_path, name) VALUES (?1, ?2)",
            params![repo, name],
        )?;
        let (running, owner): (i64, Option<u32>) = self.conn.query_row(
            "SELECT running, owner_pid FROM jobs WHERE repo_path = ?1 AND name = ?2",
            params![repo, name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let live = running > 0 && owner.is_some_and(alive);
        if live && !overlap {
            return Ok(false);
        }
        let count = if live { running + 1 } else { 1 };
        let n = self.conn.execute(
            "UPDATE jobs SET running = ?6, owner_pid = ?3
             WHERE repo_path = ?1 AND name = ?2 AND running = ?4 AND owner_pid IS ?5",
            params![repo, name, pid, running, owner, count],
        )?;
        Ok(n == 1)
    }

    /// Runs claimed across all jobs (by the daemon or `nibble cron run`).
    pub fn runs_in_flight(&self) -> Result<usize> {
        let n: i64 =
            self.conn
                .query_row("SELECT COALESCE(SUM(running), 0) FROM jobs", [], |r| {
                    r.get(0)
                })?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// End one run's claim; the job is free once none is left.
    pub fn release(&self, repo: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE jobs SET running = MAX(running - 1, 0),
               owner_pid = CASE WHEN running <= 1 THEN NULL ELSE owner_pid END
             WHERE repo_path = ?1 AND name = ?2 AND running > 0",
            params![repo, name],
        )?;
        Ok(())
    }

    /// Clear claims and close runs whose owning process is gone. Run by the
    /// daemon at start and on every pass, so a run killed with Ctrl-C does
    /// not block its job for good, while a live manual run is left alone.
    /// Returns the (container, task id) of each closed run, whose session
    /// may still be going inside the container.
    pub fn reap_stale(&self) -> Result<Vec<(String, String)>> {
        let owners: Vec<(String, String, Option<u32>)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT repo_path, name, owner_pid FROM jobs WHERE running > 0")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        for (repo, name, owner) in owners {
            if !owner.is_some_and(alive) {
                self.conn.execute(
                    "UPDATE jobs SET running = 0, owner_pid = NULL
                     WHERE repo_path = ?1 AND name = ?2 AND owner_pid IS ?3",
                    params![repo, name, owner],
                )?;
            }
        }
        type Open = (i64, Option<u32>, Option<String>, Option<String>);
        let open: Vec<Open> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, owner_pid, container, task_id FROM runs WHERE status = 'running'",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut orphans = Vec::new();
        for (id, owner, container, task_id) in open {
            if !owner.is_some_and(alive) {
                self.conn.execute(
                    "UPDATE runs SET status = 'failed', finished_at = ?2,
                       error = 'the process running it stopped before it finished'
                     WHERE id = ?1 AND status = 'running'",
                    params![id, Utc::now().timestamp()],
                )?;
                if let (Some(c), Some(t)) = (container, task_id) {
                    orphans.push((c, t));
                }
            }
        }
        Ok(orphans)
    }

    /// Where a run executes, so a run orphaned by its process dying can be
    /// found and stopped inside the container.
    pub fn set_run_target(&self, id: i64, container: &str, task_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET container = ?2, task_id = ?3 WHERE id = ?1",
            params![id, container, task_id],
        )?;
        Ok(())
    }

    /// Record a run starting in process `pid`; returns its id. Old runs of
    /// the job beyond the kept number are deleted.
    pub fn start_run(
        &self,
        repo: &str,
        name: &str,
        label: &str,
        trigger: &str,
        pid: u32,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs (repo_path, name, label, trigger, started_at, status, owner_pid)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6)",
            params![repo, name, label, trigger, Utc::now().timestamp(), pid],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "DELETE FROM runs WHERE repo_path = ?1 AND name = ?2
               AND id NOT IN (SELECT id FROM runs WHERE repo_path = ?1 AND name = ?2
                              ORDER BY id DESC LIMIT ?3)",
            params![repo, name, RUNS_KEPT],
        )?;
        Ok(id)
    }

    pub fn finish_run(&self, id: i64, end: &RunEnd) -> Result<()> {
        let report = end.report.map(serde_json::to_string).transpose()?;
        self.conn.execute(
            "UPDATE runs SET finished_at = ?2, status = ?3, summary = ?4, report = ?5,
               session_id = ?6, cost_usd = ?7, error = ?8
             WHERE id = ?1",
            params![
                id,
                Utc::now().timestamp(),
                end.status.as_str(),
                end.report.map(|r| r.summary.as_str()),
                report,
                end.session_id,
                end.cost_usd,
                end.error,
            ],
        )?;
        Ok(())
    }

    /// The columns [`Self::row_to_run`] reads, in its order.
    const RUN_COLUMNS: &'static str = "id, repo_path, name, label, trigger, started_at, \
        finished_at, status, summary, report, session_id, cost_usd, error";

    fn row_to_run(r: &rusqlite::Row) -> rusqlite::Result<RunRecord> {
        let report: Option<String> = r.get(9)?;
        Ok(RunRecord {
            id: r.get(0)?,
            repo_path: r.get(1)?,
            name: r.get(2)?,
            label: r.get(3)?,
            trigger: r.get(4)?,
            started_at: ts(r.get(5)?).unwrap_or_default(),
            finished_at: ts(r.get(6)?),
            status: RunStatus::parse(&r.get::<_, String>(7)?),
            summary: r.get(8)?,
            // A report that no longer parses is shown as missing.
            report: report.and_then(|j| serde_json::from_str(&j).ok()),
            session_id: r.get(10)?,
            cost_usd: r.get(11)?,
            error: r.get(12)?,
        })
    }

    /// The newest runs first; of one job when `job` is given.
    pub fn runs(&self, job: Option<(&str, &str)>, limit: usize) -> Result<Vec<RunRecord>> {
        let (repo, name) = job.unwrap_or_default();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM runs WHERE ?1 = 0 OR (repo_path = ?2 AND name = ?3)
             ORDER BY id DESC LIMIT ?4",
            Self::RUN_COLUMNS
        ))?;
        let rows = stmt.query_map(
            params![job.is_some() as i64, repo, name, limit as i64],
            Self::row_to_run,
        )?;
        rows.collect::<Result<_, _>>().map_err(Into::into)
    }

    pub fn run(&self, id: i64) -> Result<Option<RunRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {} FROM runs WHERE id = ?1", Self::RUN_COLUMNS),
                params![id],
                Self::row_to_run,
            )
            .optional()
            .map_err(Into::into)
    }

    fn meta(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()
            .map_err(Into::into)
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }
}

/// What `nibble cron import` would do with one job an older nibble kept
/// in `~/.nibble/tasks.db`.
#[derive(Debug, PartialEq)]
pub enum LegacyItem {
    /// Write `def` to `path` and register `repo`.
    Import {
        repo: PathBuf,
        path: PathBuf,
        def: super::JobDef,
        /// The legacy row, remembered once imported.
        row: i64,
    },
    Skip {
        repo_path: String,
        label: String,
        reason: String,
    },
}

/// Whether `nibble cron import` already ran on this machine.
#[cfg(test)]
pub fn legacy_done(store: &CronStore) -> Result<bool> {
    Ok(store.meta(LEGACY_EXPORTED)?.is_some())
}

/// Whether the jobs in `~/.nibble/tasks.db` are still to be imported.
pub fn legacy_pending(store: &CronStore, db: &crate::db::Database) -> Result<bool> {
    Ok(store.meta(LEGACY_EXPORTED)?.is_none() && db.has_cron_jobs()?)
}

/// Plan the import of older nibble's jobs. Nothing is written: the rows
/// come from `~/.nibble`, which sandboxes can write, so the user reviews
/// the plan first. Each job gets a file name no other file or planned job
/// has.
pub fn plan_legacy(store: &CronStore, db: &crate::db::Database) -> Result<Vec<LegacyItem>> {
    let mut taken: Vec<PathBuf> = Vec::new();
    let mut plan = Vec::new();
    // Oldest first, so the original of two same-named jobs keeps the name.
    for job in db.list_cron_jobs(None)?.into_iter().rev() {
        let row = job.id.unwrap_or_default();
        let label = job
            .label
            .clone()
            .map(|l| l.chars().filter(|c| !c.is_control()).collect::<String>())
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| "job".to_string());
        let skip = |reason: String| LegacyItem::Skip {
            repo_path: job.repo_path.clone(),
            label: label.clone(),
            reason,
        };
        // Imported before, even if its file was edited since.
        if store.meta(&imported_key(row))?.is_some() {
            plan.push(skip("already imported".to_string()));
            continue;
        }
        let repo = match super::validate_repo(Path::new(&job.repo_path), true) {
            Ok(r) => r,
            Err(e) => {
                plan.push(skip(format!("{e:#}")));
                continue;
            }
        };
        if job.prompt.trim().is_empty() || super::validate_schedule(&job.schedule).is_err() {
            plan.push(skip("empty prompt or invalid schedule".to_string()));
            continue;
        }
        let base = super::slug(&label);
        let mut wanted = super::JobDef {
            name: base.clone(),
            label: label.clone(),
            schedule: job.schedule.clone(),
            enabled: job.enabled,
            skip_if_running: job.skip_if_running,
            expires_at: job.expires_at,
            timeout: super::DEFAULT_TIMEOUT,
            model: None,
            env: vec![],
            notify: super::Notify::Slack,
            // Old jobs were plain prompts that could edit the repo.
            read_only: false,
            prompt: job.prompt.clone(),
        };
        // As the file will read back (titles and prompts are trimmed), so
        // what is approved is exactly what is on disk.
        if let Ok(p) = super::parse_job(&base, &super::format_job(&wanted)) {
            wanted = p.def;
        }
        // Walk base, base-2, ... once: a file holding this very job means a
        // retry after a partial import; the first free name is taken.
        let mut free = None;
        for n in 1.. {
            let name = if n == 1 {
                base.clone()
            } else {
                // Shortened so the suffix keeps the name within 64.
                let suffix = format!("-{n}");
                let keep: String = base.chars().take(64 - suffix.len()).collect();
                format!("{}{suffix}", keep.trim_end_matches('-'))
            };
            let path = super::job_path(&repo, &name);
            let mut d = wanted.clone();
            d.name = name.clone();
            if std::fs::read_to_string(&path).is_ok_and(|t| t == super::format_job(&d)) {
                break;
            }
            if super::nothing_at(&path) && !taken.contains(&path) {
                free = Some((name, path));
                break;
            }
        }
        let Some((name, path)) = free else {
            plan.push(skip("already imported".to_string()));
            continue;
        };
        taken.push(path.clone());
        wanted.name = name;
        plan.push(LegacyItem::Import {
            repo,
            path,
            def: wanted,
            row,
        });
    }
    Ok(plan)
}

/// Carry out a reviewed plan: write the files, register the repos, and
/// remember that the import happened.
pub fn apply_legacy(store: &CronStore, plan: &[LegacyItem]) -> Result<()> {
    for item in plan {
        if let LegacyItem::Import {
            repo,
            path,
            def,
            row,
        } = item
        {
            let key = repo.to_string_lossy();
            let text = super::format_job(def);
            // The plan was made before you were asked: a file that appeared
            // since is not replaced.
            if !super::nothing_at(path) {
                eprintln!("  skipped {}: it appeared after the plan", path.display());
                continue;
            }
            super::real_jobs_dir(repo)?;
            std::fs::write(path, &text)
                .with_context(|| format!("failed to write {}", path.display()))?;
            store.add_repo(&key)?;
            // You reviewed it in the plan.
            store.set_approved(&key, &def.name, &text)?;
            store.set_meta(&imported_key(*row), &path.to_string_lossy())?;
        }
    }
    mark_legacy_done(store)
}

fn imported_key(row: i64) -> String {
    format!("legacy_imported:{row}")
}

/// Remember that older nibble's jobs were dealt with.
pub fn mark_legacy_done(store: &CronStore) -> Result<()> {
    store.set_meta(LEGACY_EXPORTED, &Utc::now().to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, CronStore) {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = CronStore::open_in(&tmp.path().join("cron")).unwrap();
        (tmp, s)
    }

    #[test]
    fn dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (tmp, _s) = store();
        let mode = std::fs::metadata(tmp.path().join("cron"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn repos_register_once_and_forget_drops_jobs() {
        let (_t, s) = store();
        assert!(s.add_repo("/r").unwrap());
        assert!(!s.add_repo("/r").unwrap());
        s.add_repo("/a").unwrap();
        assert_eq!(s.repos().unwrap(), ["/a", "/r"]);
        let mut j = s.job("/r", "x").unwrap();
        j.paused = true;
        s.put_job(&j).unwrap();
        assert!(s.remove_repo("/r").unwrap());
        assert!(
            !s.job("/r", "x").unwrap().paused,
            "state went with the repo"
        );
        assert!(!s.remove_repo("/r").unwrap());
    }

    #[test]
    fn job_state_round_trips_and_prunes() {
        let (_t, s) = store();
        let fresh = s.job("/r", "a").unwrap();
        assert_eq!(fresh, JobState::new("/r", "a"));
        let t = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        let j = JobState {
            schedule: Some("0 9 * * *".into()),
            next_run: Some(t),
            last_run: Some(t),
            paused: false,
            running: true,
            ..fresh
        };
        s.put_job(&j).unwrap();
        assert_eq!(s.job("/r", "a").unwrap(), j);
        s.put_job(&JobState::new("/r", "b")).unwrap();
        s.prune_jobs("/r", &["b".to_string()]).unwrap();
        assert_eq!(s.job("/r", "a").unwrap(), JobState::new("/r", "a"));
        assert!(
            s.conn
                .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
                .unwrap()
                == 1
        );
    }

    fn report(status: super::super::run::Severity) -> super::super::run::Report {
        super::super::run::Report {
            status,
            summary: "sum".into(),
            findings: vec![],
        }
    }

    /// A pid that is certainly not a live process.
    const DEAD: u32 = u32::MAX - 1;

    #[test]
    fn claims_are_exclusive_and_dead_owners_lose_them() {
        let (_t, s) = store();
        let me = std::process::id();
        assert!(s.claim("/r", "a", me, false).unwrap());
        assert!(
            !s.claim("/r", "a", me, false).unwrap(),
            "held by a live owner"
        );
        assert!(s.claim("/r", "a", me, true).unwrap(), "overlap allowed");
        assert!(s.job("/r", "a").unwrap().running);
        s.release("/r", "a").unwrap();
        assert!(
            s.job("/r", "a").unwrap().running,
            "the overlapping run still holds it"
        );
        s.release("/r", "a").unwrap();
        assert!(!s.job("/r", "a").unwrap().running);
        s.release("/r", "a").unwrap();
        assert!(!s.job("/r", "a").unwrap().running, "no underflow");
        assert!(s.claim("/r", "x", me, false).unwrap());
        assert!(s.claim("/r", "y", me, false).unwrap());
        assert_eq!(s.runs_in_flight().unwrap(), 2, "counts every job's claims");
        // A dead owner's claim is taken over, and reaped.
        assert!(s.claim("/r", "b", DEAD, false).unwrap());
        assert!(s.claim("/r", "b", me, false).unwrap());
        assert!(s.claim("/r", "c", DEAD, false).unwrap());
        s.reap_stale().unwrap();
        assert!(!s.job("/r", "c").unwrap().running);
        assert!(s.job("/r", "b").unwrap().running, "a live claim stays");
        assert!(alive(me) && !alive(DEAD));
    }

    #[test]
    fn runs_are_recorded_listed_and_pruned() {
        let (_t, s) = store();
        let me = std::process::id();
        let a = s.start_run("/r", "a", "A", "manual", me).unwrap();
        let b = s.start_run("/r", "b", "B", "schedule", DEAD).unwrap();
        let r = s.run(a).unwrap().unwrap();
        assert_eq!((r.status, r.finished_at), (RunStatus::Running, None));
        let rep = report(super::super::run::Severity::Warn);
        s.finish_run(
            a,
            &RunEnd {
                status: RunStatus::Warn,
                report: Some(&rep),
                session_id: Some("sid"),
                cost_usd: Some(0.5),
                error: None,
            },
        )
        .unwrap();
        let r = s.run(a).unwrap().unwrap();
        assert_eq!(r.status, RunStatus::Warn);
        assert_eq!(r.report, Some(rep));
        assert_eq!(r.summary.as_deref(), Some("sum"));
        assert_eq!(r.session_id.as_deref(), Some("sid"));
        assert!(r.finished_at.is_some());
        let all: Vec<i64> = s.runs(None, 10).unwrap().iter().map(|r| r.id).collect();
        assert_eq!(all, [b, a], "newest first");
        assert_eq!(s.runs(Some(("/r", "a")), 10).unwrap().len(), 1);
        assert!(s.run(999).unwrap().is_none());
        // A run whose process died is closed; a live one is not.
        let live = s.start_run("/r", "d", "D", "manual", me).unwrap();
        s.set_run_target(b, "ctr", "task-b").unwrap();
        assert_eq!(
            s.reap_stale().unwrap(),
            [("ctr".to_string(), "task-b".to_string())],
            "the orphan is handed back to be killed"
        );
        assert!(s.reap_stale().unwrap().is_empty(), "once");
        let r = s.run(b).unwrap().unwrap();
        assert_eq!(r.status, RunStatus::Failed);
        assert!(r.error.unwrap().contains("stopped"));
        assert_eq!(s.run(live).unwrap().unwrap().status, RunStatus::Running);
        // Only the newest RUNS_KEPT per job survive.
        for _ in 0..RUNS_KEPT + 5 {
            s.start_run("/r", "c", "C", "schedule", me).unwrap();
        }
        assert_eq!(
            s.runs(Some(("/r", "c")), 1000).unwrap().len() as i64,
            RUNS_KEPT
        );
        assert_eq!(
            s.runs(Some(("/r", "a")), 10).unwrap().len(),
            1,
            "other jobs kept"
        );
        for st in ["running", "ok", "warn", "critical", "failed", "timeout"] {
            assert_eq!(RunStatus::parse(st).as_str(), st);
        }
    }

    fn git_repo(tmp: &Path, name: &str) -> PathBuf {
        let repo = tmp.join(name);
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::canonicalize(repo).unwrap()
    }

    #[test]
    fn legacy_jobs_are_planned_then_imported() {
        let (tmp, s) = store();
        let repo = git_repo(tmp.path(), "repo");
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let db = crate::db::Database::open(tmp.path().join("tasks.db")).unwrap();
        let add = |repo: &str, label: Option<&str>, enabled: bool| {
            let mut job = crate::models::CronJob::new(
                repo.to_string(),
                "0 9 * * *".into(),
                "Do the thing.".into(),
                label.map(str::to_string),
                Utc::now(),
            );
            job.enabled = enabled;
            db.insert_cron_job(&job).unwrap();
        };
        let r = repo.to_string_lossy().to_string();
        assert!(
            !legacy_pending(&s, &db).unwrap(),
            "no rows: nothing pending"
        );
        add(&r, Some("Malone Daily Tasks"), false);
        add(&r, Some("malone daily tasks!"), true); // same slug
        add(&r, None, true);
        add(&format!("{r}/../repo"), Some("Line\nbreak"), true); // not canonical
        add("/gone", None, true);
        add(&plain.to_string_lossy(), None, true); // not a git repo
                                                   // An existing file is never replaced.
        std::fs::create_dir_all(super::super::jobs_dir(&repo)).unwrap();
        std::fs::write(super::super::job_path(&repo, "job"), "mine").unwrap();
        assert!(legacy_pending(&s, &db).unwrap());

        let plan = plan_legacy(&s, &db).unwrap();
        let mut imported = Vec::new();
        let mut skipped = 0;
        for item in &plan {
            match item {
                LegacyItem::Import {
                    repo: r2,
                    path,
                    def,
                    ..
                } => {
                    assert_eq!(r2, &repo, "registered by its canonical path");
                    imported.push((
                        path.file_name().unwrap().to_string_lossy().to_string(),
                        def.clone(),
                    ));
                }
                LegacyItem::Skip { .. } => skipped += 1,
            }
        }
        let mut names: Vec<&str> = imported.iter().map(|(n, _)| n.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "job-2.md",
                "linebreak.md",
                "malone-daily-tasks-2.md",
                "malone-daily-tasks.md"
            ]
        );
        assert_eq!(skipped, 2, "{plan:?}");
        assert!(s.repos().unwrap().is_empty(), "planning writes nothing");

        apply_legacy(&s, &plan).unwrap();
        assert_eq!(
            std::fs::read_to_string(super::super::job_path(&repo, "job")).unwrap(),
            "mine"
        );
        let def = super::super::load_job_file(&super::super::job_path(&repo, "malone-daily-tasks"))
            .unwrap();
        assert!(!def.enabled && !def.read_only);
        assert_eq!(def.prompt, "Do the thing.");
        assert_eq!(s.repos().unwrap(), [r]);
        assert!(!legacy_pending(&s, &db).unwrap(), "done once");
    }

    #[test]
    fn schema_is_versioned() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("c");
        drop(CronStore::open_in(&dir).unwrap());
        let conn = Connection::open(dir.join("cron.db")).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v as usize, MIGRATIONS.len());
        // Reopening applies nothing twice.
        drop(CronStore::open_in(&dir).unwrap());
        conn.pragma_update(None, "user_version", 99).unwrap();
        let err = CronStore::open_in(&dir).err().unwrap().to_string();
        assert!(err.contains("newer nibble"), "{err}");
    }

    #[test]
    fn a_retried_import_skips_what_it_wrote() {
        let (tmp, s) = store();
        let repo = git_repo(tmp.path(), "repo");
        let db = crate::db::Database::open(tmp.path().join("tasks.db")).unwrap();
        db.insert_cron_job(&crate::models::CronJob::new(
            repo.to_string_lossy().to_string(),
            "0 9 * * *".into(),
            "Go".into(),
            Some("Daily".into()),
            Utc::now(),
        ))
        .unwrap();
        let plan = plan_legacy(&s, &db).unwrap();
        apply_legacy(&s, &plan).unwrap();
        let again = plan_legacy(&s, &db).unwrap();
        assert!(
            matches!(&again[..], [LegacyItem::Skip { reason, .. }] if reason == "already imported"),
            "{again:?}"
        );
        // A file appearing between plan and apply is not replaced.
        std::fs::remove_file(super::super::job_path(&repo, "daily")).unwrap();
        let plan = plan_legacy(&s, &db).unwrap();
        std::fs::write(super::super::job_path(&repo, "daily"), "theirs").unwrap();
        apply_legacy(&s, &plan).unwrap();
        assert_eq!(
            std::fs::read_to_string(super::super::job_path(&repo, "daily")).unwrap(),
            "theirs"
        );
        assert!(legacy_done(&s).unwrap());
    }

    #[test]
    fn an_edited_import_is_not_imported_again() {
        let (tmp, s) = store();
        let repo = git_repo(tmp.path(), "repo");
        let db = crate::db::Database::open(tmp.path().join("tasks.db")).unwrap();
        db.insert_cron_job(&crate::models::CronJob::new(
            repo.to_string_lossy().to_string(),
            "0 9 * * *".into(),
            "Go".into(),
            Some("Daily".into()),
            Utc::now(),
        ))
        .unwrap();
        apply_legacy(&s, &plan_legacy(&s, &db).unwrap()).unwrap();
        let path = super::super::job_path(&repo, "daily");
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("0 9", "0 8");
        std::fs::write(&path, edited).unwrap();
        let again = plan_legacy(&s, &db).unwrap();
        assert!(
            matches!(&again[..], [LegacyItem::Skip { reason, .. }] if reason == "already imported")
        );
        assert!(!super::super::job_path(&repo, "daily-2").exists());
        // A file appearing between plan and apply is not replaced.
        let (_t2, fresh) = store();
        let plan = plan_legacy(&fresh, &db).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "theirs").unwrap();
        apply_legacy(&fresh, &plan).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs");
    }

    #[test]
    fn numbered_names_stay_valid() {
        let (tmp, s) = store();
        let repo = git_repo(tmp.path(), "repo");
        let db = crate::db::Database::open(tmp.path().join("tasks.db")).unwrap();
        let long = "x".repeat(80);
        for p in ["a", "b"] {
            db.insert_cron_job(&crate::models::CronJob::new(
                repo.to_string_lossy().to_string(),
                "0 9 * * *".into(),
                p.into(),
                Some(long.clone()),
                Utc::now(),
            ))
            .unwrap();
        }
        apply_legacy(&s, &plan_legacy(&s, &db).unwrap()).unwrap();
        let (jobs, bad) = super::super::load_repo_jobs(&repo).unwrap();
        assert_eq!((jobs.len(), bad.len()), (2, 0), "{bad:?}");
    }

    #[test]
    fn a_bad_legacy_time_does_not_panic() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::open(tmp.path().join("tasks.db")).unwrap();
        let job = crate::models::CronJob::new(
            "/r".into(),
            "0 9 * * *".into(),
            "p".into(),
            None,
            Utc::now(),
        );
        db.insert_cron_job(&job).unwrap();
        db.set_cron_next_run_for_test(i64::MAX).unwrap();
        assert!(db.has_cron_jobs().unwrap());
        assert_eq!(db.list_cron_jobs(None).unwrap().len(), 1);
    }

    #[test]
    fn approved_jobs_survive_a_missing_file() {
        let (_t, s) = store();
        s.set_approved("/r", "a", "def").unwrap();
        s.put_job(&JobState::new("/r", "b")).unwrap();
        s.prune_jobs("/r", &[]).unwrap();
        assert_eq!(s.job("/r", "a").unwrap().approved.as_deref(), Some("def"));
        assert_eq!(s.job("/r", "b").unwrap(), JobState::new("/r", "b"));
    }

    #[test]
    fn imports_match_what_reads_back_and_any_earlier_copy() {
        let (tmp, s) = store();
        let repo = git_repo(tmp.path(), "repo");
        let db = crate::db::Database::open(tmp.path().join("tasks.db")).unwrap();
        // Untrimmed values, as older nibble stored them.
        db.insert_cron_job(&crate::models::CronJob::new(
            repo.to_string_lossy().to_string(),
            "0 9 * * *".into(),
            "  Audit deps \n".into(),
            Some(" Deps ".into()),
            Utc::now(),
        ))
        .unwrap();
        // Ten unrelated files with the same slug sit before the free name.
        std::fs::create_dir_all(super::super::jobs_dir(&repo)).unwrap();
        std::fs::write(super::super::job_path(&repo, "deps"), "other").unwrap();
        for n in 2..=11 {
            std::fs::write(super::super::job_path(&repo, &format!("deps-{n}")), "other").unwrap();
        }
        let plan = plan_legacy(&s, &db).unwrap();
        apply_legacy(&s, &plan).unwrap();
        let path = super::super::job_path(&repo, "deps-12");
        let def = super::super::load_job_file(&path).unwrap();
        let key = repo.to_string_lossy().to_string();
        assert_eq!(
            s.job(&key, "deps-12").unwrap().approved,
            Some(super::super::format_job(&def)),
            "approved exactly as it reads back"
        );
        let again = plan_legacy(&s, &db).unwrap();
        assert!(
            matches!(&again[..], [LegacyItem::Skip { reason, .. }] if reason == "already imported")
        );
    }

    #[test]
    fn paused_and_present_jobs_survive_pruning() {
        let (_t, s) = store();
        let mut p = JobState::new("/r", "paused");
        p.paused = true;
        s.put_job(&p).unwrap();
        s.put_job(&JobState::new("/r", "gone")).unwrap();
        s.put_job(&JobState::new("/r", "broken")).unwrap();
        s.prune_jobs("/r", &["broken".to_string()]).unwrap();
        assert!(s.job("/r", "paused").unwrap().paused);
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "only the unpaused job without a file went");
    }
}
