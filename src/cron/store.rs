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
const MIGRATIONS: &[&str] = &["
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
"];

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
