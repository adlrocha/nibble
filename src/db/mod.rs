use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::models::{
    AgentType, CronJob, SandboxConfig, SandboxType, Task, TaskContext, TaskStatus,
};

const SCHEMA_VERSION: i32 = 11;

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
                container_name TEXT,
                repo_path TEXT,
                worktree_path TEXT,
                sandbox_type TEXT DEFAULT 'none',
                sandbox_config TEXT
            );

            CREATE INDEX idx_status ON tasks(status);
            CREATE INDEX idx_updated_at ON tasks(updated_at);
            CREATE INDEX idx_pid ON tasks(pid);
            CREATE INDEX idx_completed_at ON tasks(completed_at);
            CREATE INDEX idx_container_id ON tasks(container_id);
            CREATE INDEX idx_tasks_repo_path ON tasks(repo_path) WHERE repo_path IS NOT NULL;

            CREATE TABLE bot_messages (
                message_id INTEGER PRIMARY KEY,
                task_id TEXT NOT NULL,
                sent_at INTEGER NOT NULL
            );

            CREATE TABLE kv_store (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
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

            CREATE TABLE hermes_repos (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                repo_path TEXT UNIQUE NOT NULL,
                mount_name TEXT UNIQUE NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX idx_hermes_repos_path ON hermes_repos(repo_path);

            CREATE TABLE token_usage (
                provider TEXT NOT NULL,
                api_provider TEXT,
                model TEXT NOT NULL,
                session_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                ts INTEGER NOT NULL,
                cwd TEXT,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                estimated_cost_usd REAL NOT NULL DEFAULT 0,
                PRIMARY KEY (provider, message_id)
            );
            CREATE INDEX idx_token_usage_ts ON token_usage(ts);
            CREATE INDEX idx_token_usage_model ON token_usage(provider, model);
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

        if from_version < 9 {
            // Add hermes_repos table for dynamic repo mounts.
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS hermes_repos (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    repo_path TEXT UNIQUE NOT NULL,
                    mount_name TEXT UNIQUE NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_hermes_repos_path ON hermes_repos(repo_path);",
            )?;
        }

        if from_version < 11 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS token_usage (
                    provider TEXT NOT NULL,
                    api_provider TEXT,
                    model TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    ts INTEGER NOT NULL,
                    cwd TEXT,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                    estimated_cost_usd REAL NOT NULL DEFAULT 0,
                    PRIMARY KEY (provider, message_id)
                );
                CREATE INDEX IF NOT EXISTS idx_token_usage_ts ON token_usage(ts);
                CREATE INDEX IF NOT EXISTS idx_token_usage_model ON token_usage(provider, model);",
            )?;
        }

        if from_version < 10 {
            // Fold container_state into tasks: add columns, copy data, drop old table.
            self.conn.execute_batch(
                "ALTER TABLE tasks ADD COLUMN container_name TEXT;
                 ALTER TABLE tasks ADD COLUMN repo_path TEXT;
                 ALTER TABLE tasks ADD COLUMN worktree_path TEXT;

                 UPDATE tasks SET
                     container_name = (SELECT cs.container_name FROM container_state cs WHERE cs.task_id = tasks.task_id),
                     repo_path = (SELECT cs.repo_path FROM container_state cs WHERE cs.task_id = tasks.task_id),
                     worktree_path = (SELECT cs.worktree_path FROM container_state cs WHERE cs.task_id = tasks.task_id);

                 CREATE INDEX idx_tasks_repo_path ON tasks(repo_path) WHERE repo_path IS NOT NULL;

                 DROP TABLE container_state;",
            )?;
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
                exit_code, context, metadata, container_id, container_name,
                repo_path, worktree_path, sandbox_type, sandbox_config
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
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
                task.container_name,
                task.repo_path,
                task.worktree_path,
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
                container_id = ?13, container_name = ?14, repo_path = ?15,
                worktree_path = ?16, sandbox_type = ?17, sandbox_config = ?18
            WHERE task_id = ?19",
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
                task.container_name,
                task.repo_path,
                task.worktree_path,
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
                    exit_code, context, metadata, container_id, container_name, repo_path, worktree_path, sandbox_type, sandbox_config
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
                    exit_code, context, metadata, container_id, container_name, repo_path, worktree_path, sandbox_type, sandbox_config
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

    /// List all tasks from the database (most recent first).
    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, agent_type, title, status, created_at, updated_at,
                    completed_at, pid, ppid, monitor_pid, attention_reason,
                    exit_code, context, metadata, container_id, container_name, repo_path, worktree_path, sandbox_type, sandbox_config
             FROM tasks
             ORDER BY updated_at DESC",
        )?;

        let mut rows = stmt.query([])?;
        let mut tasks = Vec::new();
        while let Some(row) = rows.next()? {
            tasks.push(self.row_to_task(row)?);
        }
        Ok(tasks)
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
                    exit_code, context, metadata, container_id, container_name, repo_path, worktree_path, sandbox_type, sandbox_config
             FROM tasks WHERE container_id = ?1",
        )?;

        let task = stmt
            .query_row(params![container_id], |row| self.row_to_task(row))
            .optional()?;

        Ok(task)
    }

    /// List all tasks that have an associated sandbox (sandbox_type != 'none').
    pub fn list_sandbox_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, agent_type, title, status, created_at, updated_at,
                    completed_at, pid, ppid, monitor_pid, attention_reason,
                    exit_code, context, metadata, container_id, container_name,
                    repo_path, worktree_path, sandbox_type, sandbox_config
             FROM tasks
             WHERE sandbox_type != 'none'
             ORDER BY created_at DESC, id DESC",
        )?;

        let tasks = stmt
            .query_map([], |row| self.row_to_task(row))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(tasks)
    }

    /// Find the most recent sandbox task for a given repo path.
    pub fn get_task_by_repo_path(&self, repo_path: &str) -> Result<Option<Task>> {
        let result = self
            .conn
            .query_row(
                "SELECT id, task_id, agent_type, title, status, created_at, updated_at,
                        completed_at, pid, ppid, monitor_pid, attention_reason,
                        exit_code, context, metadata, container_id, container_name,
                        repo_path, worktree_path, sandbox_type, sandbox_config
                 FROM tasks
                 WHERE repo_path = ?1 AND sandbox_type != 'none'
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                params![repo_path],
                |row| self.row_to_task(row),
            )
            .optional()?;
        Ok(result)
    }

    /// Return all sandbox tasks for a given repo path, newest first.
    pub fn get_tasks_by_repo_path(&self, repo_path: &str) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, agent_type, title, status, created_at, updated_at,
                    completed_at, pid, ppid, monitor_pid, attention_reason,
                    exit_code, context, metadata, container_id, container_name,
                    repo_path, worktree_path, sandbox_type, sandbox_config
             FROM tasks
             WHERE repo_path = ?1 AND sandbox_type != 'none'
             ORDER BY created_at DESC, id DESC",
        )?;

        let tasks = stmt
            .query_map(params![repo_path], |row| self.row_to_task(row))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(tasks)
    }

    /// Delete a single task by its `task_id`. Returns true if a row was removed.
    pub fn delete_task_by_id(&self, task_id: &str) -> Result<bool> {
        let deleted = self
            .conn
            .execute("DELETE FROM tasks WHERE task_id = ?1", params![task_id])?;
        Ok(deleted > 0)
    }

    /// Delete every exited task row. Live and detached rows stay.
    pub fn delete_exited_tasks(&self) -> Result<usize> {
        let deleted = self
            .conn
            .execute("DELETE FROM tasks WHERE status = 'exited'", [])?;
        Ok(deleted)
    }

    /// Delete every task row. Caller must already have stopped containers.
    pub fn delete_all_tasks(&self) -> Result<usize> {
        let deleted = self.conn.execute("DELETE FROM tasks", [])?;
        Ok(deleted)
    }

    /// Delete sandbox tasks that have been exited for longer than `days`.
    /// Returns the number of tasks deleted.
    pub fn delete_exited_sandbox_tasks_older_than(&self, days: i64) -> Result<usize> {
        let cutoff = Utc::now() - chrono::Duration::days(days);
        let deleted = self.conn.execute(
            "DELETE FROM tasks
             WHERE sandbox_type != 'none'
               AND status = 'exited'
               AND updated_at < ?1",
            params![cutoff.timestamp()],
        )?;
        Ok(deleted)
    }

    // Hermes repo mount methods

    /// Add a repo to the hermes_repos table. Returns the row id.
    pub fn insert_hermes_repo(&self, repo_path: &str, mount_name: &str) -> Result<i64> {
        let now = Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO hermes_repos (repo_path, mount_name, created_at) VALUES (?1, ?2, ?3)",
            params![repo_path, mount_name, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Remove a repo from hermes_repos by canonical path. Returns true if deleted.
    pub fn delete_hermes_repo(&self, repo_path: &str) -> Result<bool> {
        let affected = self.conn.execute(
            "DELETE FROM hermes_repos WHERE repo_path = ?1",
            params![repo_path],
        )?;
        Ok(affected > 0)
    }

    /// Get a hermes repo by canonical path. Returns (id, mount_name) if found.
    pub fn get_hermes_repo(&self, repo_path: &str) -> Result<Option<(i64, String)>> {
        let result = self
            .conn
            .query_row(
                "SELECT id, mount_name FROM hermes_repos WHERE repo_path = ?1",
                params![repo_path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(result)
    }

    /// List all hermes repos. Returns (repo_path, mount_name) pairs.
    pub fn list_hermes_repos(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT repo_path, mount_name FROM hermes_repos ORDER BY created_at ASC")?;
        let repos = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(repos)
    }

    /// Seed hermes_repos from legacy config.toml repos list.
    /// Returns the number of repos seeded.
    pub fn seed_hermes_repos_from_config(
        &self,
        repos: &[(String, std::path::PathBuf)],
    ) -> Result<usize> {
        let existing = self.list_hermes_repos()?;
        let existing_paths: std::collections::HashSet<&str> =
            existing.iter().map(|(p, _)| p.as_str()).collect();

        let mut seeded = 0;
        for (mount_name, abs_path) in repos {
            let path_str = abs_path.to_string_lossy().to_string();
            if existing_paths.contains(path_str.as_str()) {
                continue;
            }
            self.insert_hermes_repo(&path_str, mount_name)?;
            seeded += 1;
        }
        Ok(seeded)
    }

    /// Check if a mount_name is already taken.
    pub fn hermes_mount_name_exists(&self, mount_name: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM hermes_repos WHERE mount_name = ?1",
            params![mount_name],
            |row| row.get(0),
        )?;
        Ok(count > 0)
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
                     FROM cron_jobs WHERE repo_path = ?1 ORDER BY created_at DESC, id DESC",
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
                     FROM cron_jobs ORDER BY created_at DESC, id DESC",
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
        let container_name: Option<String> = row.get(16)?;
        let repo_path: Option<String> = row.get(17)?;
        let worktree_path: Option<String> = row.get(18)?;
        let sandbox_type_str: Option<String> = row.get(19)?;
        let sandbox_type = sandbox_type_str
            .and_then(|s| SandboxType::from_str(&s).ok())
            .unwrap_or(SandboxType::None);
        let sandbox_config_json: Option<String> = row.get(20)?;
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
            container_name,
            repo_path,
            worktree_path,
            sandbox_type,
            sandbox_config,
        })
    }

    // ── token_usage ────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_token_usage(
        &self,
        provider: &str,
        api_provider: Option<&str>,
        model: &str,
        session_id: &str,
        message_id: &str,
        ts: i64,
        cwd: Option<&str>,
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: i64,
        cache_write_tokens: i64,
        estimated_cost_usd: f64,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO token_usage
                (provider, api_provider, model, session_id, message_id, ts, cwd,
                 input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                 estimated_cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                provider,
                api_provider,
                model,
                session_id,
                message_id,
                ts,
                cwd,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                estimated_cost_usd,
            ],
        )?;
        if changed == 0 {
            // Record already exists; refresh derived fields (especially cost)
            // so pricing-table updates are reflected on re-scans.
            self.conn.execute(
                "UPDATE token_usage
                 SET api_provider = ?,
                     model = ?,
                     session_id = ?,
                     ts = ?,
                     cwd = ?,
                     input_tokens = ?,
                     output_tokens = ?,
                     cache_read_tokens = ?,
                     cache_write_tokens = ?,
                     estimated_cost_usd = ?
                 WHERE provider = ? AND message_id = ?",
                params![
                    api_provider,
                    model,
                    session_id,
                    ts,
                    cwd,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    estimated_cost_usd,
                    provider,
                    message_id,
                ],
            )?;
        }
        Ok(changed > 0)
    }

    /// Aggregate usage grouped by one of: "model", "provider", "sandbox" (cwd).
    /// `since_ts` filters to ts >= since_ts; pass 0 to include all rows.
    /// Returns earliest and latest ts among matched rows so the caller can
    /// render the actual covered window.
    pub fn token_usage_summary(
        &self,
        group_by: &str,
        since_ts: i64,
    ) -> Result<(Vec<TokenUsageSummaryRow>, Option<(i64, i64)>)> {
        // (display bucket, raw model, raw api_provider) per grouping.
        let (bucket_sql, model_sql, api_provider_sql) = match group_by {
            "provider" => ("provider", "NULL", "NULL"),
            "sandbox" | "cwd" => ("COALESCE(cwd, '(unknown)')", "NULL", "NULL"),
            // default "model"
            _ => (
                "provider || '/' || model",
                "MAX(model)",
                "MAX(api_provider)",
            ),
        };
        let sql = format!(
            "SELECT {bucket_sql} AS bucket,
                    {model_sql} AS model,
                    {api_provider_sql} AS api_provider,
                    SUM(input_tokens), SUM(output_tokens),
                    SUM(cache_read_tokens), SUM(cache_write_tokens),
                    SUM(estimated_cost_usd), COUNT(*),
                    MAX(provider)
             FROM token_usage
             WHERE ts >= ?1
             GROUP BY bucket
             ORDER BY SUM(estimated_cost_usd) DESC, bucket"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![since_ts], |row| {
                Ok(TokenUsageSummaryRow {
                    bucket: row.get(0)?,
                    model: row.get(1).ok(),
                    api_provider: row.get(2).ok(),
                    input_tokens: row.get::<_, i64>(3).unwrap_or(0),
                    output_tokens: row.get::<_, i64>(4).unwrap_or(0),
                    cache_read_tokens: row.get::<_, i64>(5).unwrap_or(0),
                    cache_write_tokens: row.get::<_, i64>(6).unwrap_or(0),
                    estimated_cost_usd: row.get::<_, f64>(7).unwrap_or(0.0),
                    message_count: row.get::<_, i64>(8).unwrap_or(0),
                    provider: row.get(9).unwrap_or_default(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Discover the actual ts window among matched rows.
        let window: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT MIN(ts), MAX(ts) FROM token_usage WHERE ts >= ?1",
                params![since_ts],
                |row| {
                    let mn: Option<i64> = row.get(0)?;
                    let mx: Option<i64> = row.get(1)?;
                    Ok(mn.zip(mx))
                },
            )
            .optional()?
            .flatten();

        Ok((rows, window))
    }
}

#[derive(Debug, Clone)]
pub struct TokenUsageSummaryRow {
    pub bucket: String,
    /// Raw model string when grouped by model; None for other groupings.
    pub model: Option<String>,
    /// Raw api_provider when grouped by model; None for other groupings.
    pub api_provider: Option<String>,
    /// Top-level provider ("claude" | "pi"); always populated.
    pub provider: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub estimated_cost_usd: f64,
    pub message_count: i64,
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

    #[test]
    fn test_delete_task_by_id() {
        let (db, _temp) = create_test_db();

        let task = Task::new(
            "test-del".to_string(),
            AgentType::ClaudeCode,
            "Test task".to_string(),
            None,
            None,
        );
        db.insert_task(&task).unwrap();

        // Deleting an existing task returns true and removes it.
        assert!(db.delete_task_by_id("test-del").unwrap());
        assert!(db.get_task_by_id("test-del").unwrap().is_none());

        // Deleting a non-existent task returns false.
        assert!(!db.delete_task_by_id("does-not-exist").unwrap());
    }

    /// AC-5: DB round-trip for AgentType::Pi
    #[test]
    fn test_ac5_agent_type_pi_db_round_trip() {
        let (db, _temp) = create_test_db();
        let task = Task::new(
            "pi-123".to_string(),
            AgentType::Pi,
            "pi task".to_string(),
            None,
            None,
        );
        db.insert_task(&task).unwrap();
        let retrieved = db.get_task_by_id("pi-123").unwrap().unwrap();
        assert_eq!(retrieved.agent_type, AgentType::Pi);
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

    // ── hermes_repos table tests ─────────────────────────────────────────────

    /// AC-4: Insert and list hermes repos
    #[test]
    fn test_hermes_ac4_insert_and_list_repos() {
        let (db, _temp) = create_test_db();
        assert!(db.list_hermes_repos().unwrap().is_empty());

        db.insert_hermes_repo("/home/user/project-a", "project-a")
            .unwrap();
        db.insert_hermes_repo("/home/user/project-b", "project-b")
            .unwrap();

        let repos = db.list_hermes_repos().unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(
            repos[0],
            ("/home/user/project-a".to_string(), "project-a".to_string())
        );
        assert_eq!(
            repos[1],
            ("/home/user/project-b".to_string(), "project-b".to_string())
        );
    }

    /// AC-5: Get hermes repo by path
    #[test]
    fn test_hermes_ac5_get_repo_by_path() {
        let (db, _temp) = create_test_db();
        db.insert_hermes_repo("/home/user/myrepo", "myrepo")
            .unwrap();

        let result = db.get_hermes_repo("/home/user/myrepo").unwrap();
        assert!(result.is_some());
        let (id, name) = result.unwrap();
        assert!(id > 0);
        assert_eq!(name, "myrepo");
    }

    /// AC-5: Get non-existent repo returns None
    #[test]
    fn test_hermes_get_repo_not_found() {
        let (db, _temp) = create_test_db();
        assert!(db.get_hermes_repo("/nonexistent").unwrap().is_none());
    }

    /// AC-5: Delete hermes repo
    #[test]
    fn test_hermes_ac5_delete_repo() {
        let (db, _temp) = create_test_db();
        db.insert_hermes_repo("/home/user/myrepo", "myrepo")
            .unwrap();
        assert!(db.delete_hermes_repo("/home/user/myrepo").unwrap());
        assert!(db.get_hermes_repo("/home/user/myrepo").unwrap().is_none());
        assert!(db.list_hermes_repos().unwrap().is_empty());
    }

    /// AC-5: Delete non-existent repo returns false
    #[test]
    fn test_hermes_delete_nonexistent() {
        let (db, _temp) = create_test_db();
        assert!(!db.delete_hermes_repo("/nonexistent").unwrap());
    }

    /// INV-3: Duplicate repo_path is rejected (UNIQUE constraint)
    #[test]
    fn test_hermes_inv3_duplicate_path_rejected() {
        let (db, _temp) = create_test_db();
        db.insert_hermes_repo("/home/user/myrepo", "myrepo")
            .unwrap();
        let result = db.insert_hermes_repo("/home/user/myrepo", "myrepo-2");
        assert!(result.is_err(), "duplicate repo_path should be rejected");
    }

    /// INV-3: Duplicate mount_name is rejected (UNIQUE constraint)
    #[test]
    fn test_hermes_inv3_duplicate_mount_name_rejected() {
        let (db, _temp) = create_test_db();
        db.insert_hermes_repo("/home/user/repo1", "myrepo").unwrap();
        let result = db.insert_hermes_repo("/home/user/repo2", "myrepo");
        assert!(result.is_err(), "duplicate mount_name should be rejected");
    }

    /// AC-10: seed_hermes_repos_from_config seeds new repos
    #[test]
    fn test_hermes_ac10_seed_from_config() {
        let (db, _temp) = create_test_db();
        let repos = vec![
            (
                "project-a".to_string(),
                std::path::PathBuf::from("/home/user/project-a"),
            ),
            (
                "project-b".to_string(),
                std::path::PathBuf::from("/home/user/project-b"),
            ),
        ];
        let seeded = db.seed_hermes_repos_from_config(&repos).unwrap();
        assert_eq!(seeded, 2);

        let listed = db.list_hermes_repos().unwrap();
        assert_eq!(listed.len(), 2);
    }

    /// AC-10: seed_hermes_repos_from_config skips existing repos
    #[test]
    fn test_hermes_ac10_seed_skips_existing() {
        let (db, _temp) = create_test_db();
        db.insert_hermes_repo("/home/user/project-a", "project-a")
            .unwrap();

        let repos = vec![
            (
                "project-a".to_string(),
                std::path::PathBuf::from("/home/user/project-a"),
            ),
            (
                "project-b".to_string(),
                std::path::PathBuf::from("/home/user/project-b"),
            ),
        ];
        let seeded = db.seed_hermes_repos_from_config(&repos).unwrap();
        assert_eq!(seeded, 1, "only the new repo should be seeded");

        let listed = db.list_hermes_repos().unwrap();
        assert_eq!(listed.len(), 2);
    }

    /// hermes_mount_name_exists
    #[test]
    fn test_hermes_mount_name_exists() {
        let (db, _temp) = create_test_db();
        assert!(!db.hermes_mount_name_exists("myrepo").unwrap());
        db.insert_hermes_repo("/home/user/myrepo", "myrepo")
            .unwrap();
        assert!(db.hermes_mount_name_exists("myrepo").unwrap());
    }

    /// Schema v9 migration: hermes_repos table exists on fresh DB
    #[test]
    fn test_hermes_schema_v9_table_exists() {
        let (db, _temp) = create_test_db();
        let result = db.conn.execute(
            "INSERT INTO hermes_repos (repo_path, mount_name, created_at) VALUES (?1, ?2, ?3)",
            params!["/test", "test", chrono::Utc::now().timestamp()],
        );
        assert!(
            result.is_ok(),
            "hermes_repos table should exist in fresh schema"
        );
    }

    /// INV-1: list_sandbox_tasks filters out non-sandbox tasks
    #[test]
    fn test_list_sandbox_tasks_filters_non_sandbox() {
        let (db, _temp) = create_test_db();

        let plain = Task::new(
            "plain".into(),
            AgentType::ClaudeCode,
            "plain".into(),
            None,
            None,
        );
        db.insert_task(&plain).unwrap();

        let mut sandboxed = Task::new(
            "sandbox".into(),
            AgentType::ClaudeCode,
            "sandbox".into(),
            None,
            None,
        );
        sandboxed.sandbox_type = SandboxType::Podman;
        sandboxed.container_id = Some("abc".into());
        sandboxed.container_name = Some("nibble-test".into());
        sandboxed.repo_path = Some("/tmp/test".into());
        db.insert_task(&sandboxed).unwrap();

        let list = db.list_sandbox_tasks().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task_id, "sandbox");
        assert_eq!(list[0].container_name, Some("nibble-test".into()));
    }

    /// INV-2: get_task_by_repo_path returns the most recent sandbox for a repo
    #[test]
    fn test_get_task_by_repo_path() {
        let (db, _temp) = create_test_db();

        let mut t1 = Task::new(
            "old".into(),
            AgentType::ClaudeCode,
            "old".into(),
            None,
            None,
        );
        t1.sandbox_type = SandboxType::Podman;
        t1.repo_path = Some("/tmp/repo".into());
        db.insert_task(&t1).unwrap();

        // Force a small delay so created_at differs
        std::thread::sleep(std::time::Duration::from_millis(10));

        let mut t2 = Task::new(
            "new".into(),
            AgentType::ClaudeCode,
            "new".into(),
            None,
            None,
        );
        t2.sandbox_type = SandboxType::Podman;
        t2.repo_path = Some("/tmp/repo".into());
        db.insert_task(&t2).unwrap();

        let found = db.get_task_by_repo_path("/tmp/repo").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().task_id, "new");

        assert!(db.get_task_by_repo_path("/nonexistent").unwrap().is_none());
    }

    /// INV-3: get_tasks_by_repo_path returns all sandbox tasks for a repo, newest first
    #[test]
    fn test_get_tasks_by_repo_path() {
        let (db, _temp) = create_test_db();

        let mut t1 = Task::new(
            "first".into(),
            AgentType::ClaudeCode,
            "first".into(),
            None,
            None,
        );
        t1.sandbox_type = SandboxType::Podman;
        t1.repo_path = Some("/tmp/repo".into());
        db.insert_task(&t1).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));

        let mut t2 = Task::new(
            "second".into(),
            AgentType::ClaudeCode,
            "second".into(),
            None,
            None,
        );
        t2.sandbox_type = SandboxType::Podman;
        t2.repo_path = Some("/tmp/repo".into());
        db.insert_task(&t2).unwrap();

        let list = db.get_tasks_by_repo_path("/tmp/repo").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].task_id, "second");
        assert_eq!(list[1].task_id, "first");
    }
}
