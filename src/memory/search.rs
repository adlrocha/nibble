//! Search across memory and lesson Markdown files.
//!
//! Uses ripgrep for keyword search (fast, no index needed at our scale).
//! Falls back to `grep -r` if rg is not available.

use crate::config::memory_dir;
use crate::memory::models::*;
use anyhow::Result;
use std::process::Command;

/// Search for memories matching a query string.
pub fn search_memories(
    query: &str,
    project: Option<&str>,
    memory_type: Option<&MemoryType>,
    limit: Option<usize>,
) -> Result<Vec<MemoryEntry>> {
    let dir = memory_dir().join("memories");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    // Try ripgrep first, fall back to grep
    let matching_files = rg_search(&dir, query)?;

    let mut entries: Vec<MemoryEntry> = Vec::new();
    for path in matching_files {
        let entry = match crate::memory::store::parse_memory_entry(&path) {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Apply filters
        if let Some(p) = project {
            if entry.project.as_deref() != Some(p) {
                continue;
            }
        }
        if let Some(t) = memory_type {
            if entry.memory_type != *t {
                continue;
            }
        }

        entries.push(entry);
    }

    // Sort by confidence + recency
    entries.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.created_at.cmp(&a.created_at))
    });

    if let Some(limit) = limit {
        entries.truncate(limit);
    }

    Ok(entries)
}

/// Search for lessons matching a query string (keyword-based).
pub fn search_lessons(
    query: &str,
    status: Option<&LessonStatus>,
    limit: Option<usize>,
) -> Result<Vec<LessonEntry>> {
    let dir = memory_dir().join("lessons");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let matching_files = rg_search(&dir, query)?;

    let mut entries: Vec<LessonEntry> = Vec::new();
    for path in matching_files {
        let entry = match crate::memory::store::parse_lesson_entry(&path) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if let Some(s) = status {
            if entry.status != *s {
                continue;
            }
        }

        entries.push(entry);
    }

    // Sort by severity then recency
    entries.sort_by(|a, b| {
        let sev = |s: &LessonSeverity| match s {
            LessonSeverity::Critical => 0,
            LessonSeverity::High => 1,
            LessonSeverity::Medium => 2,
            LessonSeverity::Low => 3,
        };
        sev(&a.severity)
            .cmp(&sev(&b.severity))
            .then_with(|| b.created_at.cmp(&a.created_at))
    });

    if let Some(limit) = limit {
        entries.truncate(limit);
    }

    Ok(entries)
}

/// Search lessons using context string for semantic matching.
/// In Phase 1, this uses keyword matching (semantic search deferred to Phase 3).
pub fn search_lessons_by_context(
    context: &str,
    status: Option<&LessonStatus>,
    limit: Option<usize>,
) -> Result<Vec<LessonEntry>> {
    // Phase 1: use keyword search on the context words
    // Extract significant words from the context (skip short/common words)
    let words: Vec<&str> = context
        .split_whitespace()
        .filter(|w| w.len() > 3)
        .take(5) // use top 5 significant words
        .collect();

    if words.is_empty() {
        // Fall back to listing active lessons
        return crate::memory::store::list_lessons(status, None, None, limit);
    }

    let query = words.join(" ");
    search_lessons(&query, status, limit)
}

/// Run ripgrep to find files matching a query. Falls back to grep -r.
fn rg_search(dir: &std::path::Path, query: &str) -> Result<Vec<std::path::PathBuf>> {
    // Try rg first
    let output = Command::new("rg")
        .arg("-l") // list files only
        .arg("--sortr")
        .arg("modified")
        .arg("--max-count")
        .arg("1")
        .arg(query)
        .arg(dir)
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let files: Vec<std::path::PathBuf> = stdout
                .lines()
                .filter(|l| !l.is_empty())
                .map(std::path::PathBuf::from)
                .collect();
            return Ok(files);
        }
        _ => {
            // rg not found or no matches — try grep -r
            let output = Command::new("grep").arg("-rl").arg(query).arg(dir).output();

            match output {
                Ok(out) if out.status.success() => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let files: Vec<std::path::PathBuf> = stdout
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(std::path::PathBuf::from)
                        .collect();
                    Ok(files)
                }
                _ => Ok(Vec::new()), // no matches
            }
        }
    }
}
