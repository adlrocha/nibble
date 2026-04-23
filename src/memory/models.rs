//! Data types for the memory system.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Memory types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    SessionSummary,
    Decision,
    Pattern,
    UserInstruction,
    Observation,
    BugRecord,
}

impl MemoryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionSummary => "session_summary",
            Self::Decision => "decision",
            Self::Pattern => "pattern",
            Self::UserInstruction => "user_instruction",
            Self::Observation => "observation",
            Self::BugRecord => "bug_record",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "session_summary" => Self::SessionSummary,
            "decision" => Self::Decision,
            "pattern" => Self::Pattern,
            "user_instruction" => Self::UserInstruction,
            "observation" => Self::Observation,
            "bug_record" => Self::BugRecord,
            _ => Self::Observation, // default fallback
        }
    }
}

impl std::fmt::Display for MemoryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Lesson types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LessonCategory {
    SpecGap,
    ImplBug,
    TestGap,
    AuditBlindSpot,
    QaCatch,
    Process,
}

impl LessonCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SpecGap => "spec_gap",
            Self::ImplBug => "impl_bug",
            Self::TestGap => "test_gap",
            Self::AuditBlindSpot => "audit_blind_spot",
            Self::QaCatch => "qa_catch",
            Self::Process => "process",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "spec_gap" => Self::SpecGap,
            "impl_bug" => Self::ImplBug,
            "test_gap" => Self::TestGap,
            "audit_blind_spot" => Self::AuditBlindSpot,
            "qa_catch" => Self::QaCatch,
            "process" => Self::Process,
            _ => Self::ImplBug,
        }
    }
}

impl std::fmt::Display for LessonCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LessonSeverity {
    Low,
    Medium,
    High,
    Critical,
}

impl LessonSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "critical" => Self::Critical,
            _ => Self::Medium,
        }
    }
}

impl std::fmt::Display for LessonSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LessonStatus {
    Active,
    Resolved,
    Encoded,
}

impl LessonStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Resolved => "resolved",
            Self::Encoded => "encoded",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "resolved" => Self::Resolved,
            "encoded" => Self::Encoded,
            _ => Self::Active,
        }
    }
}

impl std::fmt::Display for LessonStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Entry types ───────────────────────────────────────────────────────────────

/// A memory entry, parsed from a Markdown file with YAML frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub memory_id: String,
    pub memory_type: MemoryType,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub tags: Vec<String>,
    pub confidence: f32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub access_count: u32,

    /// The Markdown content (body after frontmatter).
    #[serde(skip)]
    pub content: String,

    /// Path to the source .md file.
    #[serde(skip)]
    pub file_path: PathBuf,
}

/// A lesson entry, parsed from a Markdown file with YAML frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LessonEntry {
    pub lesson_id: String,
    pub category: LessonCategory,
    pub severity: LessonSeverity,
    pub status: LessonStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_session: Option<String>,
    pub occurrence_count: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_note: Option<String>,

    /// The Markdown content (body after frontmatter).
    #[serde(skip)]
    pub content: String,

    /// The prevention section (## Prevention in the body).
    #[serde(skip)]
    pub prevention: String,

    /// Path to the source .md file.
    #[serde(skip)]
    pub file_path: PathBuf,
}

// ── Index cache types ─────────────────────────────────────────────────────────

/// In-memory representation of .index.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexCache {
    pub version: u32,
    pub generated_at: DateTime<Utc>,
    pub memories: std::collections::HashMap<String, IndexMemoryEntry>,
    pub lessons: std::collections::HashMap<String, IndexLessonEntry>,
    pub stats: IndexStatStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMemoryEntry {
    pub path: String,
    #[serde(rename = "type")]
    pub memory_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexLessonEntry {
    pub path: String,
    pub category: String,
    pub severity: String,
    pub status: String,
    pub occurrence_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStatStats {
    pub total_memories: usize,
    pub total_lessons: usize,
    pub active_lessons: usize,
    pub by_type: std::collections::HashMap<String, usize>,
    pub oldest: Option<DateTime<Utc>>,
    pub newest: Option<DateTime<Utc>>,
}
