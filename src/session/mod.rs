//! Session discovery, diagnostics, and reading.
//!
//! Lists available sessions for each agent with browser-history-like UX,
//! and can read their contents with agent-specific formatting.

pub(crate) mod cli;
pub(crate) mod pi;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

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
    /// Timestamp of the transcript's own last event, when the file carries
    /// RFC3339 `timestamp` records (claude and pi-family transcripts do).
    /// Restored or synced transcripts can carry a fresh mtime with months-old
    /// content, so this is the reliable recency signal (see
    /// [`SessionInfo::effective_time`]).
    pub content_time: Option<std::time::SystemTime>,
    pub size_bytes: u64,
}

impl SessionInfo {
    /// Best-known recency of the conversation: the transcript's own last
    /// event timestamp when available, else the file mtime.
    pub fn effective_time(&self) -> Option<std::time::SystemTime> {
        self.content_time.or(self.modified)
    }
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

/// Path-boundary repo match: true when workspace `ws` is exactly `repo` or
/// a subdirectory of it. Unlike substring matching this rejects sibling
/// repos sharing a prefix (`/work/nibble` vs `/work/nibble-review`), and
/// never matches short inputs like `.` against every transcript path.
pub fn workspace_matches_repo(ws: &str, repo: &str) -> bool {
    let repo = repo.trim_end_matches('/');
    !repo.is_empty()
        && (ws == repo
            || ws
                .strip_prefix(repo)
                .is_some_and(|rest| rest.starts_with('/')))
}

/// List sessions grouped by date (today, yesterday, etc.).
/// `repo_pred` receives each session and decides whether it belongs to the
/// requested repo (see [`session::cli::repo_predicate`]).
pub fn list_sessions_grouped(
    agent_filter: Option<&str>,
    repo_pred: Option<&dyn Fn(&SessionInfo) -> bool>,
    date_range: Option<DateRange>,
    limit: usize,
) -> Result<Vec<SessionGroup>> {
    list_sessions_grouped_with_home(
        dirs::home_dir().unwrap_or_default().as_ref(),
        agent_filter,
        repo_pred,
        date_range,
        limit,
    )
}

fn list_sessions_grouped_with_home(
    home: &std::path::Path,
    agent_filter: Option<&str>,
    repo_pred: Option<&dyn Fn(&SessionInfo) -> bool>,
    date_range: Option<DateRange>,
    limit: usize,
) -> Result<Vec<SessionGroup>> {
    let mut sessions = list_all_sessions_with_home(home)?;

    // Apply filters
    if let Some(a) = agent_filter {
        sessions.retain(|s| s.agent == a);
    }
    if let Some(pred) = repo_pred {
        sessions.retain(|s| pred(s));
    }
    if let Some(range) = date_range {
        if let Some(since_dt) = range.since {
            sessions.retain(|s| {
                s.effective_time()
                    .map(|m| {
                        let dt: chrono::DateTime<chrono::Utc> = m.into();
                        dt >= since_dt
                    })
                    .unwrap_or(false)
            });
        }
        if let Some(until_dt) = range.until {
            sessions.retain(|s| {
                s.effective_time()
                    .map(|m| {
                        let dt: chrono::DateTime<chrono::Utc> = m.into();
                        dt < until_dt
                    })
                    .unwrap_or(false)
            });
        }
    }

    // Sort by effective recency (transcript content, else mtime) descending
    sessions.sort_by(|a, b| {
        b.effective_time()
            .unwrap_or(std::time::UNIX_EPOCH)
            .cmp(&a.effective_time().unwrap_or(std::time::UNIX_EPOCH))
    });

    sessions.truncate(limit);

    // Group by date
    let mut groups: Vec<SessionGroup> = Vec::new();
    let now = chrono::Local::now().date_naive();
    let yesterday = now.pred_opt().unwrap_or(now);

    for session in sessions {
        let session_date = session.effective_time().map(|m| {
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

/// Longest window of a transcript scanned for event timestamps.
/// Large transcripts (multi-MB sessions are common) make a full read per
/// listing prohibitive; the last records are within a few KB of the end.
const TAIL_SCAN_BYTES: u64 = 64 * 1024;
/// Backward-walk step when locating the final record's start, and how far
/// we are willing to walk (64 chunks ≈ 4 MB) before giving up on it.
const BACKTRACK_CHUNK_BYTES: u64 = 64 * 1024;
const BACKTRACK_MAX_CHUNKS: u64 = 64;
/// How much of a record's start is examined for its header timestamp.
const RECORD_HEADER_BYTES: u64 = 8 * 1024;

/// Extract the newest RFC3339 `timestamp` from a transcript.
///
/// Line-oriented JSONL transcripts are append-ordered, so the last
/// timestamped record is the conversation's most recent event. Scanning is
/// layered, because a final record can be larger than the scan window:
///
/// 1. Exact — parse complete JSONL records in the last
///    [`TAIL_SCAN_BYTES`] and take the last parseable `timestamp`.
/// 2. The newest record spans the window start (header cut off) but its
///    `timestamp` lies inside the window — raw-scan the window text.
/// 3. Walk backwards from EOF to the final record's start and read its
///    header timestamp — handles final records larger than the window.
/// 4. Last resort: the file head window, preferring its newest timestamp.
///
/// Gzipped archives are skipped (their tail is compressed bytes); callers
/// fall back to mtime via [`SessionInfo::effective_time`].
fn last_event_timestamp(path: &std::path::Path) -> Option<std::time::SystemTime> {
    if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        return None;
    }
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let tail = read_range(&mut file, len.saturating_sub(TAIL_SCAN_BYTES), len)?;

    // 1) Exact: newest complete, parseable record in the window.
    let exact = tail
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|v| {
            v.get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        });
    if let Some(dt) = exact {
        return Some(dt.into());
    }
    // 2) Newest record spans the window start; timestamp inside the window.
    if let Some(ts) = scan_timestamps(&tail).pop() {
        return Some(ts);
    }
    // 3) Final record larger than the window: find where it starts and
    //    read the timestamp from its header.
    if let Some(ts) = final_record_timestamp(&mut file, len) {
        return Some(ts);
    }
    // 4) Give up precisely: prefer the newest timestamp near the file head.
    let head = read_range(&mut file, 0, len.min(RECORD_HEADER_BYTES * 8))?;
    scan_timestamps(&head).pop()
}

/// Timestamp of the final record, found by walking backwards from EOF
/// (excluding the record's own terminating newline) to the previous
/// record's terminator — the final record's start — then scanning its
/// header. Returns None when the walk exceeds [`BACKTRACK_MAX_CHUNKS`].
fn final_record_timestamp(file: &mut fs::File, len: u64) -> Option<std::time::SystemTime> {
    for chunk in 1..=BACKTRACK_MAX_CHUNKS {
        // Exclusive upper bound skips the final record's terminator so a
        // chunk-1 hit is a genuine record boundary.
        let end = len
            .saturating_sub(1)
            .saturating_sub(chunk * BACKTRACK_CHUNK_BYTES);
        if end == 0 {
            break;
        }
        let start = end.saturating_sub(BACKTRACK_CHUNK_BYTES);
        let text = read_range(file, start, end)?;
        if let Some(nl) = text.rfind('\n') {
            let record_start = start + nl as u64 + 1;
            let header_end = record_start.saturating_add(RECORD_HEADER_BYTES).min(len);
            let header = read_range(file, record_start, header_end)?;
            return scan_timestamps(&header).into_iter().next();
        }
    }
    None
}

/// Read `[start, end)` as lossy UTF-8. Windows that start mid-record are
/// fine: truncated JSON fails to parse in the exact path and the raw scan
/// wants timestamps from cut-off records too.
fn read_range(file: &mut fs::File, start: u64, end: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    if end <= start {
        return Some(String::new());
    }
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = vec![0u8; (end - start) as usize];
    file.read_exact(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Extract RFC3339 timestamps from raw transcript text in document order,
/// without requiring well-formed JSON lines. Fallback for records that
/// span a scan-window boundary, where line-based JSON parsing can't help.
fn scan_timestamps(text: &str) -> Vec<std::time::SystemTime> {
    const KEY: &str = "\"timestamp\"";
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find(KEY) {
        let after_key = from + rel + KEY.len();
        let rest = &text[after_key..];
        // Position of the value's opening quote (":" precedes it).
        let Some(open) = rest.find('"') else {
            break;
        };
        let value = &rest[open + 1..];
        let Some(close) = value.find('"') else {
            break;
        };
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&value[..close]) {
            out.push(dt.into());
        }
        from = after_key + open + 1 + close + 1;
    }
    out
}

fn list_all_sessions_with_home(home: &std::path::Path) -> Result<Vec<SessionInfo>> {
    let mut sessions = Vec::new();
    sessions.extend(list_pi_family_sessions_with_home(home, ".pi", "pi")?);
    sessions.extend(list_pi_family_sessions_with_home(home, ".omp", "omp")?);
    sessions.extend(list_claude_sessions_with_home(home)?);

    // Transcript timestamps override mtime as the recency signal: restored,
    // synced or touched files carry a fresh mtime with old content.
    for s in &mut sessions {
        s.content_time = last_event_timestamp(&s.path);
    }

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

/// All sessions whose ID starts with `prefix` (exact or short ID).
/// Used to resolve short IDs unambiguously instead of silently picking one.
pub fn find_sessions_by_prefix(prefix: &str) -> Vec<SessionInfo> {
    let mut out = Vec::new();
    if let Ok(sessions) = list_all_sessions() {
        for s in sessions {
            if s.session_id.starts_with(prefix) {
                out.push(s);
            }
        }
    }
    out
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
                content_time: None,
                size_bytes,
            });
        }
    }

    Ok(sessions)
}

fn extract_pi_header(path: &Path) -> (String, Option<String>) {
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
                content_time: None,
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

    // ── Repo filtering ───────────────────────────────────────────────────────────

    #[test]
    fn last_event_timestamp_reads_last_timestamped_line() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_temp_file(
            temp.path(),
            "s.jsonl",
            concat!(
                "{\"type\":\"user\",\"timestamp\":\"2026-02-12T09:51:44.958Z\"}\n",
                "{\"type\":\"assistant\",\"timestamp\":\"2026-02-12T09:55:11.634Z\"}\n",
                "{\"type\":\"summary\"}\n",
            ),
        );
        let ts = last_event_timestamp(&path).unwrap();
        let dt: chrono::DateTime<chrono::Utc> = ts.into();
        assert_eq!(dt.to_rfc3339(), "2026-02-12T09:55:11.634+00:00");
    }

    #[test]
    fn last_event_timestamp_survives_truncated_tail_line() {
        let temp = tempfile::tempdir().unwrap();
        // Last write stopped mid-record (crash/interrupt): the partial JSON
        // must be skipped, not misparsed.
        let path = write_temp_file(
            temp.path(),
            "s.jsonl",
            concat!(
                "{\"type\":\"user\",\"timestamp\":\"2026-03-01T10:00:00Z\"}\n",
                "{\"type\":\"assistant\",\"timestamp\":\"2026-0",
            ),
        );
        let ts = last_event_timestamp(&path).unwrap();
        let dt: chrono::DateTime<chrono::Utc> = ts.into();
        assert_eq!(dt.to_rfc3339(), "2026-03-01T10:00:00+00:00");
    }

    #[test]
    fn last_event_timestamp_only_reads_tail_of_large_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("big.jsonl");
        // One filler line longer than TAIL_SCAN_BYTES, then the real final
        // record: the scanner must drop the partial filler prefix and still
        // find the closing timestamp.
        let filler = format!(
            "{{\"type\":\"filler\",\"data\":\"{}\"}}\n",
            "x".repeat(80 * 1024)
        );
        let mut content = filler;
        content.push_str("{\"type\":\"user\",\"timestamp\":\"2026-01-05T08:00:00Z\"}\n");
        std::fs::write(&path, content).unwrap();
        let ts = last_event_timestamp(&path).unwrap();
        let dt: chrono::DateTime<chrono::Utc> = ts.into();
        assert_eq!(dt.to_rfc3339(), "2026-01-05T08:00:00+00:00");
    }

    #[test]
    fn last_event_timestamp_final_record_larger_than_window() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("huge-final.jsonl");
        // Multi-record file whose FINAL record alone exceeds the tail
        // window: nothing parseable remains in the window, so the record's
        // own header timestamp must be found via the backward walk — not
        // the earlier record's date.
        let mut content =
            String::from("{\"type\":\"user\",\"timestamp\":\"2026-01-01T09:00:00Z\"}\n");
        content.push_str(
            "{\"type\":\"assistant\",\"timestamp\":\"2026-05-01T12:00:00Z\",\"tool_output\":\"",
        );
        content.push_str(&"y".repeat(160 * 1024));
        content.push_str("\"}\n");
        std::fs::write(&path, content).unwrap();
        let ts = last_event_timestamp(&path).unwrap();
        let dt: chrono::DateTime<chrono::Utc> = ts.into();
        assert_eq!(dt.to_rfc3339(), "2026-05-01T12:00:00+00:00");
    }

    #[test]
    fn last_event_timestamp_record_spanning_window_start() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("spanning.jsonl");
        // Final record straddles the window start: its header (with the
        // timestamp) is cut off, but the timestamp itself lies inside the
        // window — the raw scan must find it where line parsing can't.
        let mut content =
            String::from("{\"type\":\"old\",\"timestamp\":\"2026-04-01T00:00:00Z\"}\n");
        content.push_str("{\"type\":\"assistant\",\"data\":\"");
        content.push_str(&"z".repeat(70 * 1024));
        content.push_str("\",\"timestamp\":\"2026-04-02T06:30:00Z\",\"more\":\"");
        content.push_str(&"z".repeat(10 * 1024));
        content.push_str("\"}\n");
        std::fs::write(&path, content).unwrap();
        let ts = last_event_timestamp(&path).unwrap();
        let dt: chrono::DateTime<chrono::Utc> = ts.into();
        assert_eq!(dt.to_rfc3339(), "2026-04-02T06:30:00+00:00");
    }

    #[test]
    fn last_event_timestamp_none_without_timestamps() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_temp_file(temp.path(), "s.jsonl", "{\"type\":\"opaque\"}\n");
        assert_eq!(last_event_timestamp(&path), None);
        assert_eq!(
            last_event_timestamp(temp.path().join("missing.jsonl").as_path()),
            None
        );
    }

    #[test]
    fn last_event_timestamp_skips_gz_archives() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("s.jsonl.gz");
        let mut enc = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        enc.write_all(b"{\"timestamp\":\"2026-01-01T00:00:00Z\"}\n")
            .unwrap();
        drop(enc.finish().unwrap());
        assert_eq!(last_event_timestamp(&path), None);
    }

    #[test]
    fn effective_time_prefers_content_over_mtime() {
        let now = std::time::SystemTime::now();
        let old = std::time::SystemTime::UNIX_EPOCH;
        let mut s = SessionInfo {
            agent: "claude".into(),
            session_id: "s".into(),
            workspace: None,
            path: PathBuf::from("/x"),
            modified: Some(now),
            content_time: Some(old),
            size_bytes: 0,
        };
        assert_eq!(s.effective_time(), Some(old));
        s.content_time = None;
        assert_eq!(s.effective_time(), Some(now));
        s.modified = None;
        assert_eq!(s.effective_time(), None);
    }

    #[test]
    fn grouping_uses_content_timestamps_not_mtime() {
        // Regression: transcripts bulk-restored today carry a fresh mtime but
        // months-old content; they must group by their own last event, not
        // land under "Today".
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/projects/-home-u-work-nibble");
        std::fs::create_dir_all(&dir).unwrap();
        write_temp_file(
            &dir,
            "0aaa1111-2222-3333-4444-555566667777.jsonl",
            concat!(
                "{\"type\":\"user\",\"cwd\":\"/home/u/work/nibble\",\"timestamp\":\"2026-02-12T09:51:44.958Z\"}\n",
                "{\"type\":\"assistant\",\"cwd\":\"/home/u/work/nibble\",\"timestamp\":\"2026-02-12T09:55:11.634Z\"}\n",
            ),
        );
        // No date filter: grouping alone must reflect the transcript date.
        let groups = list_sessions_grouped_with_home(home.path(), None, None, None, 50).unwrap();
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, vec!["February 12, 2026"], "not Today: {labels:?}");

        // --today must exclude the old-content session even with a fresh mtime.
        let now = chrono::Local::now();
        let start_of_day = now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .unwrap();
        let groups = list_sessions_grouped_with_home(
            home.path(),
            None,
            None,
            Some(DateRange {
                since: Some(start_of_day.with_timezone(&chrono::Utc)),
                until: None,
            }),
            50,
        )
        .unwrap();
        assert!(groups.iter().all(|g| g.sessions.is_empty()));
    }

    #[test]
    fn workspace_matches_repo_path_boundaries() {
        let repo = "/home/u/work/nibble";
        assert!(workspace_matches_repo(repo, repo));
        assert!(workspace_matches_repo("/home/u/work/nibble/scripts", repo));
        // Sibling repos sharing a prefix must not match.
        assert!(!workspace_matches_repo("/home/u/work/nibble-review", repo));
        assert!(!workspace_matches_repo("/home/u/work", repo));
        // Trailing slashes on either side don't change the answer.
        assert!(workspace_matches_repo(
            "/home/u/work/nibble/",
            "/home/u/work/nibble/"
        ));
        assert!(workspace_matches_repo(
            "/home/u/work/nibble",
            "/home/u/work/nibble/"
        ));
    }

    #[test]
    fn list_sessions_grouped_repo_predicate_keeps_only_that_repo() {
        // Regression: `session list --repo .` used to substring-match "."
        // against every transcript path, listing sessions from every repo.
        // A repo predicate must keep only sessions whose workspace is the
        // repo itself or a subdirectory of it.
        let home = tempfile::tempdir().unwrap();
        for slug in [
            "--home--u--work--nibble--",
            "--home--u--work--nibble--scripts--",
            "--home--u--work--nibble-review--",
            "--home--u--work--other--",
        ] {
            let dir = home.path().join(format!(".pi/agent/sessions/{}", slug));
            std::fs::create_dir_all(&dir).unwrap();
            let cwd = format!("/{}/", slug.trim_matches('-').replace("--", "/"));
            let cwd = cwd.trim_end_matches('/');
            write_temp_file(
                &dir,
                &format!("2026-10-12T10-00-00-000Z_{}.jsonl", slug.replace("--", "")),
                &format!(
                    r#"{{\"type\":\"session\",\"version\":3,\"id\":\"{}\",\"timestamp\":\"2026-10-12T10:00:00Z\",\"cwd\":\"{}\"}}"#,
                    slug.replace("--", ""),
                    cwd
                ),
            );
        }

        let repo = "/home/u/work/nibble";
        let pred = |s: &SessionInfo| {
            s.workspace
                .as_deref()
                .is_some_and(|w| workspace_matches_repo(w, repo))
        };
        let groups =
            list_sessions_grouped_with_home(home.path(), None, Some(&pred), None, 50).unwrap();
        let kept: Vec<&str> = groups
            .iter()
            .flat_map(|g| g.sessions.iter().filter_map(|s| s.workspace.as_deref()))
            .collect();
        assert_eq!(
            kept.len(),
            2,
            "nibble + nibble/scripts, not the other repos"
        );
        assert!(kept.iter().all(|w| workspace_matches_repo(w, repo)));
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
