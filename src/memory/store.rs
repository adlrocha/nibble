//! File-based storage for memories and lessons.
//!
//! Each memory/lesson is a Markdown file with YAML frontmatter.
//! The file system is the database.

use crate::config::memory_dir;
use crate::memory::models::*;
use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

// ── YAML frontmatter wrappers ────────────────────────────────────────────────

/// Frontmatter for memory files.
#[derive(Debug, Serialize, Deserialize)]
struct MemoryFrontmatter {
    memory_id: String,
    #[serde(rename = "type")]
    memory_type: String,
    agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    tags: Vec<String>,
    confidence: f32,
    created_at: String,
    updated_at: String,
    access_count: u32,
}

/// Frontmatter for lesson files.
#[derive(Debug, Serialize, Deserialize)]
struct LessonFrontmatter {
    lesson_id: String,
    category: String,
    severity: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_session: Option<String>,
    occurrence_count: u32,
    created_at: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution_note: Option<String>,
}

/// Maximum content length for memory bodies.
/// Session summaries can be long; 8192 captures most sessions without truncation.
const MAX_CONTENT_LEN: usize = 8192;

// ── Memory operations ────────────────────────────────────────────────────────

/// Write a new memory to disk. Returns the file path.
pub fn write_memory(
    memory_type: &MemoryType,
    content: &str,
    agent: &str,
    project: Option<&str>,
    tags: &[String],
    session_id: Option<&str>,
    task_id: Option<&str>,
    confidence: Option<f32>,
    update_id: Option<&str>,
    title: Option<&str>,
) -> Result<(PathBuf, String)> {
    let now = Utc::now();
    let content = truncate_content(content);

    // If updating, find and overwrite the existing file
    if let Some(id) = update_id {
        return update_existing_memory(id, &content, title, &now);
    }

    let memory_id = Uuid::new_v4().to_string();
    let short_id = &memory_id[..8];
    let date = now.format("%Y-%m-%d").to_string();
    let filename = format!("{}_{}_{}.md", date, short_id, memory_type.as_str());

    let dir = memory_dir().join("memories");
    fs::create_dir_all(&dir)?;

    let fm = MemoryFrontmatter {
        memory_id: memory_id.clone(),
        memory_type: memory_type.as_str().to_string(),
        agent: agent.to_string(),
        session_id: session_id.map(|s| s.to_string()),
        task_id: task_id.map(|s| s.to_string()),
        project: project.map(|s| s.to_string()),
        title: title.map(|s| s.to_string()),
        tags: tags.to_vec(),
        confidence: confidence.unwrap_or(1.0).clamp(0.0, 1.0),
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
        access_count: 0,
    };

    let path = dir.join(&filename);
    write_frontmatter_file(&path, &fm, &content)?;

    Ok((path, memory_id))
}

/// Update an existing memory file by memory_id.
fn update_existing_memory(
    memory_id: &str,
    content: &str,
    title: Option<&str>,
    now: &chrono::DateTime<Utc>,
) -> Result<(PathBuf, String)> {
    let path = find_memory_file(memory_id)?
        .ok_or_else(|| anyhow::anyhow!("Memory not found: {}", memory_id))?;

    // Read existing frontmatter
    let (fm, _old_content) = read_memory_file(&path)?;
    let mut updated_fm = fm;
    updated_fm.updated_at = now.to_rfc3339();
    if let Some(t) = title {
        updated_fm.title = Some(t.to_string());
    }

    write_frontmatter_file(&path, &updated_fm, content)?;
    Ok((path, memory_id.to_string()))
}

/// Delete a memory file by memory_id.
pub fn forget_memory(memory_id: &str) -> Result<PathBuf> {
    let path = find_memory_file(memory_id)?
        .ok_or_else(|| anyhow::anyhow!("Memory not found: {}", memory_id))?;
    fs::remove_file(&path)?;
    Ok(path)
}

/// Find the file path for a memory_id by scanning the index or directory.
fn find_memory_file(memory_id: &str) -> Result<Option<PathBuf>> {
    let dir = memory_dir().join("memories");
    if !dir.is_dir() {
        return Ok(None);
    }

    // Quick check: try to find by prefix (first 8 chars of UUID used in filename)
    let short_id = &memory_id[..8.min(memory_id.len())];
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        // Filenames are: YYYY-MM-DD_SHORTID_TYPE.md
        // Match by short_id appearing in the filename, then verify frontmatter
        if name.contains(short_id) {
            if let Ok((fm, _)) = read_memory_file(&entry.path()) {
                if fm.memory_id == memory_id || fm.memory_id.starts_with(memory_id) {
                    return Ok(Some(entry.path()));
                }
            }
        }
    }

    // Fallback: scan all files
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if let Ok((fm, _)) = read_memory_file(&entry.path()) {
            if fm.memory_id == memory_id || fm.memory_id.starts_with(memory_id) {
                return Ok(Some(entry.path()));
            }
        }
    }

    Ok(None)
}

/// Read a memory file, returning frontmatter and content.
fn read_memory_file(path: &Path) -> Result<(MemoryFrontmatter, String)> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read memory file: {}", path.display()))?;
    let (fm_str, body) = parse_frontmatter(&content)?;
    let fm: MemoryFrontmatter = serde_yaml::from_str(&fm_str)
        .with_context(|| format!("Failed to parse frontmatter in: {}", path.display()))?;
    Ok((fm, body))
}

/// Parse a memory entry from a file path.
pub fn parse_memory_entry(path: &Path) -> Result<MemoryEntry> {
    let (fm, content) = read_memory_file(path)?;
    Ok(MemoryEntry {
        memory_id: fm.memory_id,
        memory_type: MemoryType::from_str_lossy(&fm.memory_type),
        agent: fm.agent,
        session_id: fm.session_id,
        task_id: fm.task_id,
        project: fm.project,
        title: fm.title,
        tags: fm.tags,
        confidence: fm.confidence,
        created_at: chrono::DateTime::parse_from_rfc3339(&fm.created_at)?.with_timezone(&Utc),
        updated_at: chrono::DateTime::parse_from_rfc3339(&fm.updated_at)?.with_timezone(&Utc),
        access_count: fm.access_count,
        content,
        file_path: path.to_path_buf(),
    })
}

/// List all memory entries, optionally filtered.
pub fn list_memories(
    project: Option<&str>,
    memory_type: Option<&MemoryType>,
    since: Option<&chrono::DateTime<Utc>>,
    limit: Option<usize>,
) -> Result<Vec<MemoryEntry>> {
    let dir = memory_dir().join("memories");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<MemoryEntry> = Vec::new();

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let mem = match parse_memory_entry(&path) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[memory] Skipping invalid file {}: {e:#}", path.display());
                continue;
            }
        };

        // Apply filters
        if let Some(p) = project {
            if mem.project.as_deref() != Some(p) {
                continue;
            }
        }
        if let Some(t) = memory_type {
            if mem.memory_type != *t {
                continue;
            }
        }
        if let Some(s) = since {
            if &mem.created_at < s {
                continue;
            }
        }

        entries.push(mem);
    }

    // Sort by created_at descending (newest first)
    entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    if let Some(limit) = limit {
        entries.truncate(limit);
    }

    Ok(entries)
}

/// Get stats about stored memories.
pub fn memory_stats(project: Option<&str>) -> Result<IndexStatStats> {
    let entries = list_memories(project, None, None, None)?;
    let lessons = list_lessons(None, None, None, None)?;

    let mut by_type: HashMap<String, usize> = HashMap::new();
    for e in &entries {
        *by_type.entry(e.memory_type.to_string()).or_insert(0) += 1;
    }

    let oldest = entries.iter().map(|e| e.created_at).min();
    let newest = entries.iter().map(|e| e.created_at).max();
    let active_lessons = lessons
        .iter()
        .filter(|l| l.status == LessonStatus::Active)
        .count();

    Ok(IndexStatStats {
        total_memories: entries.len(),
        total_lessons: lessons.len(),
        active_lessons,
        by_type,
        oldest,
        newest,
    })
}

// ── Lesson operations ────────────────────────────────────────────────────────

/// Write a new lesson to disk.
pub fn write_lesson(
    content: &str,
    prevention: &str,
    category: &LessonCategory,
    severity: &LessonSeverity,
    project: Option<&str>,
    tags: &[String],
    source_session: Option<&str>,
) -> Result<(PathBuf, String)> {
    let now = Utc::now();
    let lesson_id = Uuid::new_v4().to_string();
    let short_id = &lesson_id[..8];
    let date = now.format("%Y-%m-%d").to_string();
    let filename = format!("{}_{}_{}.md", date, short_id, category.as_str());

    let dir = memory_dir().join("lessons");
    fs::create_dir_all(&dir)?;

    let fm = LessonFrontmatter {
        lesson_id: lesson_id.clone(),
        category: category.as_str().to_string(),
        severity: severity.as_str().to_string(),
        status: LessonStatus::Active.as_str().to_string(),
        project: project.map(|s| s.to_string()),
        tags: tags.to_vec(),
        source_session: source_session.map(|s| s.to_string()),
        occurrence_count: 1,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
        resolved_at: None,
        resolution_note: None,
    };

    // Build body with content + optional Prevention section
    let body = if prevention.is_empty() {
        truncate_content(content)
    } else {
        format!(
            "{}\n\n## Prevention\n\n{}",
            truncate_content(content),
            prevention
        )
    };

    let path = dir.join(&filename);
    write_frontmatter_file(&path, &fm, &body)?;

    Ok((path, lesson_id))
}

/// Resolve a lesson by lesson_id.
pub fn resolve_lesson(lesson_id: &str, note: Option<&str>) -> Result<PathBuf> {
    let path = find_lesson_file(lesson_id)?
        .ok_or_else(|| anyhow::anyhow!("Lesson not found: {}", lesson_id))?;

    let content = fs::read_to_string(&path)?;
    let (fm_str, body) = parse_frontmatter(&content)?;
    let mut fm: LessonFrontmatter = serde_yaml::from_str(&fm_str)?;

    fm.status = LessonStatus::Resolved.as_str().to_string();
    fm.updated_at = Utc::now().to_rfc3339();
    fm.resolved_at = Some(Utc::now().to_rfc3339());
    if let Some(n) = note {
        fm.resolution_note = Some(n.to_string());
    }

    write_frontmatter_file(&path, &fm, &body)?;
    Ok(path)
}

/// Find lesson file by lesson_id.
fn find_lesson_file(lesson_id: &str) -> Result<Option<PathBuf>> {
    let dir = memory_dir().join("lessons");
    if !dir.is_dir() {
        return Ok(None);
    }

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let content = fs::read_to_string(entry.path())?;
        let (fm_str, _) = parse_frontmatter(&content)?;
        let fm: LessonFrontmatter = match serde_yaml::from_str(&fm_str) {
            Ok(fm) => fm,
            Err(_) => continue,
        };
        if fm.lesson_id == lesson_id {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

/// Parse a lesson entry from a file path.
pub fn parse_lesson_entry(path: &Path) -> Result<LessonEntry> {
    let content = fs::read_to_string(path)?;
    let (fm_str, body) = parse_frontmatter(&content)?;
    let fm: LessonFrontmatter = serde_yaml::from_str(&fm_str)?;

    // Split body into content and prevention
    let (lesson_content, prevention) = if let Some(idx) = body.find("## Prevention") {
        (
            body[..idx].trim().to_string(),
            body[idx + "## Prevention".len()..].trim().to_string(),
        )
    } else {
        (body, String::new())
    };

    Ok(LessonEntry {
        lesson_id: fm.lesson_id,
        category: LessonCategory::from_str_lossy(&fm.category),
        severity: LessonSeverity::from_str_lossy(&fm.severity),
        status: LessonStatus::from_str_lossy(&fm.status),
        project: fm.project,
        tags: fm.tags,
        source_session: fm.source_session,
        occurrence_count: fm.occurrence_count,
        created_at: chrono::DateTime::parse_from_rfc3339(&fm.created_at)?.with_timezone(&Utc),
        updated_at: chrono::DateTime::parse_from_rfc3339(&fm.updated_at)?.with_timezone(&Utc),
        resolved_at: fm
            .resolved_at
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        resolution_note: fm.resolution_note,
        content: lesson_content,
        prevention,
        file_path: path.to_path_buf(),
    })
}

/// List all lesson entries, optionally filtered.
pub fn list_lessons(
    status: Option<&LessonStatus>,
    severity: Option<&LessonSeverity>,
    project: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<LessonEntry>> {
    let dir = memory_dir().join("lessons");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<LessonEntry> = Vec::new();

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let lesson = match parse_lesson_entry(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[memory] Skipping invalid lesson {}: {e:#}", path.display());
                continue;
            }
        };

        if let Some(s) = status {
            if lesson.status != *s {
                continue;
            }
        }
        if let Some(s) = severity {
            if lesson.severity != *s {
                continue;
            }
        }
        if let Some(p) = project {
            if lesson.project.as_deref() != Some(p) {
                continue;
            }
        }

        entries.push(lesson);
    }

    // Sort: critical first, then high, medium, low; within severity, newest first
    entries.sort_by(|a, b| {
        let sev_order = |s: &LessonSeverity| match s {
            LessonSeverity::Critical => 0,
            LessonSeverity::High => 1,
            LessonSeverity::Medium => 2,
            LessonSeverity::Low => 3,
        };
        sev_order(&a.severity)
            .cmp(&sev_order(&b.severity))
            .then_with(|| b.created_at.cmp(&a.created_at))
    });

    if let Some(limit) = limit {
        entries.truncate(limit);
    }

    Ok(entries)
}

// ── File helpers ─────────────────────────────────────────────────────────────

/// Parse YAML frontmatter from a Markdown string.
/// Returns (frontmatter_yaml, body).
fn parse_frontmatter(content: &str) -> Result<(String, String)> {
    let mut byte_pos = 0;
    let mut lines = content.lines();

    // First line must be ---
    let first = lines.next();
    if first.unwrap_or("").trim() != "---" {
        anyhow::bail!("Missing opening --- frontmatter delimiter");
    }
    byte_pos += first.unwrap().len() + 1; // +1 for the newline

    let mut fm_lines: Vec<&str> = Vec::new();
    for line in lines {
        byte_pos += line.len() + 1; // +1 for newline
        if line.trim() == "---" {
            // Found closing delimiter — body starts after this line
            let body = content[byte_pos..]
                .trim_start_matches('\n')
                .trim()
                .to_string();
            return Ok((fm_lines.join("\n"), body));
        }
        fm_lines.push(line);
    }

    anyhow::bail!("Missing closing --- frontmatter delimiter")
}

/// Write a file with YAML frontmatter.
fn write_frontmatter_file(path: &Path, frontmatter: &impl Serialize, body: &str) -> Result<()> {
    let fm_yaml =
        serde_yaml::to_string(frontmatter).with_context(|| "Failed to serialize frontmatter")?;

    let mut file = fs::File::create(path)
        .with_context(|| format!("Failed to create file: {}", path.display()))?;

    write!(file, "---\n{fm_yaml}---\n\n{body}\n")?;

    Ok(())
}

/// Truncate content to MAX_CONTENT_LEN chars with ellipsis.
fn truncate_content(content: &str) -> String {
    if content.len() <= MAX_CONTENT_LEN {
        content.to_string()
    } else {
        let mut end = MAX_CONTENT_LEN;
        // Try to break at a word boundary
        if let Some(pos) = content[..end].rfind(' ') {
            if pos > MAX_CONTENT_LEN / 2 {
                end = pos;
            }
        }
        format!("{}…", &content[..end])
    }
}
