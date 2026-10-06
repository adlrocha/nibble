//! CLI command handlers for `nibble session list` / `nibble session read`.

use anyhow::Result;
use std::collections::HashMap;

use crate::db::Database;
use crate::commands::resolve_sandbox_repo_path;
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

    let groups = session::list_sessions_grouped(
        agent.as_deref(),
        repo.as_deref(),
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

    // Build lookup maps from session identifiers → project_path
    let tasks = db.list_tasks().unwrap_or_default();
    let mut claude_task_repo: HashMap<String, String> = HashMap::new();
    let mut pi_task_repo: HashMap<String, String> = HashMap::new();
    // Reverse maps: agent session_id → task_id (for memory badge linkage)
    let mut claude_session_to_task: HashMap<String, String> = HashMap::new();
    let mut pi_path_to_task: HashMap<String, String> = HashMap::new();
    let home_dir = dirs::home_dir();
    for task in &tasks {
        if let Some(ctx) = &task.context {
            if let Some(path) = &ctx.project_path {
                if let Some(sid) = &ctx.claude_session_id {
                    claude_task_repo.insert(sid.clone(), path.clone());
                    claude_session_to_task.insert(sid.clone(), task.task_id.clone());
                }
                if let Some(serde_json::Value::String(p)) = ctx.extra.get("pi_session_path") {
                    // pi_session_path is stored as a container path when the
                    // sandbox attach logic discovers it. Convert it back to a
                    // host path so it matches the paths returned by session
                    // discovery (which scans ~/.pi on the host).
                    let host_path = if let Some(home) = &home_dir {
                        let home_str = home.to_string_lossy();
                        if let Some(rest) = p.strip_prefix("/home/node/") {
                            format!("{}/{}", home_str, rest)
                        } else {
                            p.clone()
                        }
                    } else {
                        p.clone()
                    };
                    pi_task_repo.insert(host_path.clone(), path.clone());
                    pi_path_to_task.insert(host_path, task.task_id.clone());
                }
            }
        }
    }

    // When --sandbox is given, filter groups to exact repo matches.
    let groups = if let Some(repo) = &sandbox_repo {
        let mut filtered = groups;
        for group in &mut filtered {
            group.sessions.retain(|s| {
                let resolved_repo = match s.agent.as_str() {
                    "claude" => claude_task_repo.get(&s.session_id).cloned(),
                    "pi" | "omp" => pi_task_repo
                        .get(&s.path.to_string_lossy().to_string())
                        .cloned(),
                    _ => None,
                };
                resolved_repo.as_deref() == Some(repo)
            });
        }
        filtered.retain(|g| !g.sessions.is_empty());
        filtered
    } else {
        groups
    };

    // Compute max title width for alignment
    let mut max_title_width = 40;
    for group in &groups {
        for s in &group.sessions {
            let title = session::get_session_title(s);
            max_title_width = max_title_width.max(title.len().min(60));
        }
    }
    max_title_width = max_title_width.max(20);

    for group in &groups {
        println!("\n{} ({})", group.label, group.sessions.len());
        println!("{}", "─".repeat(max_title_width + 67));
        for s in &group.sessions {
            let time = session::format_time(s.modified);
            let title = session::get_session_title(s);
            let sid = &s.session_id[..s.session_id.len().min(8)];

            // Resolve workspace: prefer task project_path for sandbox sessions
            let resolved_ws = match s.agent.as_str() {
                "claude" => claude_task_repo.get(&s.session_id).cloned(),
                "pi" | "omp" => pi_task_repo
                    .get(&s.path.to_string_lossy().to_string())
                    .cloned(),
                _ => None,
            };
            let ws =
                session::format_workspace(resolved_ws.as_deref().or(s.workspace.as_deref()));

            let agent_short = memory::format::agent_short_name(&s.agent);
            // Resolve task_id for memory badge lookup
            let task_id_for_mem = match s.agent.as_str() {
                "claude" => claude_session_to_task.get(&s.session_id).cloned(),
                "pi" | "omp" => pi_path_to_task
                    .get(&s.path.to_string_lossy().to_string())
                    .cloned(),
                _ => None,
            };
            let mem_count = task_id_for_mem
                .as_ref()
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
            let linked_task = task_id_for_mem
                .as_deref()
                .map(|t| &t[..t.len().min(8)])
                .unwrap_or("—");
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

pub(crate) fn cmd_session_read(id: &str, raw: bool) -> Result<()> {
    if raw {
        let content = session::read_session_raw(id)?;
        println!("{}", content);
    } else {
        let content = session::read_session(id)?;
        // Use pager for formatted output
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
    Ok(())
}
