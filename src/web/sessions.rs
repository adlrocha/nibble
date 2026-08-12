//! Parsing of pi session transcripts (`~/.pi/agent/sessions/<slug>/*.jsonl`)
//! into summaries and renderable detail events for the web UI.
//!
//! All functions are pure over a caller-supplied root directory so tests can
//! point at a tempdir. Corrupt or partial lines are skipped (INV-1).

use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// One-line summary of a session file, used for listing/search/stats.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    #[serde(skip)]
    pub file_path: PathBuf,
    pub cwd: String,
    /// Short project name derived from the cwd basename.
    pub project: String,
    /// First user message text, truncated.
    pub title: String,
    pub started_at: String,
    pub last_active_at: String,
    pub message_count: usize,
    pub user_message_count: usize,
    pub tool_call_count: usize,
    pub models: Vec<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: f64,
    pub size_bytes: u64,
}

/// A renderable event in a session transcript.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    User {
        ts: String,
        text: String,
    },
    Assistant {
        ts: String,
        text: String,
        thinking: Option<String>,
        tool_calls: Vec<ToolCallView>,
        model: Option<String>,
        provider: Option<String>,
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: i64,
        cache_write_tokens: i64,
        cost_usd: f64,
    },
    ToolResult {
        ts: String,
        tool_name: String,
        text: String,
        is_error: bool,
    },
    ModelChange {
        ts: String,
        provider: String,
        model: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallView {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub summary: SessionSummary,
    pub events: Vec<SessionEvent>,
}

const TITLE_MAX: usize = 120;

/// Recursively find all `.jsonl` session files under `root`.
pub fn find_session_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return out;
    }
    for entry in walkdir::WalkDir::new(root)
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "jsonl") {
            out.push(p.to_path_buf());
        }
    }
    out.sort();
    out
}

fn project_of(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cwd.to_string())
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

/// Extract the text content blocks of a message joined into one string.
fn text_of(content: &Value) -> String {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Parse a session file into a summary. Returns None if the file has no
/// recognizable session header or no messages at all.
pub fn summarize_file(path: &Path) -> Option<SessionSummary> {
    let meta = fs::metadata(path).ok()?;
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut id = String::new();
    let mut cwd = String::new();
    let mut started_at = String::new();
    let mut last_active_at = String::new();
    let mut title = String::new();
    let mut message_count = 0usize;
    let mut user_message_count = 0usize;
    let mut tool_call_count = 0usize;
    let mut models: Vec<String> = Vec::new();
    let (mut input, mut output, mut cache_read, mut cache_write) = (0i64, 0i64, 0i64, 0i64);
    let mut cost = 0f64;

    for line in reader.lines() {
        let Ok(line) = line else { continue }; // INV-1: skip unreadable lines
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let ts = v.get("timestamp").and_then(Value::as_str).unwrap_or("");
        match v.get("type").and_then(Value::as_str) {
            Some("session") => {
                id = v
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                cwd = v
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                started_at = ts.to_string();
            }
            Some("model_change") => {
                if let Some(m) = v.get("modelId").and_then(Value::as_str) {
                    if !models.iter().any(|x| x == m) {
                        models.push(m.to_string());
                    }
                }
            }
            Some("message") => {
                message_count += 1;
                if !ts.is_empty() {
                    last_active_at = ts.to_string();
                }
                let Some(msg) = v.get("message") else { continue };
                match msg.get("role").and_then(Value::as_str) {
                    Some("user") => {
                        user_message_count += 1;
                        if title.is_empty() {
                            if let Some(content) = msg.get("content") {
                                let text = text_of(content);
                                let text = text.trim();
                                // Skip system-injected noise (e.g. "[memory] ..." dumps).
                                if !text.is_empty() && !text.starts_with('[') {
                                    title = truncate(text, TITLE_MAX);
                                }
                            }
                        }
                    }
                    Some("assistant") => {
                        if let Some(m) = msg.get("model").and_then(Value::as_str) {
                            if !models.iter().any(|x| x == m) {
                                models.push(m.to_string());
                            }
                        }
                        if let Some(u) = msg.get("usage") {
                            input += u.get("input").and_then(Value::as_i64).unwrap_or(0);
                            output += u.get("output").and_then(Value::as_i64).unwrap_or(0);
                            cache_read +=
                                u.get("cacheRead").and_then(Value::as_i64).unwrap_or(0);
                            cache_write +=
                                u.get("cacheWrite").and_then(Value::as_i64).unwrap_or(0);
                            cost += u
                                .get("cost")
                                .and_then(|c| c.get("total"))
                                .and_then(Value::as_f64)
                                .unwrap_or(0.0);
                        }
                        if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
                            tool_call_count += blocks
                                .iter()
                                .filter(|b| {
                                    b.get("type").and_then(Value::as_str) == Some("toolCall")
                                })
                                .count();
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if id.is_empty() {
        // Fall back to the filename stem: "<timestamp>_<uuid>".
        id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.rsplit('_').next())
            .unwrap_or_default()
            .to_string();
    }
    if id.is_empty() || message_count == 0 {
        return None;
    }
    if started_at.is_empty() {
        started_at = last_active_at.clone();
    }
    if title.is_empty() {
        title = "(no user message)".to_string();
    }

    Some(SessionSummary {
        id,
        file_path: path.to_path_buf(),
        project: project_of(&cwd),
        cwd,
        title,
        started_at,
        last_active_at,
        message_count,
        user_message_count,
        tool_call_count,
        models,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        cost_usd: cost,
        size_bytes: meta.len(),
    })
}

/// Scan a sessions root and summarize every parseable file (INV-1).
/// Used by tests; the server uses its own mtime-cached variant.
#[allow(dead_code)]
pub fn scan_index(root: &Path) -> Vec<SessionSummary> {
    let mut summaries: Vec<SessionSummary> = find_session_files(root)
        .iter()
        .filter_map(|p| summarize_file(p))
        .collect();
    // INV-6: ids are UUIDs; on collision keep the first occurrence.
    let mut seen = std::collections::HashSet::new();
    summaries.retain(|s| seen.insert(s.id.clone()));
    summaries.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
    summaries
}

/// Case-insensitive full-text search: does `needle` appear in the title or
/// any message text of the session at `path`?
pub fn file_matches(path: &Path, title: &str, needle: &str) -> bool {
    let needle = needle.to_lowercase();
    if title.to_lowercase().contains(&needle) {
        return true;
    }
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        // Cheap pre-filter before paying for JSON parsing.
        if !line.to_lowercase().contains(&needle) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(content) = v.pointer("/message/content") else {
            continue;
        };
        if text_of(content).to_lowercase().contains(&needle) {
            return true;
        }
    }
    false
}

/// Parse a session file into its full event stream for the detail view.
pub fn parse_detail(summary: &SessionSummary) -> Option<SessionDetail> {
    let file = fs::File::open(&summary.file_path).ok()?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let ts = v.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
        match v.get("type").and_then(Value::as_str) {
            Some("model_change") => {
                events.push(SessionEvent::ModelChange {
                    ts,
                    provider: v
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    model: v
                        .get("modelId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            Some("message") => {
                let Some(msg) = v.get("message") else { continue };
                let content = msg.get("content").cloned().unwrap_or(Value::Null);
                match msg.get("role").and_then(Value::as_str) {
                    Some("user") => events.push(SessionEvent::User {
                        ts,
                        text: text_of(&content),
                    }),
                    Some("assistant") => {
                        let thinking = content.as_array().and_then(|blocks| {
                            let t: String = blocks
                                .iter()
                                .filter(|b| {
                                    b.get("type").and_then(Value::as_str) == Some("thinking")
                                })
                                .filter_map(|b| b.get("thinking").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n");
                            if t.is_empty() {
                                None
                            } else {
                                Some(t)
                            }
                        });
                        let tool_calls = content
                            .as_array()
                            .map(|blocks| {
                                blocks
                                    .iter()
                                    .filter(|b| {
                                        b.get("type").and_then(Value::as_str)
                                            == Some("toolCall")
                                    })
                                    .map(|b| ToolCallView {
                                        name: b
                                            .get("name")
                                            .and_then(Value::as_str)
                                            .unwrap_or("?")
                                            .to_string(),
                                        arguments: b
                                            .get("arguments")
                                            .map(|a| {
                                                serde_json::to_string_pretty(a)
                                                    .unwrap_or_default()
                                            })
                                            .unwrap_or_default(),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        let u = msg.get("usage").cloned().unwrap_or(Value::Null);
                        events.push(SessionEvent::Assistant {
                            ts,
                            text: text_of(&content),
                            thinking,
                            tool_calls,
                            model: msg
                                .get("model")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            provider: msg
                                .get("provider")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            input_tokens: u.get("input").and_then(Value::as_i64).unwrap_or(0),
                            output_tokens: u.get("output").and_then(Value::as_i64).unwrap_or(0),
                            cache_read_tokens: u
                                .get("cacheRead")
                                .and_then(Value::as_i64)
                                .unwrap_or(0),
                            cache_write_tokens: u
                                .get("cacheWrite")
                                .and_then(Value::as_i64)
                                .unwrap_or(0),
                            cost_usd: u
                                .get("cost")
                                .and_then(|c| c.get("total"))
                                .and_then(Value::as_f64)
                                .unwrap_or(0.0),
                        });
                    }
                    Some("toolResult") => events.push(SessionEvent::ToolResult {
                        ts,
                        tool_name: msg
                            .get("toolName")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_string(),
                        text: text_of(&content),
                        is_error: msg
                            .get("isError")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    }),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    Some(SessionDetail {
        summary: summary.clone(),
        events,
    })
}
