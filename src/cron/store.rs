//! This machine's cron state, in `$XDG_STATE_HOME/nibble/cron/cron.db`
//! (default `~/.local/state/nibble/cron`), mode 0700.
//!
//! It is kept out of `~/.nibble` on purpose: every sandbox mounts that
//! directory read-write, and a sandboxed agent must not be able to register
//! another repo, or change when or where jobs run. Job definitions are not
//! stored here; they are read from each repo's files on every pass.

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
        }
    }
}

pub struct CronStore {
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS repos (
    repo_path TEXT PRIMARY KEY,
    added_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS jobs (
    repo_path TEXT NOT NULL,
    name TEXT NOT NULL,
    schedule TEXT,
    next_run INTEGER,
    last_run INTEGER,
    paused INTEGER NOT NULL DEFAULT 0,
    running INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (repo_path, name)
);
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

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
        let conn = Connection::open(dir.join("cron.db")).context("failed to open cron.db")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)
            .context("failed to create the cron tables")?;
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
                "SELECT schedule, next_run, last_run, paused, running
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
                    })
                },
            )
            .optional()?
            .unwrap_or_else(|| JobState::new(repo, name)))
    }

    pub fn put_job(&self, s: &JobState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO jobs (repo_path, name, schedule, next_run, last_run, paused, running)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (repo_path, name) DO UPDATE SET
               schedule = ?3, next_run = ?4, last_run = ?5, paused = ?6, running = ?7",
            params![
                s.repo_path,
                s.name,
                s.schedule,
                s.next_run.map(|t| t.timestamp()),
                s.last_run.map(|t| t.timestamp()),
                s.paused as i64,
                s.running as i64,
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

    /// Drop the state of `repo`'s jobs whose files are gone.
    pub fn prune_jobs(&self, repo: &str, keep: &[String]) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM jobs WHERE repo_path = ?1")?;
        let names: Vec<String> = stmt
            .query_map(params![repo], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for name in names.iter().filter(|n| !keep.contains(n)) {
            self.delete_job(repo, name)?;
        }
        Ok(())
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

/// Write the jobs an older nibble stored in `~/.nibble/tasks.db` out as job
/// files in their repos, once, and register those repos. A file that
/// already exists is left alone. Returns what was written, for the caller
/// to report.
pub fn export_legacy(store: &CronStore, db: &crate::db::Database) -> Result<Vec<PathBuf>> {
    if store.meta(LEGACY_EXPORTED)?.is_some() {
        return Ok(vec![]);
    }
    let mut written = Vec::new();
    for job in db.list_cron_jobs(None)? {
        let repo = Path::new(&job.repo_path);
        if !repo.is_dir() {
            continue;
        }
        let label = job.label.clone().unwrap_or_else(|| "job".to_string());
        let def = super::JobDef {
            name: super::slug(&label),
            label,
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
        let path = super::job_path(repo, &def.name);
        store.add_repo(&job.repo_path)?;
        if path.exists() {
            continue;
        }
        std::fs::create_dir_all(super::jobs_dir(repo))?;
        std::fs::write(&path, super::format_job(&def))
            .with_context(|| format!("failed to write {}", path.display()))?;
        written.push(path);
    }
    store.set_meta(LEGACY_EXPORTED, &Utc::now().to_rfc3339())?;
    Ok(written)
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
            paused: true,
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
    fn legacy_jobs_become_files_once() {
        let (tmp, s) = store();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let repo_s = repo.to_string_lossy().to_string();
        let db = crate::db::Database::open(tmp.path().join("tasks.db")).unwrap();
        let next = Utc::now();
        let mut job = crate::models::CronJob::new(
            repo_s.clone(),
            "0 9 * * *".into(),
            "Do the thing.".into(),
            Some("Malone Daily Tasks".into()),
            next,
        );
        job.enabled = false;
        db.insert_cron_job(&job).unwrap();
        // A job for a repo that no longer exists is skipped.
        db.insert_cron_job(&crate::models::CronJob::new(
            "/gone".into(),
            "0 9 * * *".into(),
            "x".into(),
            None,
            next,
        ))
        .unwrap();

        let written = export_legacy(&s, &db).unwrap();
        assert_eq!(written, [repo.join(".nibble/cron/malone-daily-tasks.md")]);
        let def = super::super::load_job_file(&written[0]).unwrap();
        assert_eq!(def.label, "Malone Daily Tasks");
        assert!(!def.enabled, "a disabled job stays disabled");
        assert!(!def.read_only);
        assert_eq!(def.prompt, "Do the thing.");
        assert_eq!(s.repos().unwrap(), [repo_s]);
        // Second time: nothing, even after the file is deleted.
        std::fs::remove_file(&written[0]).unwrap();
        assert!(export_legacy(&s, &db).unwrap().is_empty());
    }
}
