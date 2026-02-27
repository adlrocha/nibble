use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

/// Type of sandbox environment
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SandboxType {
    /// No sandbox - runs directly on host
    #[default]
    None,
    /// Rootless Podman container
    Podman,
}

impl SandboxType {
    pub fn as_str(&self) -> &str {
        match self {
            SandboxType::None => "none",
            SandboxType::Podman => "podman",
        }
    }
}

impl std::str::FromStr for SandboxType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(SandboxType::None),
            "podman" => Ok(SandboxType::Podman),
            _ => Err(format!("Invalid sandbox type: {}", s)),
        }
    }
}

/// Configuration for sandbox environment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Container image to use
    pub image: String,
    /// Port ranges to expose (e.g., "3000-3100,8000-8100")
    pub port_ranges: Vec<String>,
    /// Environment variables to pass to container
    pub env_vars: HashMap<String, String>,
    /// Whether to run in privileged mode (more flexible, less isolated)
    pub privileged: bool,
    /// CPU limit (e.g., "2" for 2 cores)
    pub cpu_limit: Option<String>,
    /// Memory limit (e.g., "4g" for 4GB)
    pub memory_limit: Option<String>,
    /// Additional volume mounts (host_path:container_path)
    pub extra_volumes: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            image: "agent-inbox-sandbox:latest".to_string(),
            port_ranges: vec!["3000-3100".to_string(), "8000-8100".to_string()],
            env_vars: HashMap::new(),
            privileged: true, // Default to privileged for flexibility
            cpu_limit: None,
            memory_limit: None,
            extra_volumes: vec![],
        }
    }
}

/// Task status - simplified to 3 states for reliability
/// - Running: Agent is actively generating output
/// - Completed: Agent finished generating, waiting for user input
/// - Exited: Agent/tab closed or process terminated
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TaskStatus {
    Running,
    Completed,
    Exited,
}

impl TaskStatus {
    pub fn as_str(&self) -> &str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Completed => "completed",
            TaskStatus::Exited => "exited",
        }
    }
}

impl FromStr for TaskStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(TaskStatus::Running),
            "completed" => Ok(TaskStatus::Completed),
            "exited" => Ok(TaskStatus::Exited),
            // Legacy support
            "needs_attention" => Ok(TaskStatus::Completed),
            "failed" => Ok(TaskStatus::Exited),
            _ => Err(format!("Invalid task status: {}", s)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskContext {
    pub url: Option<String>,
    pub project_path: Option<String>,
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Option<i64>,
    pub task_id: String,
    pub agent_type: String,
    pub title: String,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub pid: Option<i32>,
    pub ppid: Option<i32>,
    pub monitor_pid: Option<i32>,
    pub attention_reason: Option<String>,
    pub exit_code: Option<i32>,
    pub context: Option<TaskContext>,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    // Sandbox fields (new in schema v3)
    pub container_id: Option<String>,
    #[serde(default)]
    pub sandbox_type: SandboxType,
    pub sandbox_config: Option<SandboxConfig>,
}

impl Task {
    pub fn new(
        task_id: String,
        agent_type: String,
        title: String,
        pid: Option<i32>,
        ppid: Option<i32>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: None,
            task_id,
            agent_type,
            title: Self::truncate_title(&title, 100),
            status: TaskStatus::Running,
            created_at: now,
            updated_at: now,
            completed_at: None,
            pid,
            ppid,
            monitor_pid: None,
            attention_reason: None,
            exit_code: None,
            context: None,
            metadata: None,
            // Sandbox fields default to None/none
            container_id: None,
            sandbox_type: SandboxType::None,
            sandbox_config: None,
        }
    }

    fn truncate_title(title: &str, max_len: usize) -> String {
        if title.len() <= max_len {
            title.to_string()
        } else {
            format!("{}...", &title[..max_len.saturating_sub(3)])
        }
    }

    /// Mark task as completed (finished generating, waiting for user)
    pub fn complete(&mut self) {
        self.status = TaskStatus::Completed;
        self.completed_at = Some(Utc::now());
        self.updated_at = Utc::now();
    }

    /// Mark task as running (actively generating)
    #[allow(dead_code)]
    pub fn set_running(&mut self) {
        self.status = TaskStatus::Running;
        self.completed_at = None;
        self.updated_at = Utc::now();
    }

    /// Mark task as exited (closed/terminated)
    pub fn set_exited(&mut self, exit_code: Option<i32>) {
        self.status = TaskStatus::Exited;
        self.exit_code = exit_code;
        self.completed_at = Some(Utc::now());
        self.updated_at = Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_creation() {
        let task = Task::new(
            "test-id".to_string(),
            "claude_code".to_string(),
            "Test task".to_string(),
            Some(1234),
            Some(1233),
        );

        assert_eq!(task.task_id, "test-id");
        assert_eq!(task.agent_type, "claude_code");
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.pid, Some(1234));
    }

    #[test]
    fn test_title_truncation() {
        let long_title = "a".repeat(150);
        let task = Task::new(
            "test-id".to_string(),
            "claude_code".to_string(),
            long_title,
            None,
            None,
        );

        assert_eq!(task.title.len(), 100);
        assert!(task.title.ends_with("..."));
    }

    #[test]
    fn test_task_complete() {
        let mut task = Task::new(
            "test-id".to_string(),
            "claude_code".to_string(),
            "Test task".to_string(),
            None,
            None,
        );

        task.complete();
        assert_eq!(task.status, TaskStatus::Completed);
        assert!(task.completed_at.is_some());
    }

    #[test]
    fn test_task_exited() {
        let mut task = Task::new(
            "test-id".to_string(),
            "claude_code".to_string(),
            "Test task".to_string(),
            None,
            None,
        );

        task.set_exited(Some(1));
        assert_eq!(task.status, TaskStatus::Exited);
        assert_eq!(task.exit_code, Some(1));
    }

    #[test]
    fn test_task_resume() {
        let mut task = Task::new(
            "test-id".to_string(),
            "claude_code".to_string(),
            "Test task".to_string(),
            None,
            None,
        );

        task.complete();
        assert_eq!(task.status, TaskStatus::Completed);

        task.set_running();
        assert_eq!(task.status, TaskStatus::Running);
        assert!(task.completed_at.is_none());
    }

    #[test]
    fn test_status_serialization() {
        assert_eq!(TaskStatus::Running.as_str(), "running");
        assert_eq!(TaskStatus::Completed.as_str(), "completed");
        assert_eq!(TaskStatus::Exited.as_str(), "exited");
    }

    #[test]
    fn test_status_deserialization() {
        assert_eq!(TaskStatus::from_str("running").unwrap(), TaskStatus::Running);
        assert_eq!(TaskStatus::from_str("completed").unwrap(), TaskStatus::Completed);
        assert_eq!(TaskStatus::from_str("exited").unwrap(), TaskStatus::Exited);
        // Legacy support
        assert_eq!(TaskStatus::from_str("needs_attention").unwrap(), TaskStatus::Completed);
        assert_eq!(TaskStatus::from_str("failed").unwrap(), TaskStatus::Exited);
        assert!(TaskStatus::from_str("invalid").is_err());
    }
}
