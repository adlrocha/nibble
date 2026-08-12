//! `nibble web` — read-only web UI for inspecting pi agent sessions.
//!
//! Serves a dark-mode single-page UI and a small JSON API over HTTP.
//! Designed to run as a systemd user service, reachable over Tailscale.

pub mod sessions;
pub mod stats;

use anyhow::Result;
use serde::Serialize;
use sessions::SessionSummary;
use stats::TaskInfo;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const INDEX_HTML: &str = include_str!("index.html");

/// How long the session index is reused before rescanning the directory.
const INDEX_TTL: Duration = Duration::from_secs(3);
/// Number of days included in the dashboard activity series.
const ACTIVITY_DAYS: usize = 90;

#[derive(Debug, Clone)]
pub struct WebConfig {
    pub host: String,
    pub port: u16,
    pub token: Option<String>,
    pub sessions_root: PathBuf,
    pub db_path: PathBuf,
}

impl Default for WebConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        Self {
            host: "0.0.0.0".to_string(),
            port: 7878,
            token: std::env::var("NIBBLE_WEB_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            sessions_root: PathBuf::from(&home).join(".pi/agent/sessions"),
            db_path: PathBuf::from(&home).join(".nibble/tasks.db"),
        }
    }
}

/// Per-file summary cache entry, invalidated on mtime/size change.
struct FileCacheEntry {
    mtime: Option<std::time::SystemTime>,
    len: u64,
    summary: SessionSummary,
}

#[derive(Default)]
struct IndexCache {
    built_at: Option<Instant>,
    /// id → summary
    summaries: Vec<SessionSummary>,
    by_id: HashMap<String, SessionSummary>,
    /// path → cached parse
    files: HashMap<PathBuf, FileCacheEntry>,
}

impl IndexCache {
    fn rebuild(&mut self, root: &std::path::Path) {
        let mut summaries = Vec::new();
        let mut live_paths = std::collections::HashSet::new();
        for path in sessions::find_session_files(root) {
            live_paths.insert(path.clone());
            let meta = std::fs::metadata(&path).ok();
            let (mtime, len) = meta
                .as_ref()
                .map(|m| (m.modified().ok(), m.len()))
                .unwrap_or((None, 0));
            let cached = self.files.get(&path);
            match cached {
                Some(e) if e.mtime == mtime && e.len == len => {
                    summaries.push(e.summary.clone());
                }
                _ => {
                    if let Some(summary) = sessions::summarize_file(&path) {
                        self.files.insert(
                            path.clone(),
                            FileCacheEntry {
                                mtime,
                                len,
                                summary: summary.clone(),
                            },
                        );
                        summaries.push(summary);
                    } else {
                        self.files.remove(&path);
                    }
                }
            }
        }
        self.files.retain(|p, _| live_paths.contains(p));
        // INV-6: keep first occurrence on id collision.
        let mut seen = std::collections::HashSet::new();
        summaries.retain(|s| seen.insert(s.id.clone()));
        summaries.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
        self.by_id = summaries
            .iter()
            .map(|s| (s.id.clone(), s.clone()))
            .collect();
        self.summaries = summaries;
        self.built_at = Some(Instant::now());
    }
}

struct App {
    cfg: WebConfig,
    index: Mutex<IndexCache>,
}

impl App {
    /// Get the current index, rebuilding when stale.
    fn with_index<R>(&self, f: impl FnOnce(&IndexCache) -> R) -> R {
        let mut guard = self.index.lock().unwrap();
        let stale = guard
            .built_at
            .map(|t| t.elapsed() > INDEX_TTL)
            .unwrap_or(true);
        if stale {
            guard.rebuild(&self.cfg.sessions_root);
        }
        f(&guard)
    }

    fn running_tasks(&self) -> Vec<TaskInfo> {
        // Best-effort: a missing/unreadable DB must never break the API.
        // Existence check keeps the service strictly read-only (INV-2):
        // Database::open runs CREATE TABLE migrations on open.
        if !self.cfg.db_path.exists() {
            return Vec::new();
        }
        let Ok(db) = crate::db::Database::open(&self.cfg.db_path) else {
            return Vec::new();
        };
        db.list_tasks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.status == crate::models::task::TaskStatus::Running)
            .map(|t| TaskInfo {
                task_id: t.task_id,
                agent_type: t.agent_type.as_str().to_string(),
                title: t.title,
                status: t.status.as_str().to_string(),
                updated_at: t.updated_at.to_rfc3339(),
                repo_path: t.repo_path,
            })
            .collect()
    }
}

fn json_response<T: Serialize>(value: &T) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let mut r = Response::from_data(body);
    r.add_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap());
    r
}

fn error_response(code: u16, msg: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut r = json_response(&serde_json::json!({ "error": msg }));
    r = r.with_status_code(StatusCode(code));
    r
}

fn html_response(html: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut r = Response::from_data(html.as_bytes().to_vec());
    r.add_header(
        Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap(),
    );
    r
}

/// Extract a query param from the request URL.
fn query_param(url: &str, key: &str) -> Option<String> {
    let qs = url.split_once('?')?.1;
    for pair in qs.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(n) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(n);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn authorized(req: &Request, token: &Option<String>) -> bool {
    // An empty configured token would match an empty ?token= — treat as unset.
    let Some(expected) = token.as_ref().filter(|t| !t.is_empty()) else {
        return true;
    };
    // ?token=... (easy to bookmark) or Authorization: Bearer ...
    if query_param(req.url(), "token").as_deref() == Some(expected.as_str()) {
        return true;
    }
    req.headers()
        .iter()
        .any(|h| {
            h.field.equiv("Authorization")
                && h.value.as_str() == format!("Bearer {expected}")
        })
}

fn route(app: &App, req: &Request) -> Response<std::io::Cursor<Vec<u8>>> {
    if req.method() != &Method::Get {
        return error_response(405, "method not allowed");
    }
    let path = req.url().split('?').next().unwrap_or("/").to_string();

    if path == "/" || path == "/index.html" {
        return html_response(INDEX_HTML);
    }

    if path == "/api/overview" {
        let tasks = app.running_tasks();
        return app.with_index(|idx| {
            json_response(&stats::build_overview(
                &idx.summaries,
                tasks,
                ACTIVITY_DAYS,
            ))
        });
    }

    if path == "/api/sessions" {
        let q = query_param(req.url(), "q").unwrap_or_default();
        let project = query_param(req.url(), "project").unwrap_or_default();
        return app.with_index(|idx| {
            let out: Vec<&SessionSummary> = idx
                .summaries
                .iter()
                .filter(|s| project.is_empty() || s.project == project)
                .filter(|s| q.is_empty() || sessions::file_matches(&s.file_path, &s.title, &q))
                .collect();
            json_response(&out)
        });
    }

    // /api/session/{id} and /api/session/{id}/raw
    if let Some(rest) = path.strip_prefix("/api/session/") {
        let (id, raw) = match rest.strip_suffix("/raw") {
            Some(id) => (id, true),
            None => (rest, false),
        };
        // INV-3: ids must be plain UUID-ish tokens — reject anything with
        // path separators or extra segments before touching the index.
        if id.is_empty()
            || id
                .chars()
                .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        {
            return error_response(404, "unknown session");
        }
        let summary = app.with_index(|idx| idx.by_id.get(id).cloned());
        let Some(summary) = summary else {
            return error_response(404, "unknown session");
        };
        if raw {
            match std::fs::read(&summary.file_path) {
                Ok(bytes) => {
                    let mut r = Response::from_data(bytes);
                    r.add_header(
                        Header::from_bytes(
                            &b"Content-Type"[..],
                            &b"application/x-ndjson"[..],
                        )
                        .unwrap(),
                    );
                    return r;
                }
                Err(_) => return error_response(404, "session file gone"),
            }
        }
        return match sessions::parse_detail(&summary) {
            Some(detail) => json_response(&detail),
            None => error_response(404, "session file gone"),
        };
    }

    error_response(404, "not found")
}

/// Run the web server (blocks forever).
pub fn serve(cfg: WebConfig) -> Result<()> {
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let server = Server::http(&addr)
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e}"))?;
    eprintln!("nibble web listening on http://{addr}");
    run(server, cfg)
}

fn run(server: Server, cfg: WebConfig) -> Result<()> {
    let app = Arc::new(App {
        index: Mutex::new(IndexCache::default()),
        cfg,
    });

    {
        let c = &app.cfg;
        eprintln!("  sessions: {}", c.sessions_root.display());
        eprintln!(
            "  auth: {}",
            if c.token.is_some() {
                "token required"
            } else {
                "disabled (no token set)"
            }
        );
    }

    for req in server.incoming_requests() {
        let app = Arc::clone(&app);
        std::thread::spawn(move || {
            let resp = if authorized(&req, &app.cfg.token) {
                route(&app, &req)
            } else {
                // INV-4: everything (including /) requires auth when a token
                // is configured.
                error_response(401, "unauthorized")
            };
            let _ = req.respond(resp);
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SID1: &str = "11111111-1111-1111-1111-111111111111";
    const SID2: &str = "22222222-2222-2222-2222-222222222222";

    fn fixture_session(id: &str, cwd: &str, day: &str) -> String {
        format!(
            concat!(
                r#"{{"type":"session","version":3,"id":"{id}","timestamp":"{day}T10:00:00.000Z","cwd":"{cwd}"}}"#, "\n",
                r#"{{"type":"model_change","id":"a","parentId":null,"timestamp":"{day}T10:00:01.000Z","provider":"zai","modelId":"glm-5.1"}}"#, "\n",
                r#"{{"type":"message","id":"m1","parentId":"a","timestamp":"{day}T10:01:00.000Z","message":{{"role":"user","content":[{{"type":"text","text":"fix the flaky test please"}}]}}}}"#, "\n",
                r#"{{"type":"message","id":"m2","parentId":"m1","timestamp":"{day}T10:02:00.000Z","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"hmm"}},{{"type":"text","text":"looking into it"}},{{"type":"toolCall","id":"t1","name":"bash","arguments":{{"command":"cargo test"}}}}],"provider":"zai","model":"glm-5.1","usage":{{"input":100,"output":50,"cacheRead":10,"cacheWrite":5,"cost":{{"total":0.01}}}}}}}}"#, "\n",
                r#"{{"type":"message","id":"m3","parentId":"m2","timestamp":"{day}T10:02:30.000Z","message":{{"role":"toolResult","toolCallId":"t1","toolName":"bash","content":[{{"type":"text","text":"test failed"}}],"isError":true}}}}"#, "\n",
                "this is not json\n",
                r#"{{"type":"message","id":"m4","parentId":"m3","timestamp":"{day}T10:03:00.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"fixed it"}}],"provider":"zai","model":"glm-5.1","usage":{{"input":200,"output":80,"cacheRead":0,"cacheWrite":0,"cost":{{"total":0.02}}}}}}}}"#, "\n",
            ),
            id = id,
            cwd = cwd,
            day = day
        )
    }

    fn make_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("--home-user-proj-a--");
        std::fs::create_dir_all(&proj).unwrap();
        let mut f = std::fs::File::create(proj.join(format!("2026-08-01T10-00-00-000Z_{SID1}.jsonl"))).unwrap();
        write!(f, "{}", fixture_session(SID1, "/home/user/proj-a", "2026-08-01")).unwrap();
        let mut f = std::fs::File::create(proj.join(format!("2026-08-02T10-00-00-000Z_{SID2}.jsonl"))).unwrap();
        write!(f, "{}", fixture_session(SID2, "/home/user/proj-a", "2026-08-02")).unwrap();
        // A corrupt file must be skipped, not fatal (INV-1).
        let mut f = std::fs::File::create(proj.join("garbage.jsonl")).unwrap();
        write!(f, "{{not json").unwrap();
        dir
    }

    // AC-1 + INV-1 + INV-5
    #[test]
    fn summarize_counts_and_tokens() {
        let dir = make_root();
        let summaries = sessions::scan_index(dir.path());
        assert_eq!(summaries.len(), 2, "corrupt file must be skipped");
        // Sorted by last activity, newest first → SID2 first.
        let s = &summaries[0];
        assert_eq!(s.id, SID2);
        assert_eq!(s.project, "proj-a");
        assert_eq!(s.title, "fix the flaky test please");
        assert_eq!(s.message_count, 4);
        assert_eq!(s.user_message_count, 1);
        assert_eq!(s.tool_call_count, 1);
        assert_eq!(s.models, vec!["glm-5.1"]);
        // INV-5: totals equal the sum of per-message usage.
        assert_eq!(s.input_tokens, 300);
        assert_eq!(s.output_tokens, 130);
        assert_eq!(s.cache_read_tokens, 10);
        assert_eq!(s.cache_write_tokens, 5);
        assert!((s.cost_usd - 0.03).abs() < 1e-9);
    }

    // AC-2
    #[test]
    fn detail_events_in_order() {
        let dir = make_root();
        let summaries = sessions::scan_index(dir.path());
        let s = summaries.iter().find(|s| s.id == SID1).unwrap();
        let d = sessions::parse_detail(s).unwrap();
        let kinds: Vec<&str> = d
            .events
            .iter()
            .map(|e| match e {
                sessions::SessionEvent::User { .. } => "user",
                sessions::SessionEvent::Assistant { .. } => "assistant",
                sessions::SessionEvent::ToolResult { .. } => "tool_result",
                sessions::SessionEvent::ModelChange { .. } => "model_change",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["model_change", "user", "assistant", "tool_result", "assistant"]
        );
        match &d.events[2] {
            sessions::SessionEvent::Assistant {
                text,
                thinking,
                tool_calls,
                output_tokens,
                ..
            } => {
                assert_eq!(text, "looking into it");
                assert_eq!(thinking.as_deref(), Some("hmm"));
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "bash");
                assert!(tool_calls[0].arguments.contains("cargo test"));
                assert_eq!(*output_tokens, 50);
            }
            _ => panic!("wrong event"),
        }
        match &d.events[3] {
            sessions::SessionEvent::ToolResult {
                tool_name,
                is_error,
                ..
            } => {
                assert_eq!(tool_name, "bash");
                assert!(*is_error);
            }
            _ => panic!("wrong event"),
        }
    }

    // AC-3
    #[test]
    fn search_matches_title_and_body_case_insensitively() {
        let dir = make_root();
        let summaries = sessions::scan_index(dir.path());
        let s = &summaries[0];
        assert!(sessions::file_matches(&s.file_path, &s.title, "FLAKY")); // title
        assert!(sessions::file_matches(&s.file_path, &s.title, "Fixed It")); // body
        assert!(!sessions::file_matches(&s.file_path, &s.title, "nonexistent-term"));
    }

    #[test]
    fn overview_aggregates() {
        let dir = make_root();
        let summaries = sessions::scan_index(dir.path());
        let o = stats::build_overview(&summaries, vec![], 90);
        assert_eq!(o.total_sessions, 2);
        assert_eq!(o.total_output_tokens, 260);
        assert_eq!(o.per_day.len(), 2);
        assert_eq!(o.per_day[0].day, "2026-08-01");
        assert_eq!(o.per_model.len(), 1);
        assert_eq!(o.per_project.len(), 1);
        assert_eq!(o.per_project[0].project, "proj-a");
    }

    // AC-4 + AC-5: HTTP-level behaviour including auth and bad ids.
    struct TestServer {
        base: String,
        _root: tempfile::TempDir,
    }

    fn start_server(token: Option<&str>) -> TestServer {
        let root = make_root();
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let cfg = WebConfig {
            host: "127.0.0.1".into(),
            port: 0,
            token: token.map(str::to_string),
            sessions_root: root.path().to_path_buf(),
            db_path: root.path().join("nonexistent.db"),
        };
        std::thread::spawn(move || {
            let _ = run(server, cfg);
        });
        TestServer {
            base: format!("http://{addr}"),
            _root: root,
        }
    }

    fn get(url: &str) -> ureq::Response {
        // ureq turns 4xx into Err; normalize to a response.
        match ureq::get(url).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("request failed: {e}"),
        }
    }

    #[test]
    fn http_requires_token_when_configured() {
        let srv = start_server(Some("secret"));
        assert_eq!(get(&format!("{}/api/overview", srv.base)).status(), 401);
        assert_eq!(get(&format!("{}/", srv.base)).status(), 401);
        assert_eq!(
            get(&format!("{}/api/overview?token=wrong", srv.base)).status(),
            401
        );
        assert_eq!(
            get(&format!("{}/api/overview?token=secret", srv.base)).status(),
            200
        );
        // No token configured → open.
        let open = start_server(None);
        assert_eq!(get(&format!("{}/api/overview", open.base)).status(), 200);
    }

    #[test]
    fn empty_token_means_open_not_bypassable() {
        // Regression: an empty configured token must behave as "no auth"
        // (documented open mode), never as a matchable empty credential.
        let srv = start_server(Some(""));
        assert_eq!(get(&format!("{}/api/overview", srv.base)).status(), 200);
    }

    #[test]
    fn http_serves_sessions_detail_and_raw() {
        let srv = start_server(None);
        let r = get(&format!("{}/api/sessions", srv.base));
        assert_eq!(r.status(), 200);
        let body: serde_json::Value = r.into_json().unwrap();
        assert_eq!(body.as_array().unwrap().len(), 2);

        let r = get(&format!("{}/api/sessions?q=flaky", srv.base));
        let body: serde_json::Value = r.into_json().unwrap();
        assert_eq!(body.as_array().unwrap().len(), 2);

        let r = get(&format!("{}/api/session/{SID1}", srv.base));
        assert_eq!(r.status(), 200);
        let body: serde_json::Value = r.into_json().unwrap();
        assert_eq!(body["events"].as_array().unwrap().len(), 5);

        let r = get(&format!("{}/api/session/{SID1}/raw", srv.base));
        assert_eq!(r.status(), 200);
        let text = r.into_string().unwrap();
        assert!(text.contains("\"type\":\"session\""));

        // AC-5: unknown ids and traversal attempts are 404, never touch fs.
        assert_eq!(
            get(&format!("{}/api/session/99999999-9999-9999-9999-999999999999", srv.base)).status(),
            404
        );
        assert_eq!(
            get(&format!("{}/api/session/..%2F..%2Fetc%2Fpasswd", srv.base)).status(),
            404
        );
        assert_eq!(get(&format!("{}/nope", srv.base)).status(), 404);
        // Non-GET methods rejected (INV-2: read-only surface).
        let r = match ureq::post(&format!("{}/api/sessions", srv.base)).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("request failed: {e}"),
        };
        assert_eq!(r.status(), 405);
    }
}
