use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::models::{SandboxConfig, SandboxType, Task, TaskContext, TaskStatus};

const SCHEMA_VERSION: i32 = 3;

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path).context("Failed to open database")?;

        // Enable WAL mode for better concurrent access
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .context("Failed to set WAL mode")?;

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
                created_at INTEGER NOT NULL,
                FOREIGN KEY (task_id) REFERENCES tasks(task_id) ON DELETE CASCADE
            );
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
                task.agent_type,
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
                task.agent_type,
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

    pub fn list_tasks(&self, status_filter: Option<TaskStatus>) -> Result<Vec<Task>> {
        let base_query = "SELECT id, task_id, agent_type, title, status, created_at, updated_at,
                completed_at, pid, ppid, monitor_pid, attention_reason,
                exit_code, context, metadata, container_id, sandbox_type, sandbox_config
         FROM tasks";
        let query = if let Some(status) = status_filter {
            format!(
                "{} WHERE status = '{}' ORDER BY updated_at DESC",
                base_query,
                status.as_str()
            )
        } else {
            format!("{} ORDER BY updated_at DESC", base_query)
        };

        let mut stmt = self.conn.prepare(&query)?;
        let tasks = stmt
            .query_map([], |row| self.row_to_task(row))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(tasks)
    }

    pub fn delete_task(&self, task_id: &str) -> Result<bool> {
        let affected = self
            .conn
            .execute("DELETE FROM tasks WHERE task_id = ?1", params![task_id])?;

        Ok(affected > 0)
    }

    pub fn cleanup_old_completed(&self, older_than_secs: i64) -> Result<usize> {
        let cutoff = Utc::now().timestamp() - older_than_secs;

        let affected = self.conn.execute(
            "DELETE FROM tasks WHERE status IN ('completed', 'exited') AND completed_at < ?1",
            params![cutoff],
        )?;

        Ok(affected)
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

    /// Delete bot_messages older than the given retention period (same cadence as tasks cleanup).
    pub fn cleanup_old_bot_messages(&self, older_than_secs: i64) -> Result<usize> {
        let cutoff = Utc::now().timestamp() - older_than_secs;
        let affected = self.conn.execute(
            "DELETE FROM bot_messages WHERE sent_at < ?1",
            params![cutoff],
        )?;
        Ok(affected)
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
        // Used by the Telegram listener to clear pending-reply state.
        self.conn.execute("DELETE FROM kv_store WHERE key = ?1", params![key])?;
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
    pub fn list_tasks_by_sandbox_type(&self, sandbox_type: SandboxType) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, agent_type, title, status, created_at, updated_at,
                    completed_at, pid, ppid, monitor_pid, attention_reason,
                    exit_code, context, metadata, container_id, sandbox_type, sandbox_config
             FROM tasks WHERE sandbox_type = ?1 ORDER BY updated_at DESC",
        )?;

        let tasks = stmt
            .query_map(params![sandbox_type.as_str()], |row| self.row_to_task(row))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(tasks)
    }

    /// Insert or update container state for resume functionality
    pub fn upsert_container_state(&self, task_id: &str, container_name: &str, repo_path: &str) -> Result<()> {
        let now = Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO container_state (task_id, container_name, repo_path, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![task_id, container_name, repo_path, now],
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

    /// List all container states
    pub fn list_container_states(&self) -> Result<Vec<(String, String, String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id, container_name, repo_path, created_at FROM container_state ORDER BY created_at DESC"
        )?;

        let states = stmt
            .query_map([], |row| {
                let task_id: String = row.get(0)?;
                let name: String = row.get(1)?;
                let path: String = row.get(2)?;
                let created: i64 = row.get(3)?;
                Ok((task_id, name, path, created))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(states)
    }

    fn row_to_task(&self, row: &rusqlite::Row) -> rusqlite::Result<Task> {
        let created_ts: i64 = row.get(5)?;
        let updated_ts: i64 = row.get(6)?;
        let completed_ts: Option<i64> = row.get(7)?;

        let context_json: Option<String> = row.get(13)?;
        let context: Option<TaskContext> = context_json
            .and_then(|s| serde_json::from_str(&s).ok());

        let metadata_json: Option<String> = row.get(14)?;
        let metadata: Option<HashMap<String, serde_json::Value>> = metadata_json
            .and_then(|s| serde_json::from_str(&s).ok());

        let status_str: String = row.get(4)?;
        let status = TaskStatus::from_str(&status_str)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e,
            ))))?;

        // Sandbox fields (may be NULL for old records)
        let container_id: Option<String> = row.get(15)?;
        let sandbox_type_str: Option<String> = row.get(16)?;
        let sandbox_type = sandbox_type_str
            .and_then(|s| SandboxType::from_str(&s).ok())
            .unwrap_or(SandboxType::None);
        let sandbox_config_json: Option<String> = row.get(17)?;
        let sandbox_config: Option<SandboxConfig> = sandbox_config_json
            .and_then(|s| serde_json::from_str(&s).ok());

        Ok(Task {
            id: Some(row.get(0)?),
            task_id: row.get(1)?,
            agent_type: row.get(2)?,
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
    PathBuf::from(home)
        .join(".agent-tasks")
        .join("tasks.db")
}

pub fn ensure_data_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    let data_dir = PathBuf::from(home).join(".agent-tasks");

    if !data_dir.exists() {
        std::fs::create_dir_all(&data_dir)
            .context("Failed to create data directory")?;
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
            "claude_code".to_string(),
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
        assert_eq!(retrieved.agent_type, "claude_code");
        assert_eq!(retrieved.status, TaskStatus::Running);
    }

    #[test]
    fn test_update_task() {
        let (db, _temp) = create_test_db();

        let mut task = Task::new(
            "test-123".to_string(),
            "claude_code".to_string(),
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
    fn test_list_tasks() {
        let (db, _temp) = create_test_db();

        let task1 = Task::new(
            "test-1".to_string(),
            "claude_code".to_string(),
            "Task 1".to_string(),
            None,
            None,
        );
        let mut task2 = Task::new(
            "test-2".to_string(),
            "opencode".to_string(),
            "Task 2".to_string(),
            None,
            None,
        );
        task2.complete();

        db.insert_task(&task1).unwrap();
        db.insert_task(&task2).unwrap();

        let all_tasks = db.list_tasks(None).unwrap();
        assert_eq!(all_tasks.len(), 2);

        let running_tasks = db.list_tasks(Some(TaskStatus::Running)).unwrap();
        assert_eq!(running_tasks.len(), 1);
        assert_eq!(running_tasks[0].task_id, "test-1");

        let completed_tasks = db.list_tasks(Some(TaskStatus::Completed)).unwrap();
        assert_eq!(completed_tasks.len(), 1);
        assert_eq!(completed_tasks[0].task_id, "test-2");
    }

    #[test]
    fn test_delete_task() {
        let (db, _temp) = create_test_db();

        let task = Task::new(
            "test-123".to_string(),
            "claude_code".to_string(),
            "Test task".to_string(),
            None,
            None,
        );

        db.insert_task(&task).unwrap();

        let deleted = db.delete_task("test-123").unwrap();
        assert!(deleted);

        let retrieved = db.get_task_by_id("test-123").unwrap();
        assert!(retrieved.is_none());
    }

    #[test]
    fn test_cleanup_old_completed() {
        let (db, _temp) = create_test_db();

        let mut task = Task::new(
            "test-123".to_string(),
            "claude_code".to_string(),
            "Test task".to_string(),
            None,
            None,
        );

        // Create a completed task
        task.complete();
        db.insert_task(&task).unwrap();

        // Should not delete tasks completed less than 1 second ago
        let deleted = db.cleanup_old_completed(1).unwrap();
        assert_eq!(deleted, 0);

        // But should delete if we look back far enough (negative time = future)
        let deleted = db.cleanup_old_completed(-1).unwrap();
        assert_eq!(deleted, 1);
    }

    #[test]
    fn test_cleanup_old_exited() {
        let (db, _temp) = create_test_db();

        let mut task = Task::new(
            "test-exited".to_string(),
            "claude_code".to_string(),
            "Exited task".to_string(),
            None,
            None,
        );

        // Create an exited task
        task.set_exited(Some(0));
        db.insert_task(&task).unwrap();

        // Should not delete tasks exited less than 1 second ago
        let deleted = db.cleanup_old_completed(1).unwrap();
        assert_eq!(deleted, 0);

        // Should delete exited tasks when retention threshold is exceeded
        let deleted = db.cleanup_old_completed(-1).unwrap();
        assert_eq!(deleted, 1);
    }
}
