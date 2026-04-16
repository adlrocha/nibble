use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

/// The agent that owns / is running a task.
///
/// New variants should be added here as new agent integrations land.
/// `Unknown` is an open-world fallback so existing DB rows with unrecognised
/// values deserialize without error and round-trip losslessly.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum AgentType {
    /// Claude Code (Anthropic) — the default agent.
    #[default]
    ClaudeCode,
    /// OpenCode — open-source coding agent.
    OpenCode,
    /// Any agent type not yet known to this binary.  Stores the raw string so
    /// `as_str()` / serialization round-trips perfectly.
    Unknown(String),
}

impl AgentType {
    /// Canonical wire/DB string for this agent type.
    pub fn as_str(&self) -> &str {
        match self {
            AgentType::ClaudeCode => "claude_code",
            AgentType::OpenCode => "opencode",
            AgentType::Unknown(s) => s.as_str(),
        }
    }
}

impl FromStr for AgentType {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "claude_code" => AgentType::ClaudeCode,
            "opencode" => AgentType::OpenCode,
            other => AgentType::Unknown(other.to_string()),
        })
    }
}

impl fmt::Display for AgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// Manual serde impls so the wire format goes through as_str / from_str,
// matching the same pattern used by TaskStatus and SandboxType.
impl Serialize for AgentType {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentType {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(AgentType::from_str(&s).unwrap()) // infallible
    }
}

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
            image: "nibble-sandbox:latest".to_string(),
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
    /// Deprecated — kept for reading legacy rows only; new code writes the typed
    /// fields below. Omitted from serialization when `None` so new rows stay clean.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Session ID for Claude Code (UUID format, used with `--resume` / `--session-id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_session_id: Option<String>,
    /// Session ID for opencode (`ses_...` format, used with `--session`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opencode_session_id: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Option<i64>,
    pub task_id: String,
    pub agent_type: AgentType,
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
        agent_type: AgentType,
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
    #[cfg(test)]
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

/// Cron job for scheduled prompts to sandboxes
#[derive(Debug, Clone)]
pub struct CronJob {
    pub id: Option<i64>,
    /// Canonical absolute path of the repo this job targets.
    /// At trigger time nibble finds or spawns a sandbox for this path.
    pub repo_path: String,
    pub label: Option<String>,
    pub schedule: String,
    pub prompt: String,
    pub enabled: bool,
    pub skip_if_running: bool,
    /// True while a background injection thread is running for this job.
    /// Prevents overlap when skip_if_running is set.
    pub running: bool,
    pub last_run: Option<DateTime<Utc>>,
    pub next_run: DateTime<Utc>,
    /// Optional expiry: job is auto-disabled after this datetime.
    pub expires_at: Option<DateTime<Utc>>,
    #[allow(dead_code)]
    pub created_at: DateTime<Utc>,
}

impl CronJob {
    pub fn new(
        repo_path: String,
        schedule: String,
        prompt: String,
        label: Option<String>,
        next_run: DateTime<Utc>,
    ) -> Self {
        Self {
            id: None,
            repo_path,
            label,
            schedule,
            prompt,
            enabled: true,
            skip_if_running: true,
            running: false,
            last_run: None,
            next_run,
            expires_at: None,
            created_at: Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_creation() {
        let task = Task::new(
            "test-id".to_string(),
            AgentType::ClaudeCode,
            "Test task".to_string(),
            Some(1234),
            Some(1233),
        );

        assert_eq!(task.task_id, "test-id");
        assert_eq!(task.agent_type, AgentType::ClaudeCode);
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.pid, Some(1234));
    }

    #[test]
    fn test_title_truncation() {
        let long_title = "a".repeat(150);
        let task = Task::new(
            "test-id".to_string(),
            AgentType::ClaudeCode,
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
            AgentType::ClaudeCode,
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
            AgentType::ClaudeCode,
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
            AgentType::ClaudeCode,
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
        assert_eq!(
            TaskStatus::from_str("running").unwrap(),
            TaskStatus::Running
        );
        assert_eq!(
            TaskStatus::from_str("completed").unwrap(),
            TaskStatus::Completed
        );
        assert_eq!(TaskStatus::from_str("exited").unwrap(), TaskStatus::Exited);
        // Legacy support
        assert_eq!(
            TaskStatus::from_str("needs_attention").unwrap(),
            TaskStatus::Completed
        );
        assert_eq!(TaskStatus::from_str("failed").unwrap(), TaskStatus::Exited);
        assert!(TaskStatus::from_str("invalid").is_err());
    }

    // ── AgentType tests ────────────────────────────────────────────────────────

    /// AC-1: known string "claude_code" parses to ClaudeCode
    #[test]
    fn test_ac1_agent_type_from_str_claude() {
        assert_eq!(
            AgentType::from_str("claude_code").unwrap(),
            AgentType::ClaudeCode
        );
    }

    /// AC-2: known string "opencode" parses to OpenCode
    #[test]
    fn test_ac2_agent_type_from_str_opencode() {
        assert_eq!(
            AgentType::from_str("opencode").unwrap(),
            AgentType::OpenCode
        );
    }

    /// AC-3: unknown string becomes Unknown variant (infallible)
    #[test]
    fn test_ac3_agent_type_from_str_unknown() {
        assert_eq!(
            AgentType::from_str("my_new_agent").unwrap(),
            AgentType::Unknown("my_new_agent".to_string())
        );
    }

    /// AC-4: as_str round-trips for all known variants
    #[test]
    fn test_ac4_agent_type_as_str() {
        assert_eq!(AgentType::ClaudeCode.as_str(), "claude_code");
        assert_eq!(AgentType::OpenCode.as_str(), "opencode");
        assert_eq!(AgentType::Unknown("my_bot".to_string()).as_str(), "my_bot");
    }

    /// INV-1: from_str(as_str()) round-trips for all variants
    #[test]
    fn test_inv1_agent_type_round_trip() {
        let variants = [
            AgentType::ClaudeCode,
            AgentType::OpenCode,
            AgentType::Unknown("future_agent".to_string()),
        ];
        for v in &variants {
            assert_eq!(
                AgentType::from_str(v.as_str()).unwrap(),
                *v,
                "round-trip failed for {:?}",
                v
            );
        }
    }

    /// INV-2: from_str is infallible — any string produces Ok(Unknown(_))
    #[test]
    fn test_inv2_agent_type_from_str_infallible() {
        for s in &["", "   ", "CLAUDE_CODE", "openCode", "🤖", "a/b"] {
            let result = AgentType::from_str(s);
            assert!(
                result.is_ok(),
                "from_str should never Err, got Err for {:?}",
                s
            );
        }
    }
}
