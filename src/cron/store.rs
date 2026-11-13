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
ALTER TABLE jobs ADD COLUMN claimed_at INTEGER;
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
    "
CREATE TABLE findings (
    repo_path TEXT NOT NULL,
    name TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    severity TEXT NOT NULL,
    notified_at INTEGER,
    snoozed_until INTEGER,
    last_seen INTEGER NOT NULL,
    PRIMARY KEY (repo_path, name, fingerprint)
);
",
];

/// What is remembered about a problem a job keeps finding, so it is
/// reported once rather than on every run.
#[derive(Debug, Clone, PartialEq)]
pub struct FindingState {
    pub severity: super::run::Severity,
    pub notified_at: Option<DateTime<Utc>>,
    pub snoozed_until: Option<DateTime<Utc>>,
    pub last_seen: DateTime<Utc>,
}

/// A run [`CronStore::reap_stale`] closed because its process was gone.
#[derive(Debug, Clone, PartialEq)]
pub struct Reaped {
    pub run_id: i64,
    /// (container, task id), when the run got as far as starting there.
    pub target: Option<(String, String)>,
}

/// Runs kept per job; older ones are deleted as new ones start.
const RUNS_KEPT: i64 = 200;

/// When this machine last booted (Linux; `None` elsewhere).
fn boot_time() -> Option<i64> {
    std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
}

/// Whether a claim or run made at `since` by `pid` still has its owner. A
/// claim from before the last boot is dead even if its pid number now
/// belongs to another process, which is likely right after a reboot.
pub fn owner_alive(pid: Option<u32>, since: Option<i64>) -> bool {
    let before_boot = matches!((since, boot_time()), (Some(s), Some(b)) if s < b);
    !before_boot && pid.is_some_and(alive)
}

/// Whether process `pid` still exists: a run's owner that died (Ctrl-C,
/// a crash, a stopped service) leaves its claim behind.
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
        self.conn
            .execute("DELETE FROM findings WHERE repo_path = ?1", params![repo])?;
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
        // A job made again under the name starts with no reported findings.
        self.conn.execute(
            "DELETE FROM findings WHERE repo_path = ?1 AND name = ?2",
            params![repo, name],
        )?;
        Ok(())
    }

    /// Drop the state of `repo`'s jobs whose files are gone. `keep` is
    /// every job file present, valid or not. A paused or approved job keeps
    /// its state either way, so one whose file vanishes for a while (a
    /// branch switch) comes back paused and still approved.
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
        let (running, owner, since): (i64, Option<u32>, Option<i64>) = self.conn.query_row(
            "SELECT running, owner_pid, claimed_at FROM jobs WHERE repo_path = ?1 AND name = ?2",
            params![repo, name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let live = running > 0 && owner_alive(owner, since);
        if live && !overlap {
            return Ok(false);
        }
        let count = if live { running + 1 } else { 1 };
        // Compare-and-swap on what was read, so two claimers never both win.
        let n = self.conn.execute(
            "UPDATE jobs SET running = :count, owner_pid = :pid, claimed_at = :now
             WHERE repo_path = :repo AND name = :name
               AND running = :was_running AND owner_pid IS :was_owner",
            rusqlite::named_params! {
                ":count": count,
                ":pid": pid,
                ":now": Utc::now().timestamp(),
                ":repo": repo,
                ":name": name,
                ":was_running": running,
                ":was_owner": owner,
            },
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
    /// Returns each run it closed, with where its session may still be
    /// going inside the container.
    pub fn reap_stale(&self) -> Result<Vec<Reaped>> {
        type Claim = (String, String, Option<u32>, Option<i64>);
        let owners: Vec<Claim> = {
            let mut stmt = self.conn.prepare(
                "SELECT repo_path, name, owner_pid, claimed_at FROM jobs WHERE running > 0",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        for (repo, name, owner, since) in owners {
            if !owner_alive(owner, since) {
                self.conn.execute(
                    "UPDATE jobs SET running = 0, owner_pid = NULL
                     WHERE repo_path = ?1 AND name = ?2 AND owner_pid IS ?3",
                    params![repo, name, owner],
                )?;
            }
        }
        type Open = (i64, Option<u32>, i64, Option<String>, Option<String>);
        let open: Vec<Open> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, owner_pid, started_at, container, task_id FROM runs
                 WHERE status = 'running'",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            rows.collect::<Result<_, _>>()?
        };
        let mut orphans = Vec::new();
        for (id, owner, started, container, task_id) in open {
            if !owner_alive(owner, Some(started)) {
                self.conn.execute(
                    "UPDATE runs SET status = 'failed', finished_at = ?2,
                       error = 'the process running it stopped before it finished'
                     WHERE id = ?1 AND status = 'running'",
                    params![id, Utc::now().timestamp()],
                )?;
                orphans.push(Reaped {
                    run_id: id,
                    target: container.zip(task_id),
                });
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
            "DELETE FROM runs WHERE repo_path = ?1 AND name = ?2 AND status != 'running'
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

    pub fn finding(&self, repo: &str, name: &str, fp: &str) -> Result<Option<FindingState>> {
        self.conn
            .query_row(
                "SELECT severity, notified_at, snoozed_until, last_seen FROM findings
                 WHERE repo_path = ?1 AND name = ?2 AND fingerprint = ?3",
                params![repo, name, fp],
                |r| {
                    let sev: String = r.get(0)?;
                    Ok(FindingState {
                        severity: super::run::Severity::parse(&sev),
                        notified_at: ts(r.get(1)?),
                        snoozed_until: ts(r.get(2)?),
                        last_seen: ts(r.get(3)?).unwrap_or_default(),
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn put_finding(&self, repo: &str, name: &str, fp: &str, f: &FindingState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO findings (repo_path, name, fingerprint, severity, notified_at,
                                   snoozed_until, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (repo_path, name, fingerprint) DO UPDATE SET
               severity = ?4, notified_at = ?5, last_seen = ?7,
               snoozed_until = NULLIF(MAX(COALESCE(snoozed_until, 0), COALESCE(?6, 0)), 0)",
            params![
                repo,
                name,
                fp,
                f.severity.as_str(),
                f.notified_at.map(|t| t.timestamp()),
                f.snoozed_until.map(|t| t.timestamp()),
                f.last_seen.timestamp(),
            ],
        )?;
        Ok(())
    }

    /// Forget the job's findings that its latest run no longer reports, so
    /// a problem that comes back is reported again. Snoozed ones are kept
    /// until their snooze ends.
    /// Only rows last seen before `seen_before` (the run's start) go, so an
    /// older overlapping run cannot erase what a newer one just recorded.
    pub fn retain_findings(
        &self,
        repo: &str,
        name: &str,
        keep: &[String],
        now: DateTime<Utc>,
        seen_before: DateTime<Utc>,
    ) -> Result<()> {
        // One statement, so a snooze set while it runs is honoured.
        let keep = serde_json::to_string(keep)?;
        self.conn.execute(
            "DELETE FROM findings WHERE repo_path = ?1 AND name = ?2
               AND fingerprint NOT IN (SELECT value FROM json_each(?3))
               AND (snoozed_until IS NULL OR snoozed_until <= ?4)
               AND last_seen < ?5",
            params![repo, name, keep, now.timestamp(), seen_before.timestamp()],
        )?;
        Ok(())
    }

    /// Run `f` in one write transaction (`BEGIN IMMEDIATE`): another
    /// process's writes cannot interleave with what `f` reads and writes.
    /// The guard rolls back on an error or a panic, like `db`'s
    /// `with_write_tx`.
    pub fn atomically<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let out = f()?;
        tx.commit()?;
        Ok(out)
    }

    /// Stop reporting these findings until `until`.
    pub fn snooze(
        &self,
        repo: &str,
        name: &str,
        fps: &[String],
        until: DateTime<Utc>,
    ) -> Result<()> {
        for fp in fps {
            let mut f = self.finding(repo, name, fp)?.unwrap_or(FindingState {
                severity: super::run::Severity::Warn,
                notified_at: None,
                snoozed_until: None,
                last_seen: Utc::now(),
            });
            f.snoozed_until = Some(until);
            self.put_finding(repo, name, fp, &f)?;
        }
        Ok(())
    }

    /// A value `nibble cron` keeps about this machine (see [`super::legacy`]).
    pub(crate) fn meta(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()
            .map_err(Into::into)
    }

    /// Set a value read back with [`Self::meta`].
    pub(crate) fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }
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
        // A claim from before the last boot is dead, whatever its pid.
        if boot_time().is_some() {
            assert!(!owner_alive(Some(me), Some(0)));
            assert!(owner_alive(Some(me), Some(Utc::now().timestamp())));
        }
        assert!(!owner_alive(None, None));
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
            [Reaped {
                run_id: b,
                target: Some(("ctr".to_string(), "task-b".to_string()))
            }],
            "the orphan is handed back to be killed"
        );
        assert!(s.reap_stale().unwrap().is_empty(), "once");
        let r = s.run(b).unwrap().unwrap();
        assert_eq!(r.status, RunStatus::Failed);
        assert!(r.error.unwrap().contains("stopped"));
        assert_eq!(s.run(live).unwrap().unwrap().status, RunStatus::Running);
        // Only the newest RUNS_KEPT finished runs per job survive; one still
        // going is never pruned, however many started after it.
        let hung = s.start_run("/r", "c", "C", "schedule", me).unwrap();
        let done = RunEnd {
            status: RunStatus::Ok,
            report: None,
            session_id: None,
            cost_usd: None,
            error: None,
        };
        for _ in 0..RUNS_KEPT + 5 {
            let id = s.start_run("/r", "c", "C", "schedule", me).unwrap();
            s.finish_run(id, &done).unwrap();
        }
        assert_eq!(
            s.runs(Some(("/r", "c")), 1000).unwrap().len() as i64,
            RUNS_KEPT + 1
        );
        assert_eq!(s.run(hung).unwrap().unwrap().status, RunStatus::Running);
        assert_eq!(
            s.runs(Some(("/r", "a")), 10).unwrap().len(),
            1,
            "other jobs kept"
        );
        for st in ["running", "ok", "warn", "critical", "failed", "timeout"] {
            assert_eq!(RunStatus::parse(st).as_str(), st);
        }
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
    fn approved_jobs_survive_a_missing_file() {
        let (_t, s) = store();
        s.set_approved("/r", "a", "def").unwrap();
        s.put_job(&JobState::new("/r", "b")).unwrap();
        s.prune_jobs("/r", &[]).unwrap();
        assert_eq!(s.job("/r", "a").unwrap().approved.as_deref(), Some("def"));
        assert_eq!(s.job("/r", "b").unwrap(), JobState::new("/r", "b"));
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
