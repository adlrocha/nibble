//! Session summarization: read capture JSONL, extract structured memories.
//!
//! When `nibble memory summarize <task-id>` runs:
//! 1. Read the capture JSONL for that session
//! 2. Extract a title and summary from the transcript heuristically
//! 3. Write a session_summary memory Markdown file
//! 4. Update .index.json and regenerate index.md
//! 5. Git commit
//!
//! NOTE: LLM-based extraction is disabled in this version. It was unreliable
//! with local models (Qwen 3.6 via llama.cpp) and produced malformed JSON or
//! thinking-tag noise. A future version may re-enable it with JSON-schema
//! constraints or a dedicated extraction model. See TODOs below.

use crate::config;
use crate::memory::git;
use crate::memory::index;
use crate::memory::llm::{LlmClient, Message};
use crate::memory::models::CaptureEvent;
use crate::memory::models::*;
use crate::memory::store;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

/// Maximum events to include in the extraction prompt.
const MAX_EVENTS: usize = 100;
/// Maximum characters per event in the prompt.
const MAX_EVENT_LEN: usize = 800;

/// Summarize a session from its capture JSONL.
///
/// Reads capture events, extracts a summary heuristically, writes files,
/// updates caches, and optionally commits to git.
pub fn summarize_session(task_id: &str, force: bool) -> Result<usize> {
    let capture_file = find_capture_file(task_id)?;
    let events = match capture_file {
        Some(path) => read_capture_jsonl(&path)?,
        None => {
            eprintln!("[summarize] No capture file found for task {task_id}");
            return Ok(0);
        }
    };
    summarize_from_events(task_id, &events, force)
}

/// Summarize a pi session from its JSONL file.
///
/// Reads the pi session file, converts it to capture events, and feeds it
/// through the same extraction pipeline as Claude Code capture files.
pub fn summarize_pi_session(
    task_id: &str,
    pi_session_path: &std::path::Path,
    force: bool,
) -> Result<usize> {
    let content = fs::read_to_string(pi_session_path)
        .with_context(|| format!("Failed to read pi session: {}", pi_session_path.display()))?;
    let events = pi_session_to_capture_events(&content);
    if events.is_empty() {
        eprintln!("[summarize] Pi session file is empty for task {task_id}");
        return Ok(0);
    }
    eprintln!(
        "[summarize] Found {} events in pi session for task {}",
        events.len(),
        task_id
    );
    summarize_from_events(task_id, &events, force)
}

/// Core summarization: extract a session_summary from capture events.
fn summarize_from_events(task_id: &str, events: &[CaptureEvent], force: bool) -> Result<usize> {
    let base = config::memory_dir();

    if events.is_empty() {
        eprintln!("[summarize] No events for task {task_id}");
        return Ok(0);
    }

    // Deduplication: skip if a session_summary already exists for this task
    if !force {
        if let Some(existing) = find_existing_session_summary(task_id)? {
            eprintln!(
                "[summarize] Session summary already exists for {} (memory {}). Use --force to overwrite.",
                task_id,
                &existing.memory_id[..8]
            );
            return Ok(0);
        }
    }

    // Heuristic extraction (LLM extraction disabled — see module doc)
    let extracted = heuristic_extraction(events);

    if extracted.memories.is_empty() {
        eprintln!("[summarize] Nothing worth remembering in this session.");
        return Ok(0);
    }

    // Resolve agent type: prefer env var, fall back to task DB record
    let mut agent = std::env::var("NIBBLE_AGENT_TYPE").unwrap_or_else(|_| "unknown".to_string());
    if agent == "unknown" {
        if let Ok(db) = crate::db::Database::open(&crate::db::default_db_path()) {
            if let Ok(Some(task)) = db.get_task_by_id(task_id) {
                agent = task.agent_type.as_str().to_string();
            }
        }
    }

    // Write memory files
    let mut written = 0;
    let project = infer_project_from_events(events);

    for mem in &extracted.memories {
        let mt = MemoryType::from_str_lossy(&mem.memory_type);
        let tags: Vec<String> = mem
            .tags
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let confidence = mem.confidence.clamp(0.0, 1.0);

        let (path, id) = store::write_memory(
            &mt,
            &mem.content,
            &agent,
            project.as_deref(),
            &tags,
            Some(task_id),
            None,
            Some(confidence),
            None,
            mem.title.as_deref(),
        )?;
        eprintln!("[summarize] Written memory: {} ({})", id, path.display());
        written += 1;
    }

    for lesson in &extracted.lessons {
        let cat = LessonCategory::from_str_lossy(&lesson.category);
        let sev = LessonSeverity::from_str_lossy(&lesson.severity);
        let tags: Vec<String> = lesson
            .tags
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let (path, id) = store::write_lesson(
            &lesson.content,
            &lesson.prevention,
            &cat,
            &sev,
            project.as_deref(),
            &tags,
            Some(task_id),
        )?;
        eprintln!("[summarize] Written lesson: {} ({})", id, path.display());
        written += 1;
    }

    // Archive the original agent session file as a standalone backup
    let _ = crate::memory::archive::archive_session(task_id);

    // Also archive the capture file if it exists
    if let Ok(Some(capture_path)) = find_capture_file(task_id) {
        if let Err(e) = crate::memory::archive::archive_from_path(&capture_path, &agent, task_id) {
            eprintln!("[summarize] Failed to archive capture: {}", e);
        }
    }

    // Update caches
    let _ = index::reindex();
    let _ = index::regenerate_index_md();

    let cfg = config::load().unwrap_or_default();
    let _ = git::commit(
        &base,
        &format!(
            "memory: summarize session {}",
            &task_id[..task_id.len().min(8)]
        ),
        &cfg.memory.sync.author_name,
        &cfg.memory.sync.author_email,
    );

    if cfg.memory.sync.auto_sync && !cfg.memory.sync.remote.is_empty() {
        let _ = git::sync(
            &base,
            &format!(
                "memory: sync after summarizing {}",
                &task_id[..task_id.len().min(8)]
            ),
            &cfg.memory.sync.author_name,
            &cfg.memory.sync.author_email,
        );
    }

    Ok(written)
}

// ── Capture file handling ────────────────────────────────────────────────────

/// Find the capture JSONL file for a task.
/// Searches in ~/.nibble/memory/capture/<project>/<task-id>.jsonl
fn find_capture_file(task_id: &str) -> Result<Option<PathBuf>> {
    let base = config::memory_dir().join("capture");
    if !base.is_dir() {
        return Ok(None);
    }

    // Search all project subdirectories for <task-id>.jsonl
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let candidate = entry.path().join(format!("{}.jsonl", task_id));
        if candidate.exists() {
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

/// Read a JSONL capture file into a list of events.
fn read_capture_jsonl(path: &PathBuf) -> Result<Vec<CaptureEvent>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read capture file: {}", path.display()))?;

    let mut events = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<CaptureEvent>(line) {
            Ok(event) => events.push(event),
            Err(e) => {
                eprintln!("[summarize] Skipping malformed capture line: {e}");
            }
        }
    }

    Ok(events)
}

/// Infer the project name from capture events.
fn infer_project_from_events(_events: &[CaptureEvent]) -> Option<String> {
    // The capture file is stored under capture/<project>/<task-id>.jsonl,
    // but we don't have that context here. Best effort: look for common patterns.
    None
}

// ── LLM extraction ───────────────────────────────────────────────────────────

/// Structured response from the heuristic/LLM extraction.
#[derive(Debug, Deserialize)]
struct ExtractedMemory {
    memory_type: String,
    content: String,
    tags: String,
    confidence: f32,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExtractedLesson {
    category: String,
    severity: String,
    content: String,
    prevention: String,
    tags: String,
}

#[derive(Debug, Deserialize)]
struct ExtractionResult {
    #[serde(default)]
    memories: Vec<ExtractedMemory>,
    #[serde(default)]
    lessons: Vec<ExtractedLesson>,
}

// TODO: Re-enable LLM extraction once we have a reliable local model
// or JSON-schema/grammar support. Qwen 3.6 via llama.cpp returns
// thinking tags and malformed JSON that breaks parsing.
// See module doc for details.
#[allow(dead_code)]
fn call_llm_extraction(llm: &LlmClient, prompt: &str) -> Result<ExtractionResult> {
    let messages = vec![
        Message {
            role: "system".to_string(),
            content: EXTRACTION_SYSTEM_PROMPT.to_string(),
        },
        Message {
            role: "user".to_string(),
            content: prompt.to_string(),
        },
    ];

    let response = llm.chat_completion(messages, 0.3)?;

    // Try to extract JSON from the response (it may be wrapped in markdown code fences)
    let json_str = extract_json_from_markdown(&response);

    let result: ExtractionResult = serde_json::from_str(&json_str).with_context(|| {
        format!(
            "Failed to parse LLM extraction response: {}",
            &response[..200.min(response.len())]
        )
    })?;

    Ok(result)
}

#[allow(dead_code)]
const EXTRACTION_SYSTEM_PROMPT: &str = r#"You are a memory extraction system. Given a coding session transcript, extract structured memories that would be useful for future sessions.

For each memory, provide:
- type: "session_summary" | "decision" | "pattern" | "user_instruction" | "observation" | "bug_record"
- content: concise description (max 500 words)
- tags: relevant technology/topic tags (comma-separated)
- confidence: 0.0 to 1.0

Rules:
- session_summary: exactly ONE per transcript. What was accomplished? Current state?
- decision: "Chose X because Y" — architectural or implementation choices.
- user_instruction: "Always do X" / "Never do Y" — only when USER states a preference to remember.
- pattern: Recurring patterns in codebase or workflow.
- observation: Factual notes about codebase, config, or environment.
- bug_record: Bugs found and how they were resolved.

Also identify any lessons learned:
- Things that went wrong and how to prevent them
- Mistakes that were caught and corrected
- Knowledge that would have helped earlier

Each lesson has:
- category: "spec_gap" | "impl_bug" | "test_gap" | "audit_blind_spot" | "qa_catch" | "process"
- severity: "low" | "medium" | "high" | "critical"
- content: what went wrong
- prevention: how to prevent it next time
- tags: comma-separated

Respond in JSON. No markdown code fences. Just raw JSON:
{"memories": [...], "lessons": [...]}
If nothing worth remembering, return {"memories": [], "lessons": []}."#;

#[allow(dead_code)]
fn build_extraction_prompt(events: &[CaptureEvent], _task_id: &str) -> String {
    let mut prompt =
        String::from("Here is a coding session transcript. Extract memories and lessons.\n\n");

    // Take the last MAX_EVENTS, truncated
    let start = events.len().saturating_sub(MAX_EVENTS);
    for (i, event) in events.iter().enumerate().skip(start) {
        let text = match event.role.as_str() {
            "user" => format!("[User]: {}", truncate(&event.content, MAX_EVENT_LEN)),
            "assistant" => format!("[Assistant]: {}", truncate(&event.content, MAX_EVENT_LEN)),
            "tool" => format!(
                "[Tool: {}]\ninput: {}\noutput: {}",
                event.name,
                truncate(&event.input, MAX_EVENT_LEN / 2),
                truncate(&event.output, MAX_EVENT_LEN / 2)
            ),
            "system" => format!("[System]: {}", truncate(&event.content, MAX_EVENT_LEN)),
            _ => format!(
                "[{}]: {}",
                event.role,
                truncate(&event.content, MAX_EVENT_LEN)
            ),
        };
        prompt.push_str(&format!("Turn {}:\n{}\n\n", i + 1, text));
    }

    prompt.push_str("Extract memories and lessons from this session.");
    prompt
}

#[allow(dead_code)]
fn extract_json_from_markdown(text: &str) -> String {
    // If the response is wrapped in ```json ... ```, extract the inner JSON
    if let Some(start) = text.find("```json") {
        if let Some(end) = text[start + 7..].find("```") {
            return text[start + 7..start + 7 + end].trim().to_string();
        }
    }
    // Also try plain ``` ... ```
    if let Some(start) = text.find("```") {
        if let Some(end) = text[start + 3..].find("```") {
            let inner = text[start + 3..start + 3 + end].trim();
            if inner.starts_with('{') {
                return inner.to_string();
            }
        }
    }
    text.trim().to_string()
}

/// Convert a pi session JSONL file into the standard capture-event format.
///
/// Reads pi's native JSONL format and transcodes `message`, `toolCall`, and
/// `toolResult` entries into the same `CaptureEvent` shape used by Claude Code
/// capture files so both agents share one summarization pipeline.
pub fn pi_session_to_capture_events(content: &str) -> Vec<CaptureEvent> {
    let mut events = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event_type = val
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        match event_type {
            "message" => {
                if let Some(msg) = val.get("message") {
                    let role = msg
                        .get("role")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let mut text_parts: Vec<String> = Vec::new();
                    if let Some(content) = msg.get("content") {
                        if let Some(arr) = content.as_array() {
                            for item in arr {
                                if let Some(txt) = item.get("text").and_then(|v| v.as_str()) {
                                    text_parts.push(txt.to_string());
                                }
                                if let Some(thinking) =
                                    item.get("thinking").and_then(|v| v.as_str())
                                {
                                    text_parts.push(format!("[thinking]\n{thinking}"));
                                }
                            }
                        } else if let Some(txt) = content.as_str() {
                            text_parts.push(txt.to_string());
                        }
                    }
                    let ts = val
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    events.push(CaptureEvent {
                        ts,
                        role: role.to_string(),
                        content: text_parts.join("\n"),
                        name: String::new(),
                        input: String::new(),
                        output: String::new(),
                    });
                }
            }
            "toolCall" => {
                let name = val
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = val
                    .get("arguments")
                    .map(|a| serde_json::to_string(a).unwrap_or_default())
                    .unwrap_or_default();
                let ts = val
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                events.push(CaptureEvent {
                    ts,
                    role: "tool".to_string(),
                    content: String::new(),
                    name,
                    input,
                    output: String::new(),
                });
            }
            "toolResult" => {
                let output = val
                    .get("output")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let ts = val
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                events.push(CaptureEvent {
                    ts,
                    role: "tool".to_string(),
                    content: String::new(),
                    name: String::new(),
                    input: String::new(),
                    output,
                });
            }
            _ => {}
        }
    }
    events
}

// ── Heuristic fallback extraction ────────────────────────────────────────────

fn find_existing_session_summary(session_id: &str) -> Result<Option<MemoryEntry>> {
    let all = store::list_memories(None, Some(&MemoryType::SessionSummary), None, None)?;
    Ok(all
        .into_iter()
        .find(|m| m.session_id.as_deref() == Some(session_id)))
}

fn heuristic_extraction(events: &[CaptureEvent]) -> ExtractionResult {
    let mut memories = Vec::new();
    let lessons = Vec::new();

    // Heuristic 1: Look for explicit "remember" or "note" from user
    for event in events {
        if event.role == "user" {
            let content_lower = event.content.to_lowercase();
            if content_lower.contains("remember")
                || content_lower.contains("note:")
                || content_lower.contains("important:")
            {
                memories.push(ExtractedMemory {
                    memory_type: "user_instruction".to_string(),
                    content: event.content.clone(),
                    tags: "user-preference".to_string(),
                    confidence: 0.9,
                    title: None,
                });
            }
        }
    }

    // Heuristic 2: Build a session summary with a real title
    let (title, summary) = build_session_summary(events);
    if !summary.is_empty() {
        memories.push(ExtractedMemory {
            memory_type: "session_summary".to_string(),
            content: summary,
            tags: "session-summary".to_string(),
            confidence: 0.75,
            title: Some(title),
        });
    }

    ExtractionResult { memories, lessons }
}

/// Build a session summary with a human-readable title.
/// Returns (title, full_summary_text).
fn build_session_summary(events: &[CaptureEvent]) -> (String, String) {
    let user_msgs: Vec<&CaptureEvent> = events.iter().filter(|e| e.role == "user").collect();

    let assistant_msgs: Vec<&CaptureEvent> =
        events.iter().filter(|e| e.role == "assistant").collect();

    let tool_events: Vec<&CaptureEvent> = events.iter().filter(|e| e.role == "tool").collect();

    // Title: first user message, truncated to ~100 chars
    let title = user_msgs
        .first()
        .map(|e| truncate(&e.content, 100))
        .or_else(|| assistant_msgs.first().map(|e| truncate(&e.content, 100)))
        .unwrap_or_else(|| "Untitled session".to_string());

    let mut summary = String::new();
    summary.push_str(&format!(
        "**Turns**: {} user, {} assistant, {} tool calls\n\n",
        user_msgs.len(),
        assistant_msgs.len(),
        tool_events.len()
    ));

    // List the topics/tools covered
    if !tool_events.is_empty() {
        let mut tool_names: Vec<&str> = tool_events
            .iter()
            .map(|e| e.name.as_str())
            .filter(|n| !n.is_empty())
            .collect();
        tool_names.sort_unstable();
        tool_names.dedup();
        if !tool_names.is_empty() {
            summary.push_str(&format!("**Tools used**: {}\n\n", tool_names.join(", ")));
        }
    }

    if let Some(first) = user_msgs.first() {
        summary.push_str(&format!(
            "**Started with**: {}\n\n",
            truncate(&first.content, 300)
        ));
    }

    if let Some(last) = assistant_msgs.last() {
        summary.push_str(&format!(
            "**Ended with**: {}\n",
            truncate(&last.content, 300)
        ));
    }

    (title, summary)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}
