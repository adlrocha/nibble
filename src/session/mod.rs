//! Session discovery, diagnostics, and reading.
//!
//! Lists available sessions for each agent with browser-history-like UX,
//! and can read their contents with agent-specific formatting.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::BufRead;
use std::path::PathBuf;

/// True for pi-family session transcripts, including omp's gzipped archives
/// (`.jsonl.gz`, produced by `omp gc --apply` — archived, never deleted).
pub fn is_session_file(path: &std::path::Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
        return true;
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".jsonl.gz"))
}

/// Session ID from a transcript filename (`<timestamp>_<id>.jsonl[.gz]`).
pub fn session_id_from_filename(path: &std::path::Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let stem = name
        .strip_suffix(".jsonl.gz")
        .or_else(|| name.strip_suffix(".jsonl"))
        .unwrap_or(name);
    stem.split('_').nth(1).unwrap_or(stem).to_string()
}

/// Open a session transcript for reading, transparently decompressing
/// `.jsonl.gz` archives.
pub fn open_session_reader(path: &std::path::Path) -> Option<Box<dyn BufRead>> {
    let file = fs::File::open(path).ok()?;
    if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        Some(Box::new(std::io::BufReader::new(
            flate2::read::GzDecoder::new(file),
        )))
    } else {
        Some(Box::new(std::io::BufReader::new(file)))
    }
}

/// Read an entire session transcript to a string (gz-aware).
pub fn read_session_text(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut reader = open_session_reader(path)?;
    let mut s = String::new();
    reader.read_to_string(&mut s).ok()?;
    Some(s)
}

/// Information about a discovered session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub agent: String,
    /// Copy-pasteable session ID (the ID the agent itself uses).
    pub session_id: String,
    /// Resolved workspace path (e.g., /workspace, /home/user/nibble).
    pub workspace: Option<String>,
    pub path: PathBuf,
    pub modified: Option<std::time::SystemTime>,
    pub size_bytes: u64,
}

/// Cached session title (lazy-generated, persisted).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionTitleCache {
    title: String,
    generated_at: chrono::DateTime<chrono::Utc>,
}

fn titles_cache_path() -> PathBuf {
    crate::config::memory_dir().join(".session-titles.json")
}

fn load_title_cache() -> HashMap<String, SessionTitleCache> {
    let path = titles_cache_path();
    if !path.exists() {
        return HashMap::new();
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_title_cache(cache: &HashMap<String, SessionTitleCache>) -> Result<()> {
    let path = titles_cache_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cache)?;
    fs::write(&path, json)?;
    Ok(())
}

/// Get or generate a title for a session.
pub fn get_session_title(session: &SessionInfo) -> String {
    let mut cache = load_title_cache();

    // Return cached title if available and recent (< 30 days old)
    if let Some(entry) = cache.get(&session.session_id) {
        let age = chrono::Utc::now() - entry.generated_at;
        if age.num_days() < 30 {
            return entry.title.clone();
        }
    }

    // Generate title from session content
    let title = generate_title(session);

    // Cache it
    cache.insert(
        session.session_id.clone(),
        SessionTitleCache {
            title: title.clone(),
            generated_at: chrono::Utc::now(),
        },
    );
    let _ = save_title_cache(&cache);

    title
}

fn generate_title(session: &SessionInfo) -> String {
    // Read the first lines only (gz-aware); transcripts can be many MB.
    let first_lines: Vec<String> = open_session_reader(&session.path)
        .map(|r| r.lines().map_while(Result::ok).take(30).collect())
        .unwrap_or_default();
    let first_lines: Vec<&str> = first_lines.iter().map(|s| s.as_str()).collect();

    match session.agent.as_str() {
        "pi" | "omp" => extract_pi_title(&first_lines),
        "claude" => extract_claude_title(&first_lines),
        _ => "Untitled session".to_string(),
    }
}

fn extract_pi_title(lines: &[&str]) -> String {
    // omp/pi write an auto-generated {"type":"title"} record — prefer it.
    for line in lines {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            if val.get("type").and_then(|v| v.as_str()) == Some("title") {
                if let Some(title) = val.get("title").and_then(|v| v.as_str()) {
                    if !title.trim().is_empty() {
                        return truncate_title(title);
                    }
                }
            }
        }
    }
    for line in lines {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            if val.get("type").and_then(|v| v.as_str()) == Some("message") {
                if let Some(msg) = val.get("message") {
                    if msg.get("role").and_then(|v| v.as_str()) == Some("user") {
                        if let Some(content) = msg.get("content") {
                            if let Some(arr) = content.as_array() {
                                for item in arr {
                                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                        return truncate_title(text);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    "Untitled session".to_string()
}

fn extract_claude_title(lines: &[&str]) -> String {
    for line in lines {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            // Skip meta/system messages
            let user_type = val.get("userType").and_then(|v| v.as_str());
            if user_type == Some("system") || user_type == Some("internal") {
                continue;
            }

            let msg = val.get("message");
            let msg_role = msg.and_then(|m| m.get("role")).and_then(|v| v.as_str());

            // For user messages, extract content
            if msg_role == Some("user") {
                if let Some(content) = msg.and_then(|m| m.get("content")) {
                    let text = if let Some(arr) = content.as_array() {
                        arr.iter()
                            .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ")
                    } else {
                        content.as_str().unwrap_or("").to_string()
                    };

                    // Skip command messages and statusline noise
                    if !text.starts_with("<") && !text.starts_with("/") && !text.trim().is_empty() {
                        return truncate_title(&text);
                    }
                }
            }
        }
    }
    "Untitled session".to_string()
}

fn truncate_title(text: &str) -> String {
    let cleaned = text.lines().next().unwrap_or(text).trim();
    if cleaned.len() <= 60 {
        cleaned.to_string()
    } else {
        format!("{}…", &cleaned[..57])
    }
}

// ── Listing with browser-history UX ──────────────────────────────────────────

/// Date grouping for browser-history display.
#[derive(Debug, Clone)]
pub struct SessionGroup {
    pub label: String,
    pub sessions: Vec<SessionInfo>,
}

/// Date range filter for sessions.
#[derive(Debug, Clone, Copy)]
pub struct DateRange {
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub until: Option<chrono::DateTime<chrono::Utc>>,
}

/// List sessions grouped by date (today, yesterday, etc.).
pub fn list_sessions_grouped(
    agent_filter: Option<&str>,
    repo_filter: Option<&str>,
    date_range: Option<DateRange>,
    limit: usize,
) -> Result<Vec<SessionGroup>> {
    let mut sessions = list_all_sessions()?;

    // Apply filters
    if let Some(a) = agent_filter {
        sessions.retain(|s| s.agent == a);
    }
    if let Some(r) = repo_filter {
        sessions.retain(|s| {
            s.workspace.as_ref().is_some_and(|w| w.contains(r))
                || s.path.to_string_lossy().contains(r)
        });
    }
    if let Some(range) = date_range {
        if let Some(since_dt) = range.since {
            sessions.retain(|s| {
                s.modified
                    .map(|m| {
                        let dt: chrono::DateTime<chrono::Utc> = m.into();
                        dt >= since_dt
                    })
                    .unwrap_or(false)
            });
        }
        if let Some(until_dt) = range.until {
            sessions.retain(|s| {
                s.modified
                    .map(|m| {
                        let dt: chrono::DateTime<chrono::Utc> = m.into();
                        dt < until_dt
                    })
                    .unwrap_or(false)
            });
        }
    }

    // Sort by modified time descending
    sessions.sort_by(|a, b| {
        b.modified
            .unwrap_or(std::time::UNIX_EPOCH)
            .cmp(&a.modified.unwrap_or(std::time::UNIX_EPOCH))
    });

    sessions.truncate(limit);

    // Group by date
    let mut groups: Vec<SessionGroup> = Vec::new();
    let now = chrono::Local::now().date_naive();
    let yesterday = now.pred_opt().unwrap_or(now);

    for session in sessions {
        let session_date = session.modified.map(|m| {
            let dt: chrono::DateTime<chrono::Local> = m.into();
            dt.date_naive()
        });

        let label = match session_date {
            Some(d) if d == now => "Today".to_string(),
            Some(d) if d == yesterday => "Yesterday".to_string(),
            Some(d) => d.format("%B %d, %Y").to_string(),
            None => "Unknown date".to_string(),
        };

        // Add to existing group or create new one
        if let Some(group) = groups.last_mut() {
            if group.label == label {
                group.sessions.push(session);
                continue;
            }
        }
        groups.push(SessionGroup {
            label,
            sessions: vec![session],
        });
    }

    Ok(groups)
}

/// List all discoverable sessions across all agents.
pub fn list_all_sessions() -> Result<Vec<SessionInfo>> {
    list_all_sessions_with_home(dirs::home_dir().unwrap_or_default().as_ref())
}

fn list_all_sessions_with_home(home: &std::path::Path) -> Result<Vec<SessionInfo>> {
    let mut sessions = Vec::new();
    sessions.extend(list_pi_family_sessions_with_home(home, ".pi", "pi")?);
    sessions.extend(list_pi_family_sessions_with_home(home, ".omp", "omp")?);
    sessions.extend(list_claude_sessions_with_home(home)?);

    // Sort by modified time descending (most recent first)
    sessions.sort_by(|a, b| {
        b.modified
            .unwrap_or(std::time::UNIX_EPOCH)
            .cmp(&a.modified.unwrap_or(std::time::UNIX_EPOCH))
    });

    Ok(sessions)
}

/// Find a session by its ID across all agents.
pub fn find_session_by_id(id: &str) -> Option<SessionInfo> {
    find_session_by_id_with_home(id, dirs::home_dir().unwrap_or_default().as_ref())
}

fn find_session_by_id_with_home(id: &str, home: &std::path::Path) -> Option<SessionInfo> {
    if let Ok(sessions) = list_all_sessions_with_home(home) {
        if let Some(s) = sessions.iter().find(|s| s.session_id == id) {
            return Some(s.clone());
        }
        let matches: Vec<_> = sessions
            .iter()
            .filter(|s| s.session_id.starts_with(id))
            .collect();
        if matches.len() == 1 {
            return Some(matches[0].clone());
        }
    }
    None
}

/// Read and pretty-print a session file by its ID.
pub fn read_session(id: &str) -> Result<String> {
    read_session_with_home(id, dirs::home_dir().unwrap_or_default().as_ref())
}

fn read_session_with_home(id: &str, home: &std::path::Path) -> Result<String> {
    let session = find_session_by_id_with_home(id, home)
        .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;

    let content = read_session_text(&session.path)
        .with_context(|| format!("Failed to read session file: {}", session.path.display()))?;

    let formatted = match session.agent.as_str() {
        "pi" | "omp" => format_pi_session(&content)?,
        "claude" => format_claude_session(&content)?,
        _ => content,
    };

    Ok(formatted)
}

/// Read raw session content (for --raw flag).
pub fn read_session_raw(id: &str) -> Result<String> {
    read_session_raw_with_home(id, dirs::home_dir().unwrap_or_default().as_ref())
}

fn read_session_raw_with_home(id: &str, home: &std::path::Path) -> Result<String> {
    let session = find_session_by_id_with_home(id, home)
        .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;

    read_session_text(&session.path)
        .with_context(|| format!("Failed to read session file: {}", session.path.display()))
}

// ── Pi sessions ──────────────────────────────────────────────────────────────

/// Decode a Pi session directory slug back to a container path.
///
/// Pi encodes the cwd as a directory slug by replacing `/` with `--` and
/// wrapping the whole thing in `--..--`.  For example:
///   `/nibble`              → `--nibble--`
///   `/nibble--feature-x`   → `--nibble--feature-x--`
///
/// Since each repo is mounted at `/<basename>` inside the container, the
/// decoded path is deterministic and does not need existence checks on the
/// host (the path is a container path, not a host path).
fn decode_pi_slug(slug: &str) -> Option<String> {
    let inner = slug.strip_prefix("--")?.strip_suffix("--")?;
    if inner.is_empty() {
        return None;
    }
    // Simple decode: replace `--` with `/` and prepend `/`.
    Some(format!("/{}", inner.replace("--", "/")))
}

/// Pi session header (first line of JSONL).
#[derive(Debug, Deserialize)]
struct PiSessionHeader {
    #[serde(rename = "type")]
    record_type: String,
    id: String,
    #[serde(default)]
    cwd: String,
}

/// List sessions for a pi-family agent (`config_dir` = ".pi" or ".omp").
/// omp (oh-my-pi) is a fork of pi and uses the identical session layout and
/// JSONL schema under `~/.omp/agent/sessions/`, so one scanner serves both.
fn list_pi_family_sessions_with_home(
    home: &std::path::Path,
    config_dir: &str,
    agent: &str,
) -> Result<Vec<SessionInfo>> {
    let mut sessions = Vec::new();
    let pi_sessions = home.join(config_dir).join("agent").join("sessions");

    if !pi_sessions.exists() {
        return Ok(sessions);
    }

    for entry in fs::read_dir(&pi_sessions)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let hash_dir = entry.path();
        for file in fs::read_dir(&hash_dir)? {
            let file = file?;
            let path = file.path();
            if !is_session_file(&path) {
                continue;
            }

            let meta = file.metadata()?;
            let modified = meta.modified().ok();
            let size_bytes = meta.len();

            let (session_id, workspace) = extract_pi_header(&path);

            sessions.push(SessionInfo {
                agent: agent.to_string(),
                session_id,
                workspace,
                path,
                modified,
                size_bytes,
            });
        }
    }

    Ok(sessions)
}

fn extract_pi_header(path: &PathBuf) -> (String, Option<String>) {
    // omp writes a {"type":"title"} record before the session header, so scan
    // the first few lines rather than only the first (gz-aware).
    if let Some(reader) = open_session_reader(path) {
        for line in reader.lines().map_while(Result::ok).take(10) {
            if let Ok(header) = serde_json::from_str::<PiSessionHeader>(&line) {
                if header.record_type == "session" && !header.cwd.is_empty() {
                    return (header.id, Some(header.cwd));
                }
            }
        }
    }
    // Fallback: derive session ID and workspace from the filename / directory name.
    // Pi filenames are like: 2026-05-08T10-01-45-668Z_<uuid>.jsonl
    // The parent directory encodes the cwd as a slug: --workspace-- → /workspace,
    // --home-adlrocha-workspace-personal-nibble-- → /home/adlrocha/workspace/personal/nibble
    let session_id = session_id_from_filename(path);

    let workspace = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .and_then(decode_pi_slug);

    (session_id, workspace)
}

fn format_pi_session(content: &str) -> Result<String> {
    let mut output = String::new();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(val) => {
                let role = val
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                output.push_str(&format!("\n── Turn {} [{}] ──\n", i + 1, role));

                match role {
                    "session" => {
                        if let Some(cwd) = val.get("cwd").and_then(|v| v.as_str()) {
                            output.push_str(&format!("cwd: {}\n", cwd));
                        }
                    }
                    "message" => {
                        if let Some(msg) = val.get("message") {
                            if let Some(role) = msg.get("role").and_then(|v| v.as_str()) {
                                output.push_str(&format!("role: {}\n", role));
                            }
                            if let Some(content) = msg.get("content") {
                                if let Some(arr) = content.as_array() {
                                    for item in arr {
                                        if let Some(txt) = item.get("text").and_then(|v| v.as_str())
                                        {
                                            output.push_str(txt);
                                            output.push('\n');
                                        }
                                        if let Some(thinking) =
                                            item.get("thinking").and_then(|v| v.as_str())
                                        {
                                            output
                                                .push_str(&format!("\n[thinking]\n{}\n", thinking));
                                        }
                                    }
                                } else if let Some(txt) = content.as_str() {
                                    output.push_str(txt);
                                    output.push('\n');
                                }
                            }
                        }
                    }
                    "toolCall" => {
                        if let Some(name) = val.get("name").and_then(|v| v.as_str()) {
                            output.push_str(&format!("tool: {}\n", name));
                        }
                        if let Some(args) = val.get("arguments") {
                            output.push_str(&format!(
                                "args: {}\n",
                                serde_json::to_string_pretty(args).unwrap_or_default()
                            ));
                        }
                    }
                    "toolResult" => {
                        if let Some(output_val) = val.get("output").and_then(|v| v.as_str()) {
                            output.push_str(&format!("result: {}\n", output_val));
                        }
                    }
                    _ => {
                        output.push_str(&format!(
                            "{}\n",
                            serde_json::to_string_pretty(&val).unwrap_or_default()
                        ));
                    }
                }
            }
            Err(_) => {
                output.push_str(&format!("\n── Turn {} [raw] ──\n{}\n", i + 1, line));
            }
        }
    }
    Ok(output)
}

// ── Claude sessions ──────────────────────────────────────────────────────────

fn list_claude_sessions_with_home(home: &std::path::Path) -> Result<Vec<SessionInfo>> {
    let mut sessions = Vec::new();
    let claude_projects = home.join(".claude").join("projects");

    if !claude_projects.exists() {
        return Ok(sessions);
    }

    for entry in fs::read_dir(&claude_projects)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let project_dir = entry.path();

        for file in fs::read_dir(&project_dir)? {
            let file = file?;
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            let meta = file.metadata()?;
            let modified = meta.modified().ok();
            let size_bytes = meta.len();

            let session_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();

            // Extract workspace from the session file (look for cwd field)
            let workspace = extract_claude_cwd(&path);

            sessions.push(SessionInfo {
                agent: "claude".to_string(),
                session_id,
                workspace,
                path,
                modified,
                size_bytes,
            });
        }
    }

    Ok(sessions)
}

fn extract_claude_cwd(path: &PathBuf) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    // Scan first 50 lines for a cwd field
    for line in content.lines().take(50) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(cwd) = val.get("cwd").and_then(|v| v.as_str()) {
                return Some(cwd.to_string());
            }
        }
    }
    None
}

fn format_claude_session(content: &str) -> Result<String> {
    let mut output = String::new();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(val) => {
                let event_type = val
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                // Skip internal/system events that aren't conversational
                match event_type {
                    "permission-mode" | "file-history-snapshot" => continue,
                    _ => {}
                }

                // Determine role and content
                let (role, text_content, tool_info) = extract_claude_turn(&val);

                if role == "skip" {
                    continue;
                }

                output.push_str(&format!("\n── Turn {} [{}] ──\n", i + 1, role));

                if !text_content.is_empty() {
                    output.push_str(&text_content);
                    output.push('\n');
                }

                if let Some((tool_name, tool_input)) = tool_info {
                    output.push_str(&format!("\n[tool: {}]\n", tool_name));
                    if !tool_input.is_empty() {
                        output.push_str(&format!("input: {}\n", tool_input));
                    }
                }
            }
            Err(_) => {
                output.push_str(&format!("\n── Turn {} [raw] ──\n{}\n", i + 1, line));
            }
        }
    }
    Ok(output)
}

/// Extract (role, text_content, optional_tool_info) from a Claude event.
fn extract_claude_turn(val: &serde_json::Value) -> (String, String, Option<(String, String)>) {
    let event_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match event_type {
        "user" => {
            if let Some(msg) = val.get("message") {
                if let Some(content) = msg.get("content") {
                    let text = if let Some(arr) = content.as_array() {
                        arr.iter()
                            .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
                            .collect::<Vec<_>>()
                            .join("")
                    } else {
                        content.as_str().unwrap_or("").to_string()
                    };
                    // Skip meta/command messages
                    if text.starts_with("<") && text.contains(">") {
                        return ("skip".to_string(), String::new(), None);
                    }
                    return ("user".to_string(), text, None);
                }
            }
            ("skip".to_string(), String::new(), None)
        }
        "assistant" => {
            if let Some(msg) = val.get("message") {
                let mut text_parts = Vec::new();
                let mut tool_name = String::new();
                let mut tool_input = String::new();

                if let Some(content) = msg.get("content") {
                    if let Some(arr) = content.as_array() {
                        for item in arr {
                            match item.get("type").and_then(|v| v.as_str()) {
                                Some("text") => {
                                    if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                                        text_parts.push(t.to_string());
                                    }
                                }
                                Some("tool_use") => {
                                    if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                                        tool_name = name.to_string();
                                    }
                                    if let Some(input) = item.get("input") {
                                        tool_input =
                                            serde_json::to_string_pretty(input).unwrap_or_default();
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

                let text = text_parts.join("\n");
                let tool = if !tool_name.is_empty() {
                    Some((tool_name, tool_input))
                } else {
                    None
                };
                ("assistant".to_string(), text, tool)
            } else {
                ("skip".to_string(), String::new(), None)
            }
        }
        "attachment" => ("skip".to_string(), String::new(), None),
        _ => ("skip".to_string(), String::new(), None),
    }
}

// ── Formatting helpers ───────────────────────────────────────────────────────

pub fn format_time(t: Option<std::time::SystemTime>) -> String {
    match t {
        Some(st) => {
            let dt: chrono::DateTime<chrono::Local> = st.into();
            dt.format("%H:%M").to_string()
        }
        None => "unknown".to_string(),
    }
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    format!("{:.1} {}", size, UNITS[unit_idx])
}

/// Flags derived from a discovered session's agent.
#[derive(Debug, Clone, Copy, Default)]
pub struct DerivedAgentFlags {
    pub hermes: bool,
    pub pi: bool,
    pub omp: bool,
    /// true when the session's agent overrode an explicit flag or was auto-detected.
    pub overridden: bool,
}

/// Given explicit agent flags and a discovered session, derive the correct
/// agent flags to use for attach.
pub fn derive_agent_flags_from_session(
    _hermes: bool,
    _pi: bool,
    _omp: bool,
    session: &SessionInfo,
) -> DerivedAgentFlags {
    let (hermes, pi, omp) = match session.agent.as_str() {
        "hermes" => (true, false, false),
        "pi" => (false, true, false),
        "omp" => (false, false, true),
        _ => (false, false, false),
    };
    DerivedAgentFlags {
        hermes,
        pi,
        omp,
        overridden: true,
    }
}

/// Format workspace path for display: extract basename or show "—".
pub fn format_workspace(ws: Option<&str>) -> String {
    match ws {
        Some(path) => {
            let path = path.trim();
            if path.is_empty() {
                return "—".to_string();
            }
            // Try to extract a meaningful name
            std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_string())
                .unwrap_or_else(|| {
                    if path.len() > 20 {
                        format!("...{}", &path[path.len() - 17..])
                    } else {
                        path.to_string()
                    }
                })
        }
        None => "—".to_string(),
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Last assistant message extraction (for Telegram safety-net)
// ══════════════════════════════════════════════════════════════════════════════

/// Best-effort extraction of the last assistant message for a task.
/// Used by the Telegram listener safety-net to send actual output instead
/// of a generic "Agent turn complete" message.
pub fn last_assistant_message_for_task(task: &crate::models::Task) -> Option<String> {
    let home = dirs::home_dir().unwrap_or_default();

    match task.agent_type {
        crate::models::AgentType::ClaudeCode => {
            let sid = task.context.as_ref()?.claude_session_id.as_deref()?;
            let sesh = find_session_by_id_with_home(sid, &home)?;
            extract_last_assistant_message(&sesh)
        }
        crate::models::AgentType::Pi | crate::models::AgentType::Omp => {
            let container_path = task
                .context
                .as_ref()?
                .extra
                .get("pi_session_path")?
                .as_str()?;
            // Convert container path to host path
            let host_path = if container_path.starts_with("/home/node/") {
                home.join(container_path.strip_prefix("/home/node/")?)
            } else {
                std::path::PathBuf::from(container_path)
            };
            if !host_path.exists() {
                return None;
            }
            let content = read_session_text(&host_path)?;
            extract_last_assistant_from_pi_content(&content)
        }
        _ => None,
    }
}

/// Extract the last assistant message text from a session file.
pub fn extract_last_assistant_message(session: &SessionInfo) -> Option<String> {
    let content = fs::read_to_string(&session.path).ok()?;
    match session.agent.as_str() {
        "claude" => extract_last_assistant_from_claude_content(&content),
        _ => None,
    }
}

fn extract_last_assistant_from_claude_content(content: &str) -> Option<String> {
    // Scan lines in reverse to find the most recent assistant turn.
    for line in content.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(line).ok()?;
        if val.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let msg = val.get("message")?;
        let text_parts = msg
            .get("content")
            .and_then(|c| c.as_array())?
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                    item.get("text").and_then(|v| v.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let text = text_parts.join("\n").trim().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

fn extract_last_assistant_from_pi_content(content: &str) -> Option<String> {
    // Pi JSONL: scan in reverse for {"type":"message","message":{"role":"assistant",...}}
    for line in content.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(line).ok()?;
        if val.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let msg = val.get("message")?;
        if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let text = if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
            arr.iter()
                .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            msg.get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let text = text.trim().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Helper: create a temp directory and write a file.
    fn write_temp_file(dir: &std::path::Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    // ── Pi header extraction ───────────────────────────────────────────────────

    #[test]
    fn extract_pi_header_valid() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_temp_file(
            temp.path(),
            "test.jsonl",
            r#"{"type":"session","version":3,"id":"abc-123","timestamp":"2026-04-27T10:00:00Z","cwd":"/workspace/nibble"}"#,
        );
        let (id, ws) = extract_pi_header(&path);
        assert_eq!(id, "abc-123");
        assert_eq!(ws, Some("/workspace/nibble".to_string()));
    }

    #[test]
    fn extract_pi_header_invalid_json() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_temp_file(temp.path(), "test.jsonl", "not json");
        let (id, ws) = extract_pi_header(&path);
        assert_eq!(id, "test"); // falls back to filename
        assert_eq!(ws, None);
    }

    #[test]
    fn extract_pi_header_missing_file() {
        let path = PathBuf::from("/nonexistent/file.jsonl");
        let (id, ws) = extract_pi_header(&path);
        assert_eq!(id, "file"); // falls back to filename
        assert_eq!(ws, None);
    }

    #[test]
    fn decode_pi_slug_known_paths() {
        // /workspace exists on this machine
        assert_eq!(
            decode_pi_slug("--workspace--"),
            Some("/workspace".to_string())
        );
        // Paths that don't exist fall back to simple decode
        assert_eq!(
            decode_pi_slug("--tmp--nonexistent--path--"),
            Some("/tmp/nonexistent/path".to_string())
        );
        // Empty inner → None
        assert_eq!(decode_pi_slug("----"), None);
        // No surrounding dashes → None
        assert_eq!(decode_pi_slug("workspace"), None);
    }

    #[test]
    fn extract_pi_header_first_line_is_message() {
        // Pi session where first line is a message (no session header).
        // Session ID and workspace must come from filename + directory slug.
        let temp = tempfile::tempdir().unwrap();
        // Create a subdirectory with pi-style slug encoding
        let slug_dir = temp.path().join("--workspace--");
        std::fs::create_dir_all(&slug_dir).unwrap();
        let path = write_temp_file(
            &slug_dir,
            "2026-05-08T10-01-45-668Z_019e0709-39c4-76fb-9029-d0403f3de449.jsonl",
            r#"{"type":"message","id":"3e94dafd","parentId":"deef62d4","timestamp":"2026-05-08T10:07:23.192Z","message":{"role":"assistant","content":[]}}"#,
        );
        let (id, ws) = extract_pi_header(&path);
        assert_eq!(id, "019e0709-39c4-76fb-9029-d0403f3de449");
        assert_eq!(ws, Some("/workspace".to_string()));
    }

    #[test]
    fn extract_pi_header_slug_decoding() {
        let temp = tempfile::tempdir().unwrap();
        let slug_dir = temp
            .path()
            .join("--home--adlrocha--workspace--personal--nibble--");
        std::fs::create_dir_all(&slug_dir).unwrap();
        let path = write_temp_file(
            &slug_dir,
            "2026-05-08T10-01-45-668Z_019abcde-1234-5678-abcd-ef0123456789.jsonl",
            r#"{"type":"message","id":"badfeed1"}"#,
        );
        let (id, ws) = extract_pi_header(&path);
        assert_eq!(id, "019abcde-1234-5678-abcd-ef0123456789");
        assert_eq!(
            ws,
            Some("/home/adlrocha/workspace/personal/nibble".to_string())
        );
    }

    // ── Claude cwd extraction ──────────────────────────────────────────────────

    #[test]
    fn extract_claude_cwd_from_session() {
        let temp = tempfile::tempdir().unwrap();
        let content = r#"{"type":"permission-mode","permissionMode":"default","sessionId":"sess-1"}
{"parentUuid":null,"type":"user","message":{"role":"user","content":"hello"},"cwd":"/home/user/project","sessionId":"sess-1"}
"#;
        let path = write_temp_file(temp.path(), "sess-1.jsonl", content);
        let cwd = extract_claude_cwd(&path);
        assert_eq!(cwd, Some("/home/user/project".to_string()));
    }

    #[test]
    fn extract_claude_cwd_no_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let content = r#"{"type":"permission-mode","permissionMode":"default"}
"#;
        let path = write_temp_file(temp.path(), "sess-2.jsonl", content);
        let cwd = extract_claude_cwd(&path);
        assert_eq!(cwd, None);
    }

    // ── Title extraction ───────────────────────────────────────────────────────

    #[test]
    fn extract_pi_title_from_message() {
        let lines = vec![
            r#"{"type":"session","id":"s1","cwd":"/workspace"}"#,
            r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"Refactor the auth module to use JWT"}]}}"#,
        ];
        assert_eq!(
            extract_pi_title(&lines),
            "Refactor the auth module to use JWT"
        );
    }

    #[test]
    fn extract_pi_title_no_user_message() {
        let lines = vec![
            r#"{"type":"session","id":"s1","cwd":"/workspace"}"#,
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"Hello"}]}}"#,
        ];
        assert_eq!(extract_pi_title(&lines), "Untitled session");
    }

    #[test]
    fn extract_claude_title_from_user_message() {
        let lines = vec![
            r#"{"type":"permission-mode","sessionId":"s1"}"#,
            r#"{"type":"user","message":{"role":"user","content":"Implement user authentication"},"sessionId":"s1","userType":"external"}"#,
        ];
        assert_eq!(
            extract_claude_title(&lines),
            "Implement user authentication"
        );
    }

    #[test]
    fn extract_claude_title_skips_meta_messages() {
        let lines = vec![
            r#"{"type":"permission-mode","sessionId":"s1"}"#,
            r#"{"type":"user","message":{"role":"user","content":"<command-message>statusline</command-message>"},"userType":"external"}"#,
            r#"{"type":"user","message":{"role":"user","content":"Actually do something useful"},"userType":"external"}"#,
        ];
        assert_eq!(extract_claude_title(&lines), "Actually do something useful");
    }
}

#[cfg(test)]
mod gz_tests {
    use super::*;
    use std::io::Write;

    fn write_gz(path: &std::path::Path, content: &str) {
        let f = std::fs::File::create(path).unwrap();
        let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        enc.write_all(content.as_bytes()).unwrap();
        enc.finish().unwrap();
    }

    #[test]
    fn is_session_file_accepts_jsonl_and_gz() {
        assert!(is_session_file(std::path::Path::new("a.jsonl")));
        assert!(is_session_file(std::path::Path::new("a.jsonl.gz")));
        assert!(!is_session_file(std::path::Path::new("a.txt")));
        assert!(!is_session_file(std::path::Path::new("a.gz")));
    }

    #[test]
    fn session_id_from_filename_handles_gz() {
        let p = std::path::Path::new("2026-09-20T10-00-00-000Z_019abcde-1234.jsonl.gz");
        assert_eq!(session_id_from_filename(p), "019abcde-1234");
        let p = std::path::Path::new("2026-09-20T10-00-00-000Z_019abcde-1234.jsonl");
        assert_eq!(session_id_from_filename(p), "019abcde-1234");
    }

    #[test]
    fn extract_pi_header_skips_title_record() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("t.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"title\",\"v\":1,\"title\":\"My session\"}\n{\"type\":\"session\",\"version\":3,\"id\":\"abc-123\",\"cwd\":\"/nibble\"}\n",
        )
        .unwrap();
        let (id, ws) = extract_pi_header(&path);
        assert_eq!(id, "abc-123");
        assert_eq!(ws, Some("/nibble".to_string()));
    }

    #[test]
    fn extract_pi_header_reads_gz() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("t.jsonl.gz");
        write_gz(
            &path,
            "{\"type\":\"title\",\"v\":1,\"title\":\"Archived\"}\n{\"type\":\"session\",\"version\":3,\"id\":\"gz-1\",\"cwd\":\"/nibble\"}\n",
        );
        let (id, ws) = extract_pi_header(&path);
        assert_eq!(id, "gz-1");
        assert_eq!(ws, Some("/nibble".to_string()));
    }

    #[test]
    fn extract_pi_title_prefers_title_record() {
        let lines = vec![
            r#"{"type":"title","v":1,"title":"Auto-generated title"}"#,
            r#"{"type":"session","id":"s1","cwd":"/workspace"}"#,
            r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"some user message"}]}}"#,
        ];
        assert_eq!(extract_pi_title(&lines), "Auto-generated title");
    }

    #[test]
    fn read_session_text_decompresses_gz() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("t.jsonl.gz");
        write_gz(&path, "{\"type\":\"session\"}\n");
        let text = read_session_text(&path).unwrap();
        assert_eq!(text, "{\"type\":\"session\"}\n");
    }
}
