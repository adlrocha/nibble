//! Session discovery, diagnostics, and reading.
//!
//! Lists available sessions for each agent with browser-history-like UX,
//! and can read their contents with agent-specific formatting.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

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
    // Try to extract first meaningful user message
    let content = fs::read_to_string(&session.path).ok().unwrap_or_default();
    let first_lines: Vec<&str> = content.lines().take(20).collect();

    match session.agent.as_str() {
        "pi" => extract_pi_title(&first_lines),
        "claude" => extract_claude_title(&first_lines),
        "opencode" => extract_opencode_title(&first_lines),
        _ => "Untitled session".to_string(),
    }
}

fn extract_pi_title(lines: &[&str]) -> String {
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

fn extract_opencode_title(lines: &[&str]) -> String {
    for line in lines {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(role) = val.get("role").and_then(|v| v.as_str()) {
                if role == "user" {
                    if let Some(content) = val.get("content").and_then(|v| v.as_str()) {
                        return truncate_title(content);
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
            s.workspace.as_ref().map_or(false, |w| w.contains(r))
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
    sessions.extend(list_pi_sessions_with_home(home)?);
    sessions.extend(list_claude_sessions_with_home(home)?);
    sessions.extend(list_opencode_sessions_with_home(home)?);

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

    let content = fs::read_to_string(&session.path)
        .with_context(|| format!("Failed to read session file: {}", session.path.display()))?;

    let formatted = match session.agent.as_str() {
        "pi" => format_pi_session(&content)?,
        "claude" => format_claude_session(&content)?,
        "opencode" => format_opencode_session(&content)?,
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

    fs::read_to_string(&session.path)
        .with_context(|| format!("Failed to read session file: {}", session.path.display()))
}

// ── Pi sessions ──────────────────────────────────────────────────────────────

/// Pi session header (first line of JSONL).
#[derive(Debug, Deserialize)]
struct PiSessionHeader {
    id: String,
    #[serde(default)]
    cwd: String,
}

fn list_pi_sessions_with_home(home: &std::path::Path) -> Result<Vec<SessionInfo>> {
    let mut sessions = Vec::new();
    let pi_sessions = home.join(".pi").join("agent").join("sessions");

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
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            let meta = file.metadata()?;
            let modified = meta.modified().ok();
            let size_bytes = meta.len();

            let (session_id, workspace) = extract_pi_header(&path);

            sessions.push(SessionInfo {
                agent: "pi".to_string(),
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
    if let Ok(content) = fs::read_to_string(path) {
        if let Some(first) = content.lines().next() {
            if let Ok(header) = serde_json::from_str::<PiSessionHeader>(first) {
                return (header.id, Some(header.cwd));
            }
        }
    }
    let fallback = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    (fallback, None)
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

// ── opencode sessions ────────────────────────────────────────────────────────

fn list_opencode_sessions_with_home(home: &std::path::Path) -> Result<Vec<SessionInfo>> {
    let mut sessions = Vec::new();
    let opencode_data = home.join(".local").join("share").join("opencode");

    if !opencode_data.exists() {
        return Ok(sessions);
    }

    for entry in fs::read_dir(&opencode_data)? {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !name.starts_with("ses_") || !name.ends_with(".json") {
            continue;
        }

        let meta = entry.metadata()?;
        let modified = meta.modified().ok();
        let size_bytes = meta.len();

        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        sessions.push(SessionInfo {
            agent: "opencode".to_string(),
            session_id,
            workspace: None,
            path,
            modified,
            size_bytes,
        });
    }

    Ok(sessions)
}

fn format_opencode_session(content: &str) -> Result<String> {
    let val: serde_json::Value =
        serde_json::from_str(content).with_context(|| "Failed to parse opencode session JSON")?;
    serde_json::to_string_pretty(&val).with_context(|| "Failed to format opencode session")
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

/// Given explicit agent flags and a discovered session, derive the correct
/// agent flags to use for attach.
///
/// Returns `(opencode, hermes, pi, agent_override)` where `agent_override` is
/// true when the session's agent overrode an explicit flag or was auto-detected.
pub fn derive_agent_flags_from_session(
    _opencode: bool,
    _hermes: bool,
    _pi: bool,
    session: &SessionInfo,
) -> (bool, bool, bool, bool) {
    let (derived_oc, derived_h, derived_pi) = match session.agent.as_str() {
        "opencode" => (true, false, false),
        "hermes" => (false, true, false),
        "pi" => (false, false, true),
        "claude" | _ => (false, false, false),
    };
    (derived_oc, derived_h, derived_pi, true)
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

    #[test]
    fn test_extract_opencode_title() {
        let lines = vec![
            r#"{"role":"system","content":"You are a helpful assistant"}"#,
            r#"{"role":"user","content":"Deploy to production"}"#,
        ];
        assert_eq!(extract_opencode_title(&lines), "Deploy to production");
    }

    #[test]
    fn truncate_title_short() {
        assert_eq!(truncate_title("Short"), "Short");
    }

    #[test]
    fn truncate_title_long() {
        let long = "a".repeat(100);
        let result = truncate_title(&long);
        assert!(result.ends_with('…'));
        assert_eq!(result.len(), 60); // 57 chars + "…"
    }

    // ── Claude turn extraction ─────────────────────────────────────────────────

    #[test]
    fn extract_claude_turn_user() {
        let val = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": "Hello world" }
        });
        let (role, text, tool) = extract_claude_turn(&val);
        assert_eq!(role, "user");
        assert_eq!(text, "Hello world");
        assert!(tool.is_none());
    }

    #[test]
    fn extract_claude_turn_user_array_content() {
        let val = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": "Part 1" }, { "type": "text", "text": "Part 2" }] }
        });
        let (role, text, tool) = extract_claude_turn(&val);
        assert_eq!(role, "user");
        assert_eq!(text, "Part 1Part 2");
    }

    #[test]
    fn extract_claude_turn_user_skips_commands() {
        let val = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": "<command-name>/statusline</command-name>" }
        });
        let (role, _text, _tool) = extract_claude_turn(&val);
        assert_eq!(role, "skip");
    }

    #[test]
    fn extract_claude_turn_assistant_text() {
        let val = serde_json::json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "I'll help you" }] }
        });
        let (role, text, tool) = extract_claude_turn(&val);
        assert_eq!(role, "assistant");
        assert_eq!(text, "I'll help you");
        assert!(tool.is_none());
    }

    #[test]
    fn extract_claude_turn_assistant_with_tool() {
        let val = serde_json::json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [
                { "type": "text", "text": "Let me check" },
                { "type": "tool_use", "name": "Bash", "input": { "command": "ls" } }
            ]}
        });
        let (role, text, tool) = extract_claude_turn(&val);
        assert_eq!(role, "assistant");
        assert_eq!(text, "Let me check");
        assert!(tool.is_some());
        let (name, input) = tool.unwrap();
        assert_eq!(name, "Bash");
        assert!(input.contains("ls"));
    }

    #[test]
    fn extract_claude_turn_skips_attachment() {
        let val = serde_json::json!({
            "type": "attachment",
            "attachment": { "type": "skill_listing" }
        });
        let (role, _text, _tool) = extract_claude_turn(&val);
        assert_eq!(role, "skip");
    }

    #[test]
    fn extract_claude_turn_skips_progress() {
        let val = serde_json::json!({
            "type": "progress",
            "data": { "type": "hook_progress" }
        });
        let (role, _text, _tool) = extract_claude_turn(&val);
        assert_eq!(role, "skip");
    }

    // ── Session formatting ─────────────────────────────────────────────────────

    #[test]
    fn format_pi_session_basic() {
        let content = r#"{"type":"session","id":"s1","cwd":"/workspace"}
{"type":"message","message":{"role":"user","content":[{"type":"text","text":"Hello"}]}}
{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"Hi there"}]}}"#;
        let result = format_pi_session(content).unwrap();
        assert!(result.contains("── Turn 1 [session] ──"));
        assert!(result.contains("cwd: /workspace"));
        assert!(result.contains("── Turn 2 [message] ──"));
        assert!(result.contains("role: user"));
        assert!(result.contains("Hello"));
        assert!(result.contains("── Turn 3 [message] ──"));
        assert!(result.contains("role: assistant"));
        assert!(result.contains("Hi there"));
    }

    #[test]
    fn format_pi_session_tool_call() {
        let content = r#"{"type":"toolCall","name":"Bash","arguments":{"command":"ls"}}"#;
        let result = format_pi_session(content).unwrap();
        assert!(result.contains("tool: Bash"));
        assert!(result.contains("args:"));
    }

    #[test]
    fn format_claude_session_skips_internal() {
        let content = r#"{"type":"permission-mode","permissionMode":"default","sessionId":"s1"}
{"type":"file-history-snapshot","messageId":"m1","snapshot":{}}
{"type":"user","message":{"role":"user","content":"Real message"},"userType":"external"}
"#;
        let result = format_claude_session(content).unwrap();
        // Should NOT contain internal events
        assert!(!result.contains("permission-mode"));
        assert!(!result.contains("file-history-snapshot"));
        // SHOULD contain user message
        assert!(result.contains("Real message"));
    }

    #[test]
    fn format_claude_session_with_tool() {
        let content = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Running command"},{"type":"tool_use","name":"Bash","input":{"command":"pwd"}}]}}"#;
        let result = format_claude_session(content).unwrap();
        assert!(result.contains("Running command"));
        assert!(result.contains("[tool: Bash]"));
        assert!(result.contains("input:"));
    }

    #[test]
    fn format_opencode_session_valid() {
        let content = r#"{"messages":[{"role":"user","content":"Hello"},{"role":"assistant","content":"Hi"}]}"#;
        let result = format_opencode_session(content).unwrap();
        assert!(result.contains("Hello"));
        assert!(result.contains("Hi"));
    }

    // ── Workspace formatting ───────────────────────────────────────────────────

    #[test]
    fn format_workspace_basename() {
        assert_eq!(format_workspace(Some("/home/user/nibble")), "nibble");
    }

    #[test]
    fn format_workspace_root() {
        assert_eq!(format_workspace(Some("/workspace")), "workspace");
    }

    #[test]
    fn format_workspace_empty() {
        assert_eq!(format_workspace(Some("")), "—");
    }

    #[test]
    fn format_workspace_none() {
        assert_eq!(format_workspace(None), "—");
    }

    // ── Size formatting ────────────────────────────────────────────────────────

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(500), "500.0 B");
    }

    #[test]
    fn format_size_kb() {
        assert_eq!(format_size(1500), "1.5 KB");
    }

    #[test]
    fn format_size_mb() {
        assert_eq!(format_size(2_000_000), "1.9 MB");
    }

    // ── Time formatting ────────────────────────────────────────────────────────

    #[test]
    fn format_time_some() {
        let st = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(3600);
        let result = format_time(Some(st));
        // Just check it doesn't panic and returns a string
        assert!(!result.is_empty());
    }

    #[test]
    fn format_time_none() {
        assert_eq!(format_time(None), "unknown");
    }

    // ── Date grouping ──────────────────────────────────────────────────────────

    #[test]
    fn list_sessions_grouped_empty() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = list_all_sessions_with_home(temp.path()).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn list_sessions_grouped_with_pi_session() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();

        // Create a Pi session
        let pi_dir = home
            .join(".pi")
            .join("agent")
            .join("sessions")
            .join("--workspace--");
        fs::create_dir_all(&pi_dir).unwrap();
        let session_content = r#"{"type":"session","version":3,"id":"test-pi-id","timestamp":"2026-04-27T10:00:00Z","cwd":"/workspace"}
{"type":"message","message":{"role":"user","content":[{"type":"text","text":"Hello from test"}]}}"#;
        write_temp_file(
            &pi_dir,
            "2026-04-27T10-00-00_test-pi-id.jsonl",
            session_content,
        );

        let sessions = list_all_sessions_with_home(home).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].agent, "pi");
        assert_eq!(sessions[0].session_id, "test-pi-id");
        assert_eq!(sessions[0].workspace, Some("/workspace".to_string()));
    }

    #[test]
    fn list_sessions_grouped_with_claude_session() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();

        // Create a Claude session
        let claude_dir = home.join(".claude").join("projects").join("-workspace--");
        fs::create_dir_all(&claude_dir).unwrap();
        let session_content = r#"{"type":"permission-mode","sessionId":"test-claude-id"}
{"type":"user","message":{"role":"user","content":"Test message"},"cwd":"/workspace","sessionId":"test-claude-id"}
"#;
        write_temp_file(&claude_dir, "test-claude-id.jsonl", session_content);

        let sessions = list_all_sessions_with_home(home).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].agent, "claude");
        assert_eq!(sessions[0].session_id, "test-claude-id");
        assert_eq!(sessions[0].workspace, Some("/workspace".to_string()));
    }

    #[test]
    fn list_sessions_grouped_date_range() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();

        // Create a Pi session
        let pi_dir = home
            .join(".pi")
            .join("agent")
            .join("sessions")
            .join("--workspace--");
        fs::create_dir_all(&pi_dir).unwrap();
        let session_content = r#"{"type":"session","id":"test-id","cwd":"/workspace"}"#;
        write_temp_file(
            &pi_dir,
            "2026-04-27T10-00-00_test-id.jsonl",
            session_content,
        );

        // Filter for a future date — should return empty
        let future = chrono::Utc::now() + chrono::Duration::days(1);
        let groups = list_sessions_grouped(
            None,
            None,
            Some(DateRange {
                since: Some(future),
                until: None,
            }),
            10,
        )
        .unwrap();
        assert!(groups.is_empty() || groups.iter().all(|g| g.sessions.is_empty()));
    }

    // ── Session ID finding ─────────────────────────────────────────────────────

    #[test]
    fn find_session_by_id_exact_match() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();

        let pi_dir = home
            .join(".pi")
            .join("agent")
            .join("sessions")
            .join("--workspace--");
        fs::create_dir_all(&pi_dir).unwrap();
        let session_content = r#"{"type":"session","id":"exact-match-id","cwd":"/workspace"}"#;
        write_temp_file(&pi_dir, "2026-04-27_exact-match-id.jsonl", session_content);

        // Use internal function directly
        let sessions = list_all_sessions_with_home(home).unwrap();
        let found = sessions.iter().find(|s| s.session_id == "exact-match-id");
        assert!(found.is_some());
        assert_eq!(found.unwrap().session_id, "exact-match-id");
    }

    #[test]
    fn find_session_by_id_prefix_match() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();

        let pi_dir = home
            .join(".pi")
            .join("agent")
            .join("sessions")
            .join("--workspace--");
        fs::create_dir_all(&pi_dir).unwrap();
        let session_content = r#"{"type":"session","id":"prefix-test-id","cwd":"/workspace"}"#;
        write_temp_file(&pi_dir, "2026-04-27_prefix-test-id.jsonl", session_content);

        let sessions = list_all_sessions_with_home(home).unwrap();
        let matches: Vec<_> = sessions
            .iter()
            .filter(|s| s.session_id.starts_with("prefix-test"))
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].session_id, "prefix-test-id");
    }

    // ── Session reading ────────────────────────────────────────────────────────

    #[test]
    fn read_session_raw_returns_content() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();

        let pi_dir = home
            .join(".pi")
            .join("agent")
            .join("sessions")
            .join("--workspace--");
        fs::create_dir_all(&pi_dir).unwrap();
        let session_content = r#"{"type":"session","id":"read-test","cwd":"/workspace"}
{"type":"message","message":{"role":"user","content":[{"type":"text","text":"Hello"}]}}"#;
        write_temp_file(&pi_dir, "2026-04-27_read-test.jsonl", session_content);

        let raw = read_session_raw_with_home("read-test", home).unwrap();
        assert!(raw.contains("Hello"));
    }

    #[test]
    fn read_session_formatted() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();

        let pi_dir = home
            .join(".pi")
            .join("agent")
            .join("sessions")
            .join("--workspace--");
        fs::create_dir_all(&pi_dir).unwrap();
        let session_content = r#"{"type":"session","id":"fmt-test","cwd":"/workspace"}
{"type":"message","message":{"role":"user","content":[{"type":"text","text":"Test msg"}]}}"#;
        write_temp_file(&pi_dir, "2026-04-27_fmt-test.jsonl", session_content);

        let formatted = read_session_with_home("fmt-test", home).unwrap();
        assert!(formatted.contains("── Turn"));
        assert!(formatted.contains("Test msg"));
    }

    #[test]
    fn read_session_not_found() {
        let temp = tempfile::tempdir().unwrap();
        let result = read_session_with_home("nonexistent", temp.path());
        assert!(result.is_err());
    }

    // ── derive_agent_flags_from_session ────────────────────────────────────────

    fn make_session(agent: &str) -> SessionInfo {
        SessionInfo {
            agent: agent.to_string(),
            session_id: "test-id".to_string(),
            workspace: None,
            path: PathBuf::from("/tmp/test.jsonl"),
            modified: None,
            size_bytes: 0,
        }
    }

    #[test]
    fn auto_detect_pi_session() {
        let sesh = make_session("pi");
        let (oc, h, pi, overridden) = derive_agent_flags_from_session(false, false, false, &sesh);
        assert!(!oc);
        assert!(!h);
        assert!(pi);
        assert!(overridden);
    }

    #[test]
    fn auto_detect_opencode_session() {
        let sesh = make_session("opencode");
        let (oc, h, pi, overridden) = derive_agent_flags_from_session(false, false, false, &sesh);
        assert!(oc);
        assert!(!h);
        assert!(!pi);
        assert!(overridden);
    }

    #[test]
    fn auto_detect_claude_session() {
        let sesh = make_session("claude");
        let (oc, h, pi, overridden) = derive_agent_flags_from_session(false, false, false, &sesh);
        assert!(!oc);
        assert!(!h);
        assert!(!pi);
        assert!(overridden);
    }

    #[test]
    fn auto_detect_hermes_session() {
        let sesh = make_session("hermes");
        let (oc, h, pi, overridden) = derive_agent_flags_from_session(false, false, false, &sesh);
        assert!(!oc);
        assert!(h);
        assert!(!pi);
        assert!(overridden);
    }

    #[test]
    fn session_overrides_explicit_flag() {
        let sesh = make_session("pi");
        // User passed --opencode, but session is pi → pi wins
        let (oc, h, pi, overridden) = derive_agent_flags_from_session(true, false, false, &sesh);
        assert!(!oc);
        assert!(!h);
        assert!(pi);
        assert!(overridden);
    }

    #[test]
    fn session_overrides_explicit_flag_claude() {
        let sesh = make_session("claude");
        // User passed --pi, but session is claude → claude wins
        let (oc, h, pi, overridden) = derive_agent_flags_from_session(false, false, true, &sesh);
        assert!(!oc);
        assert!(!h);
        assert!(!pi);
        assert!(overridden);
    }
}
