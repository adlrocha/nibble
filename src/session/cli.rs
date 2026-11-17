//! CLI command handlers for `nibble session list` / `nibble session read`.

use anyhow::{Context, Result};
use std::collections::HashMap;

use crate::commands::resolve_sandbox_repo_path;
use crate::db::Database;
use crate::{memory, session};

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_session_list(
    db: &Database,
    agent: Option<String>,
    repo: Option<String>,
    sandbox: Option<String>,
    today: bool,
    yesterday: bool,
    week: bool,
    month: bool,
    last: Option<usize>,
    limit: usize,
) -> Result<()> {
    // Resolve --sandbox path using the same semantics as attach/spawn/kill.
    let sandbox_repo = if let Some(input) = &sandbox {
        Some(resolve_sandbox_repo_path(input)?)
    } else {
        None
    };
    // Compute date filter
    let date_range = if today {
        let local_now = chrono::Local::now();
        let start_of_day = local_now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .unwrap();
        let tomorrow = start_of_day + chrono::Duration::days(1);
        Some(session::DateRange {
            since: Some(start_of_day.with_timezone(&chrono::Utc)),
            until: Some(tomorrow.with_timezone(&chrono::Utc)),
        })
    } else if yesterday {
        let local_now = chrono::Local::now();
        let start_of_today = local_now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .unwrap();
        let start_of_yesterday = start_of_today - chrono::Duration::days(1);
        Some(session::DateRange {
            since: Some(start_of_yesterday.with_timezone(&chrono::Utc)),
            until: Some(start_of_today.with_timezone(&chrono::Utc)),
        })
    } else if week {
        Some(session::DateRange {
            since: Some(chrono::Utc::now() - chrono::Duration::days(7)),
            until: None,
        })
    } else if month {
        Some(session::DateRange {
            since: Some(chrono::Utc::now() - chrono::Duration::days(30)),
            until: None,
        })
    } else {
        None
    };

    let effective_limit = last.unwrap_or(limit);

    // Session → repo/task links from the task DB, built before listing:
    // the repo/sandbox filters need them to map sandbox sessions (whose
    // workspace is a container path like /nibble) back to the host repo.
    let links = TaskLinks::from_db(db);

    let repo_pred = repo.as_deref().map(|r| repo_predicate(r, &links));
    let repo_pred_dyn = repo_pred
        .as_ref()
        .map(|f| f as &dyn Fn(&session::SessionInfo) -> bool);

    let groups = session::list_sessions_grouped(
        agent.as_deref(),
        repo_pred_dyn,
        date_range,
        effective_limit,
    )?;

    if groups.is_empty() || groups.iter().all(|g| g.sessions.is_empty()) {
        println!("No sessions found.");
        return Ok(());
    }

    // Build memory count lookup per session_id
    let memories = memory::store::list_memories(None, None, None, None).unwrap_or_default();
    let mut memory_counts: HashMap<String, usize> = HashMap::new();
    for m in &memories {
        if let Some(sid) = &m.session_id {
            *memory_counts.entry(sid.clone()).or_insert(0) += 1;
        }
    }

    // When --sandbox is given, filter groups to exact repo matches.
    let groups = if let Some(repo) = &sandbox_repo {
        let mut filtered = groups;
        for group in &mut filtered {
            group
                .sessions
                .retain(|s| links.repo(s) == Some(repo.as_str()));
        }
        filtered.retain(|g| !g.sessions.is_empty());
        filtered
    } else {
        groups
    };

    // Titles for every listed session in one title-cache pass.
    let all: Vec<session::SessionInfo> = groups
        .iter()
        .flat_map(|g| g.sessions.iter().cloned())
        .collect();
    let titles: HashMap<&str, String> = all
        .iter()
        .map(|s| s.session_id.as_str())
        .zip(session::get_session_titles(&all))
        .collect();
    let title_of = |s: &session::SessionInfo| {
        titles
            .get(s.session_id.as_str())
            .cloned()
            .unwrap_or_default()
    };

    // Compute max title width for alignment
    let max_title_width = titles
        .values()
        .map(|t| t.len().min(60))
        .fold(40, usize::max)
        .max(20);

    for group in &groups {
        println!("\n{} ({})", group.label, group.sessions.len());
        println!("{}", "─".repeat(max_title_width + 67));
        for s in &group.sessions {
            let time = session::format_time(s.effective_time());
            let title = title_of(s);
            let sid = &s.session_id[..s.session_id.len().min(8)];

            // Resolve workspace: prefer task project_path for sandbox sessions
            let ws = session::format_workspace(links.repo(s).or(s.workspace.as_deref()));

            let agent_short = memory::format::agent_short_name(&s.agent);
            // Resolve task_id for memory badge lookup
            let task_id_for_mem = links.task_id(s);
            let mem_count = task_id_for_mem
                .and_then(|tid| memory_counts.get(tid).copied())
                .unwrap_or(0);
            let mem_badge = if mem_count > 0 {
                format!("M:{}", mem_count)
            } else {
                "   ".to_string()
            };
            // Show which sandbox task this session is linked to (the
            // session the next attach would resume), so recovering the
            // right conversation after a reboot doesn't require guessing.
            let linked_task = task_id_for_mem.map(|t| &t[..t.len().min(8)]).unwrap_or("—");
            println!(
                "  {:<6} {:<10} {:<8}  {:<width$}  {:<12} {:<8} {:>5} {}",
                time,
                agent_short,
                sid,
                title,
                ws,
                linked_task,
                mem_badge,
                session::format_size(s.size_bytes),
                width = max_title_width.min(60)
            );
        }
    }
    println!();
    Ok(())
}

/// Links from agent sessions to nibble tasks, built from the task DB.
#[derive(Default)]
pub(crate) struct TaskLinks {
    /// claude session ID → host project path
    claude_task_repo: HashMap<String, String>,
    /// pi/omp transcript host path → host project path
    pi_task_repo: HashMap<String, String>,
    /// claude session ID → task ID
    claude_session_to_task: HashMap<String, String>,
    /// pi/omp transcript host path → task ID
    pi_path_to_task: HashMap<String, String>,
}

impl TaskLinks {
    pub(crate) fn from_db(db: &Database) -> Self {
        let mut links = Self::default();
        let home_dir = dirs::home_dir();
        for task in db.list_tasks().unwrap_or_default() {
            let Some(ctx) = &task.context else { continue };
            let Some(path) = &ctx.project_path else {
                continue;
            };
            if let Some(sid) = &ctx.claude_session_id {
                links.claude_task_repo.insert(sid.clone(), path.clone());
                links
                    .claude_session_to_task
                    .insert(sid.clone(), task.task_id.clone());
            }
            if let Some(serde_json::Value::String(p)) = ctx.extra.get("pi_session_path") {
                // pi_session_path is stored as a container path when the
                // sandbox attach logic discovers it. Convert it back to a
                // host path so it matches the paths returned by session
                // discovery (which scans ~/.pi on the host).
                let host_path = match (&home_dir, p.strip_prefix("/home/node/")) {
                    (Some(home), Some(rest)) => format!("{}/{}", home.to_string_lossy(), rest),
                    _ => p.clone(),
                };
                links.pi_task_repo.insert(host_path.clone(), path.clone());
                links
                    .pi_path_to_task
                    .insert(host_path, task.task_id.clone());
            }
        }
        links
    }

    /// Host repo (project path) of a session via the task DB: claude
    /// sessions by session ID, pi/omp sessions by transcript path. Sandbox
    /// sessions record a container cwd (e.g. `/nibble`), so this is the
    /// only way to map them back to the host repo they belong to.
    pub(crate) fn repo(&self, s: &session::SessionInfo) -> Option<&str> {
        match s.agent.as_str() {
            "claude" => self.claude_task_repo.get(&s.session_id),
            "pi" | "omp" => self.pi_task_repo.get(&*s.path.to_string_lossy()),
            _ => None,
        }
        .map(String::as_str)
    }

    /// Task ID a session is linked to, if any.
    pub(crate) fn task_id(&self, s: &session::SessionInfo) -> Option<&str> {
        match s.agent.as_str() {
            "claude" => self.claude_session_to_task.get(&s.session_id),
            "pi" | "omp" => self.pi_path_to_task.get(&*s.path.to_string_lossy()),
            _ => None,
        }
        .map(String::as_str)
    }
}

/// Build the repo filter predicate for `session list --repo`.
///
/// Inputs that resolve to an existing path (`.`, `~/x`, relative dirs) are
/// canonicalized and matched with path boundaries: the repo matches its own
/// sessions and subdirectory sessions, never sibling repos sharing a prefix
/// (`nibble` vs `nibble-review`). Inputs that don't exist on disk fall back
/// to workspace substring matching. Sandbox sessions (container cwd) are
/// matched through the task DB maps instead.
pub(crate) fn repo_predicate<'a>(
    filter: &str,
    links: &'a TaskLinks,
) -> impl Fn(&session::SessionInfo) -> bool + 'a {
    let (filter, is_path) = match resolve_sandbox_repo_path(filter) {
        Ok(canon) => (canon, true),
        Err(_) => (filter.to_string(), false),
    };
    move |s| {
        let matches_repo = |w: &str| {
            if is_path {
                session::workspace_matches_repo(w, &filter)
            } else {
                !filter.is_empty() && w.contains(filter.as_str())
            }
        };
        s.workspace.as_deref().is_some_and(matches_repo) || links.repo(s).is_some_and(matches_repo)
    }
}

pub(crate) fn cmd_session_info(id: &str) -> Result<()> {
    let matches = session::find_sessions_by_prefix(id);
    if matches.is_empty() {
        anyhow::bail!("No session found matching '{}'", id);
    }
    if matches.len() > 1 {
        anyhow::bail!(
            "'{}' is ambiguous — {} sessions share this prefix. Use a longer prefix:\n{}",
            id,
            matches.len(),
            matches
                .iter()
                .map(|s| format!(
                    "  {}  {}",
                    &s.session_id[..s.session_id.len().min(8)],
                    session::get_session_title(s)
                ))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    print_session_info(&matches[0]);
    Ok(())
}

/// Print a session's full ID, repo, transcript path and resume command.
pub(crate) fn print_session_info(s: &session::SessionInfo) {
    // Ready-to-paste resume command for the session's agent. pi-format
    // transcripts (pi, hermes) resume with pi's --session flag; omp is a
    // pi fork that uses --resume like claude.
    let resume_cmd = match s.agent.as_str() {
        "claude" => format!("claude --resume {}", s.session_id),
        "omp" => format!("omp --resume {}", s.session_id),
        _ => format!("pi --session {}", s.session_id),
    };

    println!("{:12}{}", "Session:", s.session_id);
    println!("{:12}{}", "Agent:", s.agent);
    if let Some(ws) = &s.workspace {
        println!("{:12}{}", "Repo:", ws);
    }
    println!("{:12}{}", "Transcript:", s.path.display());
    if let Some(m) = s.effective_time() {
        println!("{:12}{}", "Updated:", session::format_time(Some(m)));
    }
    println!("{:12}{}", "Size:", session::format_size(s.size_bytes));
    println!();
    println!("Resume from the host (run in the session's repo directory):");
    println!("  {}", resume_cmd);
}

pub(crate) fn cmd_session_read(id: &str, raw: bool) -> Result<()> {
    if raw {
        let content = session::read_session_raw(id)?;
        println!("{}", content);
    } else {
        page(&session::read_session(id)?);
    }
    Ok(())
}

/// Show text in `$PAGER` (default `less`), falling back to `cat`.
fn page(content: &str) {
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".to_string());
    let mut child = std::process::Command::new(&pager)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|_| {
            std::process::Command::new("cat")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("cat should always work")
        });
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(content.as_bytes());
    }
    let _ = child.wait();
}

/// Load every session as a search document, newest first: title, summary
/// (from the session-summary memories) and host repo. Bodies are loaded
/// separately by the background indexer.
fn load_search_docs(db: &Database) -> Result<Vec<session::search::SearchDoc>> {
    let mut sessions = session::list_all_sessions()?;
    sessions.sort_by_key(|s| std::cmp::Reverse(s.effective_time()));
    let links = TaskLinks::from_db(db);
    let summaries = session_summaries();
    let titles = session::get_session_titles(&sessions);
    Ok(sessions
        .into_iter()
        .zip(titles)
        .map(|(s, title)| {
            // Summaries are keyed by the agent session ID, or by the task
            // ID for sessions summarized through their sandbox task.
            let summary = summaries
                .get(&s.session_id)
                .or_else(|| links.task_id(&s).and_then(|t| summaries.get(t)))
                .cloned()
                .unwrap_or_default();
            let repo = links
                .repo(&s)
                .map(str::to_string)
                .or_else(|| s.workspace.clone());
            session::search::SearchDoc::new(s, title, summary, repo)
        })
        .collect())
}

/// Session-summary memory text by session ID. The newest summary wins, and
/// the generated bookkeeping lines (turn counts, tool names) are dropped so
/// a search for "read" or "bash" does not match every summary.
fn session_summaries() -> HashMap<String, String> {
    let memories = memory::store::list_memories(
        None,
        Some(&memory::models::MemoryType::SessionSummary),
        None,
        None,
    )
    .unwrap_or_default();
    let mut out = HashMap::new();
    // list_memories is newest first; keep the first summary per session.
    for m in memories {
        if let Some(sid) = m.session_id {
            out.entry(sid)
                .or_insert_with(|| memory::summarize::summary_body(&m.content));
        }
    }
    out
}

/// `nibble session search`: the interactive search UI on a terminal, or a
/// ranked plain-text list when output is piped (handy for scripts and
/// agents).
pub(crate) fn cmd_session_search(db: &Database, query: &[String], limit: usize) -> Result<()> {
    use std::io::IsTerminal;
    let query = query.join(" ");
    let docs = load_search_docs(db)?;
    if docs.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }
    let sessions: Vec<session::SessionInfo> = docs.iter().map(|d| d.session.clone()).collect();
    let (bodies, indexer) = session::search::spawn_indexer(sessions, session::search::cache_path());

    if !(std::io::stdout().is_terminal() && std::io::stdin().is_terminal()) {
        return print_search_results(docs, &query, &bodies, limit);
    }
    let result = session::search_tui::run(docs, query, &bodies, |doc| {
        page(&session::format_session(&doc.session)?);
        Ok(())
    });
    // Quitting before indexing finished stops the indexer; wait for it to
    // save what it extracted so the next search starts warm.
    drop(bodies);
    let _ = indexer.join();
    let (outcome, docs) = result?;
    if let session::search_tui::Outcome::Selected(i) = outcome {
        print_session_info(&docs[i].session);
    }
    Ok(())
}

/// Wait for every body, then print the best `limit` matches.
fn print_search_results(
    mut docs: Vec<session::search::SearchDoc>,
    query: &str,
    bodies: &std::sync::mpsc::Receiver<session::search::IndexEvent>,
    limit: usize,
) -> Result<()> {
    use session::search::IndexEvent;
    for event in bodies {
        match event {
            IndexEvent::Body { index, body } => docs[index].body = Some(body),
            IndexEvent::Failed { index, error } => eprintln!(
                "warning: could not index {}: {error}",
                docs[index].session.path.display()
            ),
            IndexEvent::Done { cache_error } => {
                if let Some(e) = cache_error {
                    eprintln!("warning: search cache not saved: {e}");
                }
                break;
            }
        }
    }
    // A closed pipe (`| head`) is not an error worth reporting.
    match std::io::Write::write_all(
        &mut std::io::stdout(),
        format_search_results(&docs, query, limit).as_bytes(),
    ) {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => {
            Err(e).context("could not write search results")
        }
        _ => Ok(()),
    }
}

/// Plain-text ranked results: where each match was found (S summary,
/// T title, B body), date, agent, short ID, title and repo.
fn format_search_results(docs: &[session::search::SearchDoc], query: &str, limit: usize) -> String {
    let hits = session::search::Ranker::default().rank(docs, query);
    if hits.is_empty() {
        return "No sessions match.\n".to_string();
    }
    let mut out = String::new();
    for hit in hits.iter().take(limit) {
        let d = &docs[hit.index];
        let flag = |on: bool, c: char| if on { c } else { '·' };
        let date = d.session.effective_time().map_or_else(
            || "unknown".to_string(),
            |t| {
                chrono::DateTime::<chrono::Local>::from(t)
                    .format("%Y-%m-%d")
                    .to_string()
            },
        );
        out.push_str(&format!(
            "{}{}{}  {}  {:<2} {}  {}  {}\n",
            flag(hit.in_summary, 'S'),
            flag(hit.in_title, 'T'),
            flag(hit.body_count > 0, 'B'),
            date,
            memory::format::agent_short_name(&d.session.agent),
            d.session.session_id.chars().take(8).collect::<String>(),
            d.title,
            session::format_workspace(d.repo.as_deref()),
        ));
    }
    if hits.len() > limit {
        out.push_str(&format!("… {} more (raise --limit)\n", hits.len() - limit));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionInfo;

    fn mk_session(agent: &str, sid: &str, workspace: Option<&str>, path: &str) -> SessionInfo {
        SessionInfo {
            agent: agent.to_string(),
            session_id: sid.to_string(),
            workspace: workspace.map(str::to_string),
            path: std::path::PathBuf::from(path),
            modified: None,
            content_time: None,
            size_bytes: 0,
        }
    }

    #[test]
    fn summary_body_drops_bookkeeping_lines() {
        let content =
            "**Turns**: 2 user\n\n**Tools used**: Bash, Read\n\n**Started with**: fix cron";
        assert_eq!(
            memory::summarize::summary_body(content),
            "**Started with**: fix cron"
        );
        assert_eq!(memory::summarize::summary_body(""), "");
    }

    #[test]
    fn task_links_resolve_claude_by_id_and_pi_by_path() {
        let mut links = TaskLinks::default();
        links
            .claude_task_repo
            .insert("c1".to_string(), "/repo/a".to_string());
        links
            .claude_session_to_task
            .insert("c1".to_string(), "task-a".to_string());
        links
            .pi_task_repo
            .insert("/p/t.jsonl".to_string(), "/repo/b".to_string());
        links
            .pi_path_to_task
            .insert("/p/t.jsonl".to_string(), "pi-task".to_string());
        let claude = mk_session("claude", "c1", Some("/workspace"), "/x/c1.jsonl");
        assert_eq!(links.repo(&claude), Some("/repo/a"));
        assert_eq!(links.task_id(&claude), Some("task-a"));
        let pi = mk_session("omp", "p1", None, "/p/t.jsonl");
        assert_eq!(links.repo(&pi), Some("/repo/b"));
        assert_eq!(links.task_id(&pi), Some("pi-task"));
        let hermes = mk_session("hermes", "c1", None, "/p/t.jsonl");
        assert_eq!(links.task_id(&hermes), None);
        assert_eq!(links.repo(&hermes), None);
    }

    fn search_doc(sid: &str, title: &str, summary: &str) -> session::search::SearchDoc {
        let mut s = mk_session("claude", sid, None, "/x.jsonl");
        s.modified =
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(86_400 * 400 + 43_200));
        session::search::SearchDoc::new(
            s,
            title.to_string(),
            summary.to_string(),
            Some("/home/u/nibble".to_string()),
        )
    }

    #[test]
    fn format_search_results_flags_fields_and_limits() {
        let docs = vec![
            search_doc("aaaaaaaa-1", "cron runner", "cron daemon"),
            search_doc("bbbbbbbb-2", "cron title only", ""),
        ];
        let out = format_search_results(&docs, "cron", 1);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        // The date is local time; check the rest of the line around it.
        assert!(lines[0].starts_with("ST·  1971-02-0"), "{}", lines[0]);
        assert!(lines[0].ends_with("  c  aaaaaaaa  cron runner  nibble"));
        assert_eq!(lines[1], "… 1 more (raise --limit)");
        assert_eq!(
            format_search_results(&docs, "zzz", 5),
            "No sessions match.\n"
        );
        let mut undated = docs;
        undated[0].session.modified = None;
        assert!(format_search_results(&undated, "runner", 5).contains("unknown"));
    }

    #[test]
    fn print_search_results_applies_every_index_event() {
        use session::search::{Body, IndexEvent};
        let docs = vec![search_doc("a", "one", ""), search_doc("b", "two", "")];
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(IndexEvent::Body {
            index: 0,
            body: Body::new("body".to_string()),
        })
        .unwrap();
        tx.send(IndexEvent::Failed {
            index: 1,
            error: "unreadable".to_string(),
        })
        .unwrap();
        tx.send(IndexEvent::Done {
            cache_error: Some("read-only".to_string()),
        })
        .unwrap();
        assert!(print_search_results(docs, "body", &rx, 5).is_ok());
    }

    #[test]
    fn repo_predicate_matches_repo_and_subdirs_not_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("nibble");
        std::fs::create_dir_all(&repo).unwrap();
        let sibling = temp.path().join("nibble-review");
        std::fs::create_dir_all(&sibling).unwrap();
        let canon = repo.canonicalize().unwrap().to_string_lossy().to_string();
        let empty = TaskLinks::default();

        let pred = repo_predicate(&canon, &empty);
        assert!(pred(&mk_session("pi", "s1", Some(&canon), "/x/s1.jsonl")));
        assert!(pred(&mk_session(
            "pi",
            "s2",
            Some(&format!("{}/scripts", canon)),
            "/x/s2.jsonl"
        )));
        // Sibling repo sharing the prefix must not match.
        assert!(!pred(&mk_session(
            "pi",
            "s3",
            Some(&sibling.to_string_lossy()),
            "/x/s3.jsonl"
        )));
        // No workspace and no DB link → no match.
        assert!(!pred(&mk_session("pi", "s4", None, "/x/s4.jsonl")));
    }

    #[test]
    fn repo_predicate_matches_sandbox_sessions_via_task_maps() {
        // Sandbox sessions record the container cwd (/nibble) as workspace;
        // the task DB maps them back to the host repo path.
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("nibble");
        std::fs::create_dir_all(&repo).unwrap();
        let canon = repo.canonicalize().unwrap().to_string_lossy().to_string();

        let mut claude_map: HashMap<String, String> = HashMap::new();
        claude_map.insert("sess-1".to_string(), canon.clone());
        let mut pi_map: HashMap<String, String> = HashMap::new();
        pi_map.insert(
            "/home/node/.pi/agent/sessions/--nibble--/t.jsonl".to_string(),
            canon.clone(),
        );
        let claude_links = TaskLinks {
            claude_task_repo: claude_map,
            ..TaskLinks::default()
        };
        let pi_links = TaskLinks {
            pi_task_repo: pi_map,
            ..TaskLinks::default()
        };

        let pred = repo_predicate(&canon, &claude_links);
        assert!(pred(&mk_session(
            "claude",
            "sess-1",
            Some("/nibble"),
            "/x/sess-1.jsonl"
        )));
        assert!(!pred(&mk_session(
            "claude",
            "sess-9",
            Some("/other"),
            "/x/sess-9.jsonl"
        )));

        let pred = repo_predicate(&canon, &pi_links);
        assert!(pred(&mk_session(
            "pi",
            "t",
            Some("/nibble"),
            "/home/node/.pi/agent/sessions/--nibble--/t.jsonl"
        )));
    }

    #[test]
    fn repo_predicate_substring_fallback_for_non_paths() {
        // Inputs that don't exist on disk keep substring semantics.
        let empty = TaskLinks::default();
        let pred = repo_predicate("does-not-exist-anywhere", &empty);
        assert!(pred(&mk_session(
            "pi",
            "s1",
            Some("/a/does-not-exist-anywhere/b"),
            "/x/s1.jsonl"
        )));
        assert!(!pred(&mk_session(
            "pi",
            "s2",
            Some("/a/other"),
            "/x/s2.jsonl"
        )));
        // Empty filter must not match everything (the old `.` bug class).
        let pred = repo_predicate("", &empty);
        assert!(!pred(&mk_session(
            "pi",
            "s3",
            Some("/anywhere"),
            "/x/s3.jsonl"
        )));
    }
}
