//! Index cache management.
//!
//! Maintains .index.json for fast listing/stats.
//! All caches are rebuildable from the Markdown source files.

use crate::config::memory_dir;
use crate::memory::models::*;
use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::fs;

/// Rebuild the .index.json cache from all Markdown files.
pub fn reindex() -> Result<()> {
    let base = memory_dir();

    let memories = crate::memory::store::list_memories(None, None, None, None)?;
    let lessons = crate::memory::store::list_lessons(None, None, None, None)?;

    let mut mem_map: HashMap<String, IndexMemoryEntry> = HashMap::new();
    let mut by_type: HashMap<String, usize> = HashMap::new();

    for m in &memories {
        let rel_path = m
            .file_path
            .strip_prefix(&base)?
            .to_string_lossy()
            .to_string();
        *by_type.entry(m.memory_type.to_string()).or_insert(0) += 1;
        mem_map.insert(
            m.memory_id.clone(),
            IndexMemoryEntry {
                path: rel_path,
                memory_type: m.memory_type.as_str().to_string(),
                project: m.project.clone(),
                title: m.title.clone(),
                tags: m.tags.clone(),
                created_at: m.created_at,
                confidence: m.confidence,
            },
        );
    }

    let mut lesson_map: HashMap<String, IndexLessonEntry> = HashMap::new();
    for l in &lessons {
        let rel_path = l
            .file_path
            .strip_prefix(&base)?
            .to_string_lossy()
            .to_string();
        lesson_map.insert(
            l.lesson_id.clone(),
            IndexLessonEntry {
                path: rel_path,
                category: l.category.as_str().to_string(),
                severity: l.severity.as_str().to_string(),
                status: l.status.as_str().to_string(),
                occurrence_count: l.occurrence_count,
            },
        );
    }

    let active_lessons = lessons
        .iter()
        .filter(|l| l.status == LessonStatus::Active)
        .count();
    let oldest = memories.iter().map(|m| m.created_at).min();
    let newest = memories.iter().map(|m| m.created_at).max();

    let cache = IndexCache {
        version: 1,
        generated_at: Utc::now(),
        memories: mem_map,
        lessons: lesson_map,
        stats: IndexStatStats {
            total_memories: memories.len(),
            total_lessons: lessons.len(),
            active_lessons,
            by_type,
            oldest,
            newest,
        },
    };

    let json = serde_json::to_string_pretty(&cache)?;
    fs::write(base.join(".index.json"), json)?;

    Ok(())
}

/// Load the index cache. Returns None if it doesn't exist or is invalid.
pub fn load_index() -> Option<IndexCache> {
    let path = memory_dir().join(".index.json");
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Regenerate the index.md table of contents.
pub fn regenerate_index_md() -> Result<()> {
    let base = memory_dir();
    let memories = crate::memory::store::list_memories(None, None, None, None)?;
    let lessons =
        crate::memory::store::list_lessons(Some(&LessonStatus::Active), None, None, None)?;
    let stats = crate::memory::store::memory_stats(None)?;

    let mut md = String::new();
    md.push_str(&format!(
        "# Memory Index\n\n*Last updated: {} · {} memories, {} active lessons*\n\n",
        Utc::now().to_rfc3339(),
        stats.total_memories,
        stats.active_lessons,
    ));

    // Recent sessions section
    md.push_str("## Recent Sessions\n\n");
    md.push_str("| Date | Project | Summary |\n");
    md.push_str("|------|---------|--------|\n");

    // List session summaries if they exist
    let sessions_dir = base.join("sessions");
    if sessions_dir.is_dir() {
        let mut session_files: Vec<std::path::PathBuf> = Vec::new();
        for entry in fs::read_dir(&sessions_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                for sub_entry in fs::read_dir(entry.path())? {
                    let sub_entry = sub_entry?;
                    let name = sub_entry.file_name().to_string_lossy().to_string();
                    if name.ends_with(".summary.md") {
                        session_files.push(sub_entry.path());
                    }
                }
            }
        }
        session_files.sort_by(|a, b| b.cmp(a));
        for path in session_files.iter().take(10) {
            let name = path.file_name().unwrap().to_string_lossy();
            let rel = path.strip_prefix(&base).unwrap_or(path);
            md.push_str(&format!("| {} | - | [link]({}) |\n", name, rel.display()));
        }
    }
    md.push('\n');

    // Active lessons section
    md.push_str(&format!("## Active Lessons ({})\n\n", lessons.len()));
    for (i, lesson) in lessons.iter().enumerate() {
        let rel = lesson
            .file_path
            .strip_prefix(&base)
            .unwrap_or(&lesson.file_path);
        md.push_str(&format!(
            "{}. **[{}]** [{}]({})\n",
            i + 1,
            lesson.severity,
            lesson
                .content
                .lines()
                .next()
                .unwrap_or("(no content)")
                .trim_end_matches('…'),
            rel.display(),
        ));
    }
    md.push('\n');

    // Key decisions
    let decisions: Vec<&MemoryEntry> = memories
        .iter()
        .filter(|m| m.memory_type == MemoryType::Decision)
        .take(15)
        .collect();
    if !decisions.is_empty() {
        md.push_str(&format!("## Key Decisions ({})\n\n", decisions.len()));
        for d in &decisions {
            let rel = d.file_path.strip_prefix(&base).unwrap_or(&d.file_path);
            let first_line = d
                .content
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect::<String>();
            md.push_str(&format!(
                "- [{}]({}) ({})\n",
                first_line,
                rel.display(),
                d.created_at.format("%Y-%m-%d"),
            ));
        }
    }

    fs::write(base.join("sessions").join("index.md"), &md)?;
    Ok(())
}
