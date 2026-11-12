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

    /// Drop the state of `repo`'s jobs whose files are gone. `keep` is
    /// every job file present, valid or not. A paused job keeps its state
    /// either way: a file that vanishes for a while (a branch switch) must
    /// not come back unpaused.
    pub fn prune_jobs(&self, repo: &str, keep: &[String]) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM jobs WHERE repo_path = ?1 AND paused = 0")?;
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

/// What `nibble cron import` would do with one job an older nibble kept
/// in `~/.nibble/tasks.db`.
#[derive(Debug, PartialEq)]
pub enum LegacyItem {
    /// Write `def` to `path` and register `repo`.
    Import {
        repo: PathBuf,
        path: PathBuf,
        def: super::JobDef,
    },
    Skip {
        repo_path: String,
        label: String,
        reason: String,
    },
}

/// Whether the jobs in `~/.nibble/tasks.db` are still to be imported.
pub fn legacy_pending(store: &CronStore, db: &crate::db::Database) -> Result<bool> {
    Ok(store.meta(LEGACY_EXPORTED)?.is_none() && !db.list_cron_jobs(None)?.is_empty())
}

/// Plan the import of older nibble's jobs. Nothing is written: the rows
/// come from `~/.nibble`, which sandboxes can write, so the user reviews
/// the plan first. Each job gets a file name no other file or planned job
/// has.
pub fn plan_legacy(db: &crate::db::Database) -> Result<Vec<LegacyItem>> {
    let mut taken: Vec<PathBuf> = Vec::new();
    let mut plan = Vec::new();
    // Oldest first, so the original of two same-named jobs keeps the name.
    for job in db.list_cron_jobs(None)?.into_iter().rev() {
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
        let (name, path) = (1..)
            .map(|n| {
                let name = if n == 1 {
                    base.clone()
                } else {
                    format!("{base}-{n}")
                };
                let path = super::job_path(&repo, &name);
                (name, path)
            })
            .find(|(_, p)| !p.exists() && !taken.contains(p))
            .unwrap_or_default();
        taken.push(path.clone());
        let def = super::JobDef {
            name,
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
        plan.push(LegacyItem::Import { repo, path, def });
    }
    Ok(plan)
}

/// Carry out a reviewed plan: write the files, register the repos, and
/// remember that the import happened.
pub fn apply_legacy(store: &CronStore, plan: &[LegacyItem]) -> Result<()> {
    for item in plan {
        if let LegacyItem::Import { repo, path, def } = item {
            std::fs::create_dir_all(super::jobs_dir(repo))?;
            std::fs::write(path, super::format_job(def))
                .with_context(|| format!("failed to write {}", path.display()))?;
            store.add_repo(&repo.to_string_lossy())?;
        }
    }
    mark_legacy_done(store)
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

        let plan = plan_legacy(&db).unwrap();
        let mut imported = Vec::new();
        let mut skipped = 0;
        for item in &plan {
            match item {
                LegacyItem::Import {
                    repo: r2,
                    path,
                    def,
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
