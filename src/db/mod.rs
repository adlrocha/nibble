use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::models::{
    AgentType, CronJob, SandboxConfig, SandboxType, Task, TaskContext, TaskStatus,
};

const SCHEMA_VERSION: i32 = 8;

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path).context("Failed to open database")?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;",
        )
        .context("Failed to set DB pragmas")?;

        let mut db = Database { conn };
        db.initialize()?;
        Ok(db)
    }

    fn initialize(&mut self) -> Result<()> {
        // Create schema_version table if it doesn't exist
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY
            )",
            [],
        )?;

        // Check current schema version
        let current_version: Option<i32> = self
            .conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .optional()?;

        match current_version {
            None => {
                // Fresh database, create schema
                self.create_schema()?;
                self.conn.execute(
                    "INSERT INTO schema_version (version) VALUES (?1)",
                    params![SCHEMA_VERSION],
                )?;
            }
            Some(v) if v < SCHEMA_VERSION => {
                self.migrate(v)?;
            }
            Some(_) => {
                // Up to date
            }
        }

        Ok(())
    }

    fn create_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT UNIQUE NOT NULL,
                agent_type TEXT NOT NULL,
                title TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                completed_at INTEGER,
                pid INTEGER,
                ppid INTEGER,
                monitor_pid INTEGER,
                attention_reason TEXT,
                exit_code INTEGER,
                context TEXT,
                metadata TEXT,
                container_id TEXT,
                sandbox_type TEXT DEFAULT 'none',
                sandbox_config TEXT
            );

            CREATE INDEX idx_status ON tasks(status);
            CREATE INDEX idx_updated_at ON tasks(updated_at);
            CREATE INDEX idx_pid ON tasks(pid);
            CREATE INDEX idx_completed_at ON tasks(completed_at);
            CREATE INDEX idx_container_id ON tasks(container_id);

            CREATE TABLE bot_messages (
                message_id INTEGER PRIMARY KEY,
                task_id TEXT NOT NULL,
                sent_at INTEGER NOT NULL
            );

            CREATE TABLE kv_store (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE container_state (
                task_id TEXT PRIMARY KEY,
                container_name TEXT NOT NULL,
                repo_path TEXT NOT NULL,
                worktree_path TEXT,
                created_at INTEGER NOT NULL,
                FOREIGN KEY (task_id) REFERENCES tasks(task_id) ON DELETE CASCADE
            );

            CREATE TABLE cron_jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                repo_path TEXT NOT NULL,
                label TEXT,
                schedule TEXT NOT NULL,
                prompt TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                skip_if_running INTEGER NOT NULL DEFAULT 1,
                running INTEGER NOT NULL DEFAULT 0,
                last_run INTEGER,
                next_run INTEGER NOT NULL,
                expires_at INTEGER,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX idx_cron_next_run ON cron_jobs(next_run) WHERE enabled=1;
            ",
        )?;

        Ok(())
    }

    fn migrate(&mut self, from_version: i32) -> Result<()> {
        if from_version < 2 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS bot_messages (
                    message_id INTEGER PRIMARY KEY,
                    task_id TEXT NOT NULL,
                    sent_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS kv_store (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );",
            )?;
        }

        if from_version < 3 {
            // Add sandbox fields to tasks table
            self.conn.execute_batch(
                "ALTER TABLE tasks ADD COLUMN container_id TEXT;
                 ALTER TABLE tasks ADD COLUMN sandbox_type TEXT DEFAULT 'none';
                 ALTER TABLE tasks ADD COLUMN sandbox_config TEXT;

                 CREATE INDEX idx_container_id ON tasks(container_id);

                 CREATE TABLE container_state (
                     task_id TEXT PRIMARY KEY,
                     container_name TEXT NOT NULL,
                     repo_path TEXT NOT NULL,
                     created_at INTEGER NOT NULL,
                     FOREIGN KEY (task_id) REFERENCES tasks(task_id) ON DELETE CASCADE
                 );",
            )?;
        }

        if from_version < 4 {
            // Add cron_jobs table for scheduled prompts (original version, without `running`)
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS cron_jobs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    task_id TEXT NOT NULL,
                    label TEXT,
                    schedule TEXT NOT NULL,
                    prompt TEXT NOT NULL,
                    enabled INTEGER NOT NULL DEFAULT 1,
                    skip_if_running INTEGER NOT NULL DEFAULT 1,
                    last_run INTEGER,
                    next_run INTEGER NOT NULL,
                    created_at INTEGER NOT NULL,
                    FOREIGN KEY (task_id) REFERENCES tasks(task_id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_cron_next_run ON cron_jobs(next_run) WHERE enabled=1;",
            )?;
        }

        if from_version < 5 {
            // Add `running` column to cron_jobs if it was missing from the v4 migration
            let has_running: bool = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('cron_jobs') WHERE name='running'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0;
            if !has_running {
                self.conn.execute_batch(
                    "ALTER TABLE cron_jobs ADD COLUMN running INTEGER NOT NULL DEFAULT 0;",
                )?;
            }
        }

        if from_version < 6 {
            // Add `expires_at` column for optional job expiry
            let has_expires: bool = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('cron_jobs') WHERE name='expires_at'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0;
            if !has_expires {
                self.conn
                    .execute_batch("ALTER TABLE cron_jobs ADD COLUMN expires_at INTEGER;")?;
            }
        }

        if from_version < 7 {
            // Migrate cron_jobs from task_id FK to repo_path.
            // Resolve repo_path by joining against container_state; disable jobs that can't be resolved.
            self.conn.execute_batch(
                "DROP TABLE IF EXISTS cron_jobs_v7;

                CREATE TABLE cron_jobs_v7 (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    repo_path TEXT NOT NULL,
                    label TEXT,
                    schedule TEXT NOT NULL,
                    prompt TEXT NOT NULL,
                    enabled INTEGER NOT NULL DEFAULT 1,
                    skip_if_running INTEGER NOT NULL DEFAULT 1,
                    running INTEGER NOT NULL DEFAULT 0,
                    last_run INTEGER,
                    next_run INTEGER NOT NULL,
                    expires_at INTEGER,
                    created_at INTEGER NOT NULL
                );

                INSERT INTO cron_jobs_v7
                    (id, repo_path, label, schedule, prompt, enabled, skip_if_running,
                     running, last_run, next_run, expires_at, created_at)
                SELECT
                    cj.id,
                    COALESCE(
                        (SELECT cs.repo_path FROM container_state cs
                         WHERE cs.task_id = cj.task_id
                         ORDER BY cs.created_at DESC LIMIT 1),
                        '(unknown)'
                    ),
                    cj.label, cj.schedule, cj.prompt, cj.enabled, cj.skip_if_running,
                    cj.running, cj.last_run, cj.next_run,
                    cj.expires_at,
                    cj.created_at
                FROM cron_jobs cj;

                UPDATE cron_jobs_v7 SET enabled = 0 WHERE repo_path = '(unknown)';

                DROP TABLE cron_jobs;
                ALTER TABLE cron_jobs_v7 RENAME TO cron_jobs;
                CREATE INDEX idx_cron_next_run ON cron_jobs(next_run) WHERE enabled=1;",
            )?;
        }

        if from_version < 8 {
            // Add worktree_path column to container_state for git worktree support.
            self.conn
                .execute_batch("ALTER TABLE container_state ADD COLUMN worktree_path TEXT;")?;
        }

        self.conn.execute(
            "UPDATE schema_version SET version = ?1",
            params![SCHEMA_VERSION],
        )?;

        Ok(())
    }

    pub fn insert_task(&self, task: &Task) -> Result<i64> {
        let context_json = task
            .context
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        let metadata_json = task
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        let sandbox_config_json = task
            .sandbox_config
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        self.conn.execute(
            "INSERT INTO tasks (
                task_id, agent_type, title, status, created_at, updated_at,
                completed_at, pid, ppid, monitor_pid, attention_reason,
                exit_code, context, metadata, container_id, sandbox_type, sandbox_config
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                task.task_id,
                task.agent_type.as_str(),
                task.title,
                task.status.as_str(),
                task.created_at.timestamp(),
                task.updated_at.timestamp(),
                task.completed_at.map(|dt| dt.timestamp()),
                task.pid,
                task.ppid,
                task.monitor_pid,
                task.attention_reason,
                task.exit_code,
                context_json,
                metadata_json,
                task.container_id,
                task.sandbox_type.as_str(),
                sandbox_config_json,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_task(&self, task: &Task) -> Result<()> {
        let context_json = task
            .context
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        let metadata_json = task
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        let sandbox_config_json = task
            .sandbox_config
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        self.conn.execute(
            "UPDATE tasks SET
                agent_type = ?1, title = ?2, status = ?3, updated_at = ?4,
                completed_at = ?5, pid = ?6, ppid = ?7, monitor_pid = ?8,
                attention_reason = ?9, exit_code = ?10, context = ?11, metadata = ?12,
                container_id = ?13, sandbox_type = ?14, sandbox_config = ?15
            WHERE task_id = ?16",
            params![
                task.agent_type.as_str(),
                task.title,
                task.status.as_str(),
                task.updated_at.timestamp(),
                task.completed_at.map(|dt| dt.timestamp()),
                task.pid,
                task.ppid,
                task.monitor_pid,
                task.attention_reason,
                task.exit_code,
                context_json,
                metadata_json,
                task.container_id,
                task.sandbox_type.as_str(),
                sandbox_config_json,
                task.task_id,
            ],
        )?;

        Ok(())
    }

    pub fn get_task_by_id(&self, task_id: &str) -> Result<Option<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, agent_type, title, status, created_at, updated_at,
                    completed_at, pid, ppid, monitor_pid, attention_reason,
                    exit_code, context, metadata, container_id, sandbox_type, sandbox_config
             FROM tasks WHERE task_id = ?1",
        )?;

        let task = stmt
            .query_row(params![task_id], |row| self.row_to_task(row))
            .optional()?;

        if task.is_some() {
            return Ok(task);
        }

        // Fall back to prefix match (allows short IDs like the first 8 chars)
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, agent_type, title, status, created_at, updated_at,
                    completed_at, pid, ppid, monitor_pid, attention_reason,
                    exit_code, context, metadata, container_id, sandbox_type, sandbox_config
             FROM tasks WHERE task_id LIKE ?1 || '%'",
        )?;

        let mut rows = stmt.query(params![task_id])?;
        let first = rows.next()?.map(|row| self.row_to_task(row)).transpose()?;

        // Ensure the prefix is unambiguous — reject if more than one match
        if first.is_some() && rows.next()?.is_some() {
            anyhow::bail!("Ambiguous short ID '{}': matches multiple tasks", task_id);
        }

        Ok(first)
    }

    /// Record that a Telegram message was sent for a task, so replies can be routed back.
    pub fn insert_bot_message(&self, message_id: i64, task_id: &str) -> Result<()> {
        let now = Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO bot_messages (message_id, task_id, sent_at) VALUES (?1, ?2, ?3)",
            params![message_id, task_id, now],
        )?;
        Ok(())
    }

    /// Look up which task a Telegram message belongs to (for routing replies).
    pub fn get_task_id_by_message_id(&self, message_id: i64) -> Result<Option<String>> {
        let task_id = self
            .conn
            .query_row(
                "SELECT task_id FROM bot_messages WHERE message_id = ?1",
                params![message_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(task_id)
    }

    /// Return the total number of bot messages recorded for `task_id`.
    /// Used by the safety-net to detect new notifications added after an inject started,
    /// without relying on timestamps (avoids clock-skew and WAL snapshot issues).
    pub fn bot_message_count_for_task(&self, task_id: &str) -> Result<i64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM bot_messages WHERE task_id = ?1",
            params![task_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Read a value from the key-value store.
    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let val = self
            .conn
            .query_row(
                "SELECT value FROM kv_store WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(val)
    }

    /// Write a value to the key-value store (upsert).
    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO kv_store (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn kv_delete(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM kv_store WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Get a task by its container ID
    #[allow(dead_code)]
    pub fn get_task_by_container_id(&self, container_id: &str) -> Result<Option<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, agent_type, title, status, created_at, updated_at,
                    completed_at, pid, ppid, monitor_pid, attention_reason,
                    exit_code, context, metadata, container_id, sandbox_type, sandbox_config
             FROM tasks WHERE container_id = ?1",
        )?;

        let task = stmt
            .query_row(params![container_id], |row| self.row_to_task(row))
            .optional()?;

        Ok(task)
    }

    /// List all tasks with a specific sandbox type
    #[allow(dead_code)]
    /// Insert or update container state, optionally recording an associated git worktree path.
    pub fn upsert_container_state_with_worktree(
        &self,
        task_id: &str,
        container_name: &str,
        repo_path: &str,
        worktree_path: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO container_state (task_id, container_name, repo_path, worktree_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![task_id, container_name, repo_path, worktree_path, now],
        )?;
        Ok(())
    }

    /// Get container state by task ID
    #[allow(dead_code)]
    pub fn get_container_state(&self, task_id: &str) -> Result<Option<(String, String, i64)>> {
        let result = self
            .conn
            .query_row(
                "SELECT container_name, repo_path, created_at FROM container_state WHERE task_id = ?1",
                params![task_id],
                |row| {
                    let name: String = row.get(0)?;
                    let path: String = row.get(1)?;
                    let created: i64 = row.get(2)?;
                    Ok((name, path, created))
                },
            )
            .optional()?;
        Ok(result)
    }

    /// Delete container state
    pub fn delete_container_state(&self, task_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM container_state WHERE task_id = ?1",
            params![task_id],
        )?;
        Ok(())
    }

    /// Find the most recent container state for a given repo path.
    /// Returns (task_id, container_name) if found.
    pub fn get_container_state_by_repo_path(
        &self,
        repo_path: &str,
    ) -> Result<Option<(String, String)>> {
        let result = self
            .conn
            .query_row(
                "SELECT task_id, container_name FROM container_state WHERE repo_path = ?1 ORDER BY created_at DESC LIMIT 1",
                params![repo_path],
                |row| {
                    let task_id: String = row.get(0)?;
                    let name: String = row.get(1)?;
                    Ok((task_id, name))
                },
            )
            .optional()?;
        Ok(result)
    }

    /// Return all containers for a given repo path, newest first.
    pub fn get_all_containers_by_repo_path(
        &self,
        repo_path: &str,
    ) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id, container_name FROM container_state WHERE repo_path = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map(params![repo_path], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// List all container states.
    /// Returns (task_id, container_name, repo_path, worktree_path, created_at).
    pub fn list_container_states(
        &self,
    ) -> Result<Vec<(String, String, String, Option<String>, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id, container_name, repo_path, worktree_path, created_at FROM container_state ORDER BY created_at DESC"
        )?;

        let states = stmt
            .query_map([], |row| {
                let task_id: String = row.get(0)?;
                let name: String = row.get(1)?;
                let path: String = row.get(2)?;
                let worktree: Option<String> = row.get(3)?;
                let created: i64 = row.get(4)?;
                Ok((task_id, name, path, worktree, created))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(states)
    }

    /// Get the worktree path for a task, if any.
    pub fn get_worktree_path(&self, task_id: &str) -> Result<Option<String>> {
        let result = self
            .conn
            .query_row(
                "SELECT worktree_path FROM container_state WHERE task_id = ?1",
                params![task_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result)
    }

    // Cron job methods
    pub fn insert_cron_job(&self, job: &CronJob) -> Result<i64> {
        let now = Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO cron_jobs (
                repo_path, label, schedule, prompt, enabled, skip_if_running,
                running, last_run, next_run, expires_at, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                job.repo_path,
                job.label,
                job.schedule,
                job.prompt,
                job.enabled as i32,
                job.skip_if_running as i32,
                job.running as i32,
                job.last_run.map(|dt| dt.timestamp()),
                job.next_run.timestamp(),
                job.expires_at.map(|dt| dt.timestamp()),
                now,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_cron_job(&self, job: &CronJob) -> Result<()> {
        self.conn.execute(
            "UPDATE cron_jobs SET
                label = ?1, schedule = ?2, prompt = ?3, enabled = ?4,
                skip_if_running = ?5, running = ?6, last_run = ?7, next_run = ?8,
                expires_at = ?9
            WHERE id = ?10",
            params![
                job.label,
                job.schedule,
                job.prompt,
                job.enabled as i32,
                job.skip_if_running as i32,
                job.running as i32,
                job.last_run.map(|dt| dt.timestamp()),
                job.next_run.timestamp(),
                job.expires_at.map(|dt| dt.timestamp()),
                job.id.unwrap_or(0),
            ],
        )?;
        Ok(())
    }

    /// Mark a single cron job as running=true/false (used by the background thread).
    pub fn set_cron_job_running(&self, id: i64, running: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE cron_jobs SET running = ?1 WHERE id = ?2",
            params![running as i32, id],
        )?;
        Ok(())
    }

    /// Clear the running flag on all cron jobs.  Called on daemon startup to
    /// recover from a crash where in-flight jobs were left with running=1.
    pub fn reset_all_cron_running_flags(&self) -> Result<()> {
        self.conn
            .execute("UPDATE cron_jobs SET running = 0 WHERE running = 1", [])?;
        Ok(())
    }

    pub fn get_cron_job(&self, id: i64) -> Result<Option<CronJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, repo_path, label, schedule, prompt, enabled, skip_if_running,
                    running, last_run, next_run, expires_at, created_at
             FROM cron_jobs WHERE id = ?1",
        )?;

        let job = stmt
            .query_row(params![id], |row| self.row_to_cron_job(row))
            .optional()?;

        Ok(job)
    }

    pub fn list_cron_jobs(&self, repo_path_filter: Option<&str>) -> Result<Vec<CronJob>> {
        let jobs = match repo_path_filter {
            Some(repo_path) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, repo_path, label, schedule, prompt, enabled, skip_if_running,
                            running, last_run, next_run, expires_at, created_at
                     FROM cron_jobs WHERE repo_path = ?1 ORDER BY created_at DESC",
                )?;
                let jobs = stmt
                    .query_map(params![repo_path], |row| self.row_to_cron_job(row))?
                    .collect::<Result<Vec<_>, _>>()?;
                jobs
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, repo_path, label, schedule, prompt, enabled, skip_if_running,
                            running, last_run, next_run, expires_at, created_at
                     FROM cron_jobs ORDER BY created_at DESC",
                )?;
                let jobs = stmt
                    .query_map([], |row| self.row_to_cron_job(row))?
                    .collect::<Result<Vec<_>, _>>()?;
                jobs
            }
        };

        Ok(jobs)
    }

    pub fn delete_cron_job(&self, id: i64) -> Result<bool> {
        let affected = self
            .conn
            .execute("DELETE FROM cron_jobs WHERE id = ?1", params![id])?;

        Ok(affected > 0)
    }

    pub fn get_cron_job_by_label(&self, label: &str) -> Result<Option<CronJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, repo_path, label, schedule, prompt, enabled, skip_if_running,
                    running, last_run, next_run, expires_at, created_at
             FROM cron_jobs WHERE label = ?1 LIMIT 1",
        )?;
        let job = stmt
            .query_row(params![label], |row| self.row_to_cron_job(row))
            .optional()?;
        Ok(job)
    }

    pub fn label_exists_for_repo(&self, repo_path: &str, label: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM cron_jobs WHERE repo_path = ?1 AND label = ?2",
            params![repo_path, label],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Get all cron jobs that are due to run (next_run <= now and enabled)
    pub fn get_due_cron_jobs(&self, now: DateTime<Utc>) -> Result<Vec<CronJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, repo_path, label, schedule, prompt, enabled, skip_if_running,
                    running, last_run, next_run, expires_at, created_at
             FROM cron_jobs WHERE enabled = 1 AND next_run <= ?1
             ORDER BY next_run ASC",
        )?;

        let jobs = stmt
            .query_map(params![now.timestamp()], |row| self.row_to_cron_job(row))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(jobs)
    }

    fn row_to_cron_job(&self, row: &rusqlite::Row) -> rusqlite::Result<CronJob> {
        let last_run_ts: Option<i64> = row.get(8)?;
        let next_run_ts: i64 = row.get(9)?;
        let expires_ts: Option<i64> = row.get(10)?;
        let created_ts: i64 = row.get(11)?;

        Ok(CronJob {
            id: Some(row.get(0)?),
            repo_path: row.get(1)?,
            label: row.get(2)?,
            schedule: row.get(3)?,
            prompt: row.get(4)?,
            enabled: row.get::<_, i32>(5)? != 0,
            skip_if_running: row.get::<_, i32>(6)? != 0,
            running: row.get::<_, i32>(7)? != 0,
            last_run: last_run_ts.map(|ts| Utc.timestamp_opt(ts, 0).unwrap()),
            next_run: Utc.timestamp_opt(next_run_ts, 0).unwrap(),
            expires_at: expires_ts.map(|ts| Utc.timestamp_opt(ts, 0).unwrap()),
            created_at: Utc.timestamp_opt(created_ts, 0).unwrap(),
        })
    }

    fn row_to_task(&self, row: &rusqlite::Row) -> rusqlite::Result<Task> {
        let created_ts: i64 = row.get(5)?;
        let updated_ts: i64 = row.get(6)?;
        let completed_ts: Option<i64> = row.get(7)?;

        let context_json: Option<String> = row.get(13)?;
        let context: Option<TaskContext> = context_json.and_then(|s| serde_json::from_str(&s).ok());

        let metadata_json: Option<String> = row.get(14)?;
        let metadata: Option<HashMap<String, serde_json::Value>> =
            metadata_json.and_then(|s| serde_json::from_str(&s).ok());

        let status_str: String = row.get(4)?;
        let status = TaskStatus::from_str(&status_str).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e,
            )))
        })?;

        // Sandbox fields (may be NULL for old records)
        let container_id: Option<String> = row.get(15)?;
        let sandbox_type_str: Option<String> = row.get(16)?;
        let sandbox_type = sandbox_type_str
            .and_then(|s| SandboxType::from_str(&s).ok())
            .unwrap_or(SandboxType::None);
        let sandbox_config_json: Option<String> = row.get(17)?;
        let sandbox_config: Option<SandboxConfig> =
            sandbox_config_json.and_then(|s| serde_json::from_str(&s).ok());

        Ok(Task {
            id: Some(row.get(0)?),
            task_id: row.get(1)?,
            agent_type: {
                let s: String = row.get(2)?;
                AgentType::from_str(&s).unwrap() // infallible
            },
            title: row.get(3)?,
            status,
            created_at: Utc.timestamp_opt(created_ts, 0).unwrap(),
            updated_at: Utc.timestamp_opt(updated_ts, 0).unwrap(),
            completed_at: completed_ts.map(|ts| Utc.timestamp_opt(ts, 0).unwrap()),
            pid: row.get(8)?,
            ppid: row.get(9)?,
            monitor_pid: row.get(10)?,
            attention_reason: row.get(11)?,
            exit_code: row.get(12)?,
            context,
            metadata,
            container_id,
            sandbox_type,
            sandbox_config,
        })
    }
}

pub fn default_db_path() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME environment variable not set");
    PathBuf::from(home).join(".nibble").join("tasks.db")
}

pub fn ensure_data_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    let data_dir = PathBuf::from(home).join(".nibble");

    if !data_dir.exists() {
        std::fs::create_dir_all(&data_dir).context("Failed to create data directory")?;
    }

    Ok(data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn create_test_db() -> (Database, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open(temp_file.path()).unwrap();
        (db, temp_file)
    }

    #[test]
    fn test_database_creation() {
        let (_db, _temp) = create_test_db();
        // If we got here, database was created successfully
    }

    #[test]
    fn test_insert_and_retrieve_task() {
        let (db, _temp) = create_test_db();

        let task = Task::new(
            "test-123".to_string(),
            AgentType::ClaudeCode,
            "Test task".to_string(),
            Some(1234),
            Some(1233),
        );

        let id = db.insert_task(&task).unwrap();
        assert!(id > 0);

        let retrieved = db.get_task_by_id("test-123").unwrap();
        assert!(retrieved.is_some());

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.task_id, "test-123");
        assert_eq!(retrieved.agent_type, AgentType::ClaudeCode);
        assert_eq!(retrieved.status, TaskStatus::Running);
    }

    #[test]
    fn test_update_task() {
        let (db, _temp) = create_test_db();

        let mut task = Task::new(
            "test-123".to_string(),
            AgentType::ClaudeCode,
            "Test task".to_string(),
            Some(1234),
            None,
        );

        db.insert_task(&task).unwrap();

        task.complete();
        db.update_task(&task).unwrap();

        let retrieved = db.get_task_by_id("test-123").unwrap().unwrap();
        assert_eq!(retrieved.status, TaskStatus::Completed);
    }

    /// AC-5: DB round-trip for AgentType::OpenCode
    #[test]
    fn test_ac5_agent_type_opencode_db_round_trip() {
        let (db, _temp) = create_test_db();
        let task = Task::new(
            "oc-123".to_string(),
            AgentType::OpenCode,
            "opencode task".to_string(),
            None,
            None,
        );
        db.insert_task(&task).unwrap();
        let retrieved = db.get_task_by_id("oc-123").unwrap().unwrap();
        assert_eq!(retrieved.agent_type, AgentType::OpenCode);
    }

    /// AC-6: DB round-trip for AgentType::Unknown — lossless
    #[test]
    fn test_ac6_agent_type_unknown_db_round_trip() {
        let (db, _temp) = create_test_db();
        let task = Task::new(
            "bot-123".to_string(),
            AgentType::Unknown("my_bot".to_string()),
            "unknown agent task".to_string(),
            None,
            None,
        );
        db.insert_task(&task).unwrap();
        let retrieved = db.get_task_by_id("bot-123").unwrap().unwrap();
        assert_eq!(
            retrieved.agent_type,
            AgentType::Unknown("my_bot".to_string())
        );
    }
}
