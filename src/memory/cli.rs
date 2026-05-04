//! CLI command handlers for the memory system.

use crate::config;
use crate::memory::models::*;
use crate::memory::{git, index, search, store};
use anyhow::Result;
use chrono::{Datelike, TimeZone, Utc};
use std::io::{BufRead, Write};

pub fn handle_write(
    content: &str,
    memory_type: &str,
    project: Option<&str>,
    tags: Option<&str>,
    update: Option<&str>,
    title: Option<&str>,
) -> Result<()> {
    let mt = MemoryType::from_str_lossy(memory_type);
    let tag_vec: Vec<String> = tags
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    // Try to infer project from cwd if not specified
    let project = project.map(|s| s.to_string()).or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string()))
    });

    let agent = std::env::var("NIBBLE_AGENT_TYPE").unwrap_or_else(|_| "manual".to_string());

    let (path, id) = store::write_memory(
        &mt,
        content,
        &agent,
        project.as_deref(),
        &tag_vec,
        None, // session_id not available for manual writes
        None, // task_id not available for manual writes
        None, // confidence defaults to 1.0 for manual writes
        update,
        title,
    )?;

    // Update caches
    let _ = index::reindex();
    let _ = index::regenerate_index_md();

    // Git commit
    let cfg = config::load().unwrap_or_default();
    let base = config::memory_dir();
    let _ = git::commit(
        &base,
        &format!(
            "memory: write {} '{}'",
            mt,
            &content[..50.min(content.len())]
        ),
        &cfg.memory.sync.author_name,
        &cfg.memory.sync.author_email,
    );

    if update.is_some() {
        println!("Updated memory: {id}");
    } else {
        println!("Created memory: {id}");
        println!("  Type: {mt}");
        println!("  File: {}", path.display());
    }

    Ok(())
}

pub fn handle_search(
    query: &str,
    project: Option<&str>,
    memory_type: Option<&str>,
    limit: Option<usize>,
    semantic: bool,
) -> Result<()> {
    let mt = memory_type.map(|s| MemoryType::from_str_lossy(s));

    let results = if semantic {
        // Semantic search is Phase 3; for now, fall back to keyword
        eprintln!("[memory] Semantic search not yet available, using keyword search");
        search::search_memories(query, project, mt.as_ref(), limit)?
    } else {
        search::search_memories(query, project, mt.as_ref(), limit)?
    };

    if results.is_empty() {
        println!("No memories found matching '{}'.", query);
        return Ok(());
    }

    println!("Found {} memory(ies):\n", results.len());
    for entry in &results {
        let date = entry.created_at.format("%Y-%m-%d").to_string();
        let display = entry.title.clone().unwrap_or_else(|| {
            let preview: String = entry.content.chars().take(120).collect();
            preview.trim_end_matches('…').to_string()
        });
        println!(
            "  [{}] {} ({}{}) confidence={:.2}",
            entry.memory_type,
            display,
            entry
                .project
                .as_deref()
                .map(|p| format!("{p} · "))
                .unwrap_or_default(),
            date,
            entry.confidence,
        );
        println!("    id: {}", entry.memory_id);
        if !entry.tags.is_empty() {
            println!("    tags: {}", entry.tags.join(", "));
        }
        println!();
    }

    Ok(())
}

pub fn handle_list(
    project: Option<&str>,
    memory_type: Option<&str>,
    since: Option<&str>,
    limit: Option<usize>,
) -> Result<()> {
    let mt = memory_type.map(|s| MemoryType::from_str_lossy(s));
    let since_dt = since.and_then(|s| {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .ok()
            .and_then(|d| {
                Utc.with_ymd_and_hms(d.year(), d.month(), d.day(), 0, 0, 0)
                    .single()
            })
    });

    let entries = store::list_memories(project, mt.as_ref(), since_dt.as_ref(), limit)?;

    if entries.is_empty() {
        println!("No memories found.");
        return Ok(());
    }

    // Group by date using shared utility
    let groups = crate::memory::format::group_by_date(&entries, |e| e.created_at);

    // Compute max title width for alignment
    let titles: Vec<String> = entries
        .iter()
        .map(|m| {
            m.title
                .clone()
                .unwrap_or_else(|| crate::memory::format::truncate_title(&m.content, 80))
        })
        .collect();
    let max_title_width = crate::memory::format::compute_max_title_width(&titles, 20, 60);

    println!("{} memory(ies)", entries.len());

    for group in &groups {
        println!("\n{} ({})", group.label, group.items.len());
        println!("{}", "─".repeat(max_title_width + 55));
        for m in &group.items {
            let time = m.created_at.format("%H:%M").to_string();
            let title = m
                .title
                .clone()
                .unwrap_or_else(|| crate::memory::format::truncate_title(&m.content, 60));
            let mid = &m.memory_id[..m.memory_id.len().min(8)];
            let type_short = type_abbrev(&m.memory_type);
            let agent_short = crate::memory::format::agent_short_name(&m.agent);
            let proj: String = m
                .project
                .as_deref()
                .map(|p| {
                    if p.len() > 12 {
                        format!("{}…", &p[..11])
                    } else {
                        p.to_string()
                    }
                })
                .unwrap_or_else(|| "—".to_string());

            println!(
                "  {:<6} {:<4} {:<8}  {:<width$}  {:<13} {}",
                time,
                agent_short,
                mid,
                title,
                proj,
                type_short,
                width = max_title_width
            );
        }
    }
    println!();

    Ok(())
}

fn type_abbrev(mt: &MemoryType) -> &'static str {
    match mt {
        MemoryType::SessionSummary => "summary",
        MemoryType::Decision => "decision",
        MemoryType::Pattern => "pattern",
        MemoryType::UserInstruction => "instruction",
        MemoryType::Observation => "observation",
        MemoryType::BugRecord => "bug",
    }
}

pub fn handle_show(id: &str) -> Result<Option<MemoryEntry>> {
    let dir = config::memory_dir().join("memories");
    if !dir.is_dir() {
        anyhow::bail!("Memory directory not found");
    }

    // Find the file
    let entry = find_memory_by_id_prefix(id)?;

    // Print the full file content
    let content = std::fs::read_to_string(&entry.file_path)?;
    println!("{content}");

    // Print navigation footer
    println!("\n---\n");
    if let Some(ref sid) = entry.session_id {
        println!("**Session**: {} (`nibble session read {}`)", sid, sid);
        println!("             (`nibble memory by-session {}`)", sid);
    }
    if let Some(ref tid) = entry.task_id {
        println!("**Task**:     {} (`nibble memory archive {}`)", tid, tid);
    }

    Ok(Some(entry))
}

pub fn handle_by_session(session_id: &str) -> Result<()> {
    let all = store::list_memories(None, None, None, None)?;
    // Support prefix matching like show/forget do
    let matched: Vec<_> = all
        .into_iter()
        .filter(|m| {
            m.session_id
                .as_deref()
                .map(|s| s == session_id || s.starts_with(session_id))
                .unwrap_or(false)
        })
        .collect();

    if matched.is_empty() {
        println!("No memories linked to session {}.", session_id);
        return Ok(());
    }

    println!(
        "{} memory(ies) for session {}:\n",
        matched.len(),
        &session_id[..session_id.len().min(8)]
    );
    for entry in &matched {
        let date = entry.created_at.format("%Y-%m-%d").to_string();
        let display = entry.title.clone().unwrap_or_else(|| {
            let preview: String = entry.content.chars().take(80).collect();
            preview.trim_end_matches('…').to_string()
        });
        println!(
            "  {} [{}] {} ({})",
            &entry.memory_id[..8],
            entry.memory_type,
            display,
            date,
        );
    }

    Ok(())
}

pub fn handle_context(query: &str, project: Option<&str>, limit: usize) -> Result<()> {
    // Search memories
    let memories = search::search_memories(query, project, None, Some(limit))?;

    // Search active lessons
    let lessons =
        search::search_lessons_by_context(query, Some(&LessonStatus::Active), Some(limit))?;

    if memories.is_empty() && lessons.is_empty() {
        println!("No relevant context found for '{}'.", query);
        return Ok(());
    }

    println!("# Context Briefing: {}\n", query);

    if !memories.is_empty() {
        println!("## Recent Memories\n");
        for (i, m) in memories.iter().enumerate().take(limit) {
            let title = m.title.as_deref().unwrap_or("(no title)");
            let date = m.created_at.format("%Y-%m-%d").to_string();
            println!(
                "{}. **{}** — *{}* ({}, {})",
                i + 1,
                title,
                &m.content[..120.min(m.content.len())],
                m.memory_type,
                date
            );
            if !m.tags.is_empty() {
                println!("   Tags: {}", m.tags.join(", "));
            }
            println!();
        }
    }

    if !lessons.is_empty() {
        println!("## Active Lessons\n");
        for (i, l) in lessons.iter().enumerate().take(limit) {
            println!(
                "{}. **[{}]** {}",
                i + 1,
                l.severity,
                &l.content[..120.min(l.content.len())]
            );
            if !l.prevention.is_empty() {
                println!(
                    "   Prevention: {}",
                    &l.prevention[..100.min(l.prevention.len())]
                );
            }
            println!();
        }
    }

    Ok(())
}

pub fn handle_forget(id: &str) -> Result<()> {
    let path = store::forget_memory(id)?;

    let _ = index::reindex();
    let _ = index::regenerate_index_md();

    let cfg = config::load().unwrap_or_default();
    let base = config::memory_dir();
    let _ = git::commit(
        &base,
        &format!("memory: forget {}", &id[..8.min(id.len())]),
        &cfg.memory.sync.author_name,
        &cfg.memory.sync.author_email,
    );

    println!("Forgot memory: {}", id);
    println!("  File: {}", path.display());

    Ok(())
}

pub fn handle_stats(project: Option<&str>) -> Result<()> {
    // Try loading from cache first
    if project.is_none() {
        if let Some(cache) = index::load_index() {
            print_stats(&cache.stats);
            return Ok(());
        }
    }

    // Rebuild if no cache
    let stats = store::memory_stats(project)?;
    print_stats(&stats);
    Ok(())
}

fn print_stats(stats: &IndexStatStats) {
    println!("Memory Statistics");
    println!("─────────────────");
    println!("Total memories:  {}", stats.total_memories);
    println!("Total lessons:   {}", stats.total_lessons);
    println!("Active lessons:  {}", stats.active_lessons);

    if !stats.by_type.is_empty() {
        println!("\nBy type:");
        let mut types: Vec<_> = stats.by_type.iter().collect();
        types.sort_by(|a, b| b.1.cmp(a.1));
        for (t, count) in types {
            println!("  {t}: {count}");
        }
    }

    if let Some(oldest) = stats.oldest {
        println!("\nOldest: {}", oldest.format("%Y-%m-%d"));
    }
    if let Some(newest) = stats.newest {
        println!("Newest: {}", newest.format("%Y-%m-%d"));
    }
}

pub fn handle_inspect(project: Option<&str>) -> Result<()> {
    let base = config::memory_dir();
    let index_path = base.join("sessions").join("index.md");

    if index_path.exists() {
        let content = std::fs::read_to_string(&index_path)?;
        // Try to open in pager
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
    } else {
        // Just list the memories
        handle_list(project, None, None, None)?;
    }

    Ok(())
}

pub fn handle_reindex() -> Result<()> {
    println!("Rebuilding index cache...");
    index::reindex()?;
    println!("Rebuilding index.md...");
    index::regenerate_index_md()?;
    println!("Done.");
    Ok(())
}

pub fn handle_config() -> Result<()> {
    let cfg = config::load().unwrap_or_default();
    let base = config::memory_dir();
    let m = &cfg.memory;

    println!("Memory System Configuration");
    println!("───────────────────────────");
    println!();

    // ── Status ──────────────────────────────────────────────────────────────
    let status = if !m.enabled {
        "DISABLED"
    } else if base.join(".git").is_dir() {
        "ENABLED (initialized)"
    } else {
        "ENABLED (not initialized — run: nibble memory reindex)"
    };
    println!("  Status:            {status}");
    println!("  Memory dir:        {}", base.display());

    // ── Directory stats ─────────────────────────────────────────────────────
    if base.is_dir() {
        let mem_count = std::fs::read_dir(base.join("memories"))
            .map(|d| {
                d.filter(|e| {
                    e.as_ref()
                        .ok()
                        .map(|e| e.path().extension().and_then(|e| e.to_str()) == Some("md"))
                        .unwrap_or(false)
                })
                .count()
            })
            .unwrap_or(0);
        let lesson_count = std::fs::read_dir(base.join("lessons"))
            .map(|d| {
                d.filter(|e| {
                    e.as_ref()
                        .ok()
                        .map(|e| e.path().extension().and_then(|e| e.to_str()) == Some("md"))
                        .unwrap_or(false)
                })
                .count()
            })
            .unwrap_or(0);
        println!("  Memories:          {mem_count} files");
        println!("  Lessons:           {lesson_count} files");
    }

    println!();

    // ── LLM Settings ────────────────────────────────────────────────────────
    println!("  [memory.llm]");
    println!("  Provider:          {}", m.llm.provider);
    println!("  Base URL:          {}", m.llm.base_url);
    println!("  Model:             {}", m.llm.model);
    println!("  Embedding model:   {}", m.llm.embedding_model);
    println!("  Embedding dims:    {}", m.llm.embedding_dims);
    if m.llm.api_key.is_empty() {
        println!("  API key:           (not set — not needed for local LLMs)");
    } else {
        println!(
            "  API key:           {}****",
            &m.llm.api_key[..4.min(m.llm.api_key.len())]
        );
    }

    println!();

    // ── Git Sync ────────────────────────────────────────────────────────────
    println!("  [memory.sync]");
    if m.sync.remote.is_empty() {
        println!("  Remote:            (not configured — local-only)");
        println!();
        println!("  {}", "⚠ No remote configured. Memories stay local.",);
        println!("     Set up sync with:");
        println!("       1. Create a private repo on GitHub/GitLab");
        println!("       2. Edit ~/.nibble/config.toml:");
        println!("          [memory.sync]");
        println!("          remote = \"git@github.com:you/nibble-memory.git\"");
        println!("          auto_sync = true");
        println!("       3. Run: nibble memory sync");
    } else {
        println!("  Remote:            {}", m.sync.remote);
        println!(
            "  Auto-sync:         {}",
            if m.sync.auto_sync {
                "enabled"
            } else {
                "disabled"
            }
        );
        println!(
            "  Author:            {} <{}>",
            m.sync.author_name, m.sync.author_email
        );

        // Check if remote is actually configured in the git repo
        if base.join(".git").is_dir() {
            let remote_output = std::process::Command::new("git")
                .args(["-C", &base.to_string_lossy()])
                .args(["remote", "-v"])
                .output();
            match remote_output {
                Ok(o) => {
                    let stdout = String::from_utf8_lossy(&o.stdout);
                    if stdout.trim().is_empty() {
                        println!("  ⚠ Remote in config.toml but not set in git repo.");
                        println!("    Run: nibble memory sync");
                    } else {
                        println!("  Git remote:");
                        for line in stdout.lines().take(2) {
                            println!("    {}", line);
                        }
                    }
                }
                Err(_) => {}
            }
        }
    }

    println!();

    // ── Config file location ────────────────────────────────────────────────
    println!("  Config file:       {}", config::config_path().display());

    Ok(())
}

// ── Setup wizard ─────────────────────────────────────────────────────────────

/// Interactive setup wizard for memory system configuration.
///
/// Asks questions, then prints the generated config as TOML for the user to
/// copy-paste. Nothing is written or executed under the hood.
pub fn handle_setup() -> Result<()> {
    use std::io::{self, Write};

    let mut cfg = config::load().unwrap_or_default();

    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║          Nibble Memory — Setup Wizard                       ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    // Lock stdin once and reuse it for every prompt — avoids lock/unlock races.
    let mut stdin = io::stdin().lock();

    // ── Step 1: Enable / Disable ─────────────────────────────────────────
    println!("Step 1: Memory system");
    println!("─────────────────────");
    println!();
    println!("  Persistent memory captures decisions, patterns, and lessons across sessions.");
    println!(
        "  Currently: {}",
        if cfg.memory.enabled {
            "ENABLED"
        } else {
            "DISABLED"
        }
    );
    println!();

    let enabled = prompt_yes_no("Enable memory system?", cfg.memory.enabled, &mut stdin);
    cfg.memory.enabled = enabled;

    if !enabled {
        println!();
        println!("  Memory system will be disabled. Existing memories are preserved.");
        println!("  Re-run this wizard to re-enable.");
        print_config_toml(&cfg);
        return Ok(());
    }

    println!();

    // ── Step 2: LLM endpoint ────────────────────────────────────────────
    println!("Step 2: LLM endpoint (for future summarization & embeddings)");
    println!("──────────────────────────────────────────────────────────────");
    println!();
    println!("  The memory system uses a local LLM to summarize sessions and generate");
    println!("  embeddings for semantic search. If you don't have one running yet,");
    println!("  skip this step — keyword search works without an LLM.");
    println!();
    println!("  Current base URL: {}", cfg.memory.llm.base_url);
    println!("  Current model:    {}", cfg.memory.llm.model);
    println!();

    let change_llm = prompt_yes_no("Change LLM settings?", false, &mut stdin);

    if change_llm {
        println!();
        if let Some(url) = prompt_input(
            &format!("Base URL [{}]", cfg.memory.llm.base_url),
            Some(&cfg.memory.llm.base_url),
            &mut stdin,
        )? {
            cfg.memory.llm.base_url = url;
        }

        if let Some(model) = prompt_input(
            &format!("Model name [{}]", cfg.memory.llm.model),
            Some(&cfg.memory.llm.model),
            &mut stdin,
        )? {
            cfg.memory.llm.model = model.clone();
            cfg.memory.llm.embedding_model = model;
        }

        // Test connectivity
        let test_url = format!("{}/models", cfg.memory.llm.base_url.trim_end_matches('/'));
        println!();
        print!("  Testing connection to {}... ", cfg.memory.llm.base_url);
        io::stdout().flush()?;
        match reqwest_blocking_get(&test_url) {
            Ok(_) => println!("{}", "OK ✓".green()),
            Err(e) => {
                println!("{}", "FAILED ✗".red());
                println!("    Could not reach LLM: {e}");
                println!("    This is fine — keyword search works without it.");
            }
        }
    }

    println!();

    // ── Step 3: Sync remote ─────────────────────────────────────────────
    println!("Step 3: Git sync (backup & cross-device sync)");
    println!("────────────────────────────────────────────────");
    println!();
    println!("  Memories are stored as Markdown files in a local git repo.");
    println!("  Setting a remote pushes them to a private repo for backup and");
    println!("  syncing across machines.");
    println!();

    let current_remote = cfg.memory.sync.remote.clone();
    if current_remote.is_empty() {
        println!("  Current remote: (not configured — local-only)");
    } else {
        println!("  Current remote: {}", current_remote);
    }
    println!();

    let change_sync = prompt_yes_no("Configure sync remote?", false, &mut stdin);

    if change_sync {
        println!();
        println!("  Enter your private repo URL (SSH or HTTPS):");
        println!("    SSH:   git@github.com:you/nibble-memory.git");
        println!("    HTTPS: https://github.com/you/nibble-memory.git");
        println!("    Leave blank to remove the remote.");
        println!();

        if let Some(remote) = prompt_input(
            "Remote URL",
            if current_remote.is_empty() {
                None
            } else {
                Some(&current_remote)
            },
            &mut stdin,
        )? {
            cfg.memory.sync.remote = remote;
            let auto = prompt_yes_no(
                "Enable auto-sync (commit + push after every write)?",
                false,
                &mut stdin,
            );
            cfg.memory.sync.auto_sync = auto;
        } else {
            cfg.memory.sync.remote = String::new();
            cfg.memory.sync.auto_sync = false;
            println!("  Remote removed.");
        }
    }

    // Drop the stdin lock before we start printing the results
    drop(stdin);

    println!();
    println!("═══════════════════════════════════════════════════════════════");
    println!();

    // ── Print generated config ──────────────────────────────────────────
    print_config_toml(&cfg);

    println!();
    println!("═══════════════════════════════════════════════════════════════");
    println!();

    // ── Print next steps ────────────────────────────────────────────────
    if !cfg.memory.sync.remote.is_empty() {
        let base = config::memory_dir();
        println!("Next steps:");
        println!();
        println!("  1. Copy the config above into ~/.nibble/config.toml");
        println!("  2. Wire the git remote:");
        if base.join(".git").is_dir() {
            println!(
                "       git -C {} remote add origin {}",
                base.display(),
                cfg.memory.sync.remote
            );
        } else {
            println!("       nibble memory reindex   # creates the git repo first");
            println!(
                "       git -C {} remote add origin {}",
                base.display(),
                cfg.memory.sync.remote
            );
        }
        println!("  3. Push your memories:");
        println!("       nibble memory sync");
        println!();
    } else {
        println!("Next step:");
        println!();
        println!("  Copy the config above into ~/.nibble/config.toml");
        println!();
    }

    println!("  Run `nibble memory config` anytime to view your settings.");
    println!("  Run `nibble memory config --setup` to reconfigure.");
    println!();

    Ok(())
}

/// Print the [memory] section of the config as TOML.
fn print_config_toml(cfg: &config::Config) {
    println!("Add this to {}:", config::config_path().display());
    println!();
    println!("[memory]");
    println!("enabled = {}", cfg.memory.enabled);
    println!();
    println!("[memory.llm]");
    println!("provider = \"{}\"", cfg.memory.llm.provider);
    println!("base_url = \"{}\"", cfg.memory.llm.base_url);
    println!("model = \"{}\"", cfg.memory.llm.model);
    println!("embedding_model = \"{}\"", cfg.memory.llm.embedding_model);
    println!("embedding_dims = {}", cfg.memory.llm.embedding_dims);
    if !cfg.memory.llm.api_key.is_empty() {
        println!("api_key = \"{}\"", cfg.memory.llm.api_key);
    }
    println!();
    println!("[memory.sync]");
    if !cfg.memory.sync.remote.is_empty() {
        println!("remote = \"{}\"", cfg.memory.sync.remote);
        println!("auto_sync = {}", cfg.memory.sync.auto_sync);
    } else {
        println!("# remote = \"git@github.com:you/nibble-memory.git\"");
        println!("# auto_sync = true");
    }
    println!("author_name = \"{}\"", cfg.memory.sync.author_name);
    println!("author_email = \"{}\"", cfg.memory.sync.author_email);
}

fn reqwest_blocking_get(url: &str) -> Result<String, String> {
    let output = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--connect-timeout",
            "3",
            url,
        ])
        .output()
        .map_err(|e| format!("curl not available: {e}"))?;

    let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if status.starts_with('2') || status.starts_with('3') {
        Ok(status)
    } else {
        Err(format!("HTTP {status}"))
    }
}

/// Prompt user with a yes/no question. Returns the chosen value.
/// Defaults to `default` on empty input.
fn prompt_yes_no(question: &str, default: bool, stdin: &mut impl BufRead) -> bool {
    let suffix = if default { " [Y/n] " } else { " [y/N] " };
    print!("  {question}{suffix}");
    std::io::stdout().flush().unwrap();

    let mut input = String::new();
    let _ = stdin.read_line(&mut input);
    let answer = input.trim().to_lowercase();

    if answer.is_empty() {
        return default;
    }
    answer == "y" || answer == "yes"
}

/// Prompt for text input. Returns Some(value) if the user entered something,
/// None if they left it blank (keep existing).
fn prompt_input(
    label: &str,
    current: Option<&str>,
    stdin: &mut impl BufRead,
) -> Result<Option<String>, anyhow::Error> {
    let hint = match current {
        Some(c) => format!(" [{c}]"),
        None => String::new(),
    };
    print!("  {label}{hint}: ");
    std::io::stdout().flush().unwrap();

    let mut input = String::new();
    let _ = stdin.read_line(&mut input);
    let trimmed = input.trim().to_string();

    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed))
    }
}

// Helper trait for colored output (zero-dependency)
trait Colored {
    fn green(&self) -> String;
    fn red(&self) -> String;
}

impl Colored for str {
    fn green(&self) -> String {
        format!("\u{001b}[32m{}\u{001b}[0m", self)
    }
    fn red(&self) -> String {
        format!("\u{001b}[31m{}\u{001b}[0m", self)
    }
}

pub fn handle_sync() -> Result<()> {
    let cfg = config::load().unwrap_or_default();
    let base = config::memory_dir();

    if !base.join(".git").is_dir() {
        anyhow::bail!("Memory directory is not a git repo. Run 'nibble memory reindex' first.");
    }

    println!("Syncing memory store...");
    git::sync(
        &base,
        "memory: sync",
        &cfg.memory.sync.author_name,
        &cfg.memory.sync.author_email,
    )?;
    println!("Done.");

    Ok(())
}

// ── Lesson commands ──────────────────────────────────────────────────────────

pub fn handle_lessons(
    context: Option<&str>,
    status: Option<&str>,
    severity: Option<&str>,
    limit: Option<usize>,
) -> Result<()> {
    let st = status.map(|s| LessonStatus::from_str_lossy(s));
    let sev = severity.map(|s| LessonSeverity::from_str_lossy(s));

    let lessons = if let Some(ctx) = context {
        search::search_lessons_by_context(ctx, st.as_ref(), limit)?
    } else {
        store::list_lessons(st.as_ref(), sev.as_ref(), None, limit)?
    };

    if lessons.is_empty() {
        println!("No lessons found.");
        return Ok(());
    }

    println!("{} lesson(s):\n", lessons.len());
    for lesson in &lessons {
        let preview: String = lesson.content.chars().take(120).collect();
        println!(
            "  [{}][{}] {} (×{})",
            lesson.severity,
            lesson.status,
            preview.trim_end_matches('…'),
            lesson.occurrence_count,
        );
        println!("    id: {}", lesson.lesson_id);
        if !lesson.prevention.is_empty() {
            let prev: String = lesson.prevention.chars().take(80).collect();
            println!("    prevention: {}", prev.trim_end_matches('…'));
        }
        println!();
    }

    Ok(())
}

pub fn handle_lesson_add(
    content: &str,
    category: &str,
    severity: &str,
    prevention: &str,
    project: Option<&str>,
    tags: Option<&str>,
) -> Result<()> {
    let cat = LessonCategory::from_str_lossy(category);
    let sev = LessonSeverity::from_str_lossy(severity);
    let tag_vec: Vec<String> = tags
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    let project = project.map(|s| s.to_string()).or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string()))
    });

    let (path, id) = store::write_lesson(
        content,
        prevention,
        &cat,
        &sev,
        project.as_deref(),
        &tag_vec,
        None,
    )?;

    let _ = index::reindex();
    let _ = index::regenerate_index_md();

    let cfg = config::load().unwrap_or_default();
    let base = config::memory_dir();
    let _ = git::commit(
        &base,
        &format!("memory: add lesson '{}'", &content[..50.min(content.len())]),
        &cfg.memory.sync.author_name,
        &cfg.memory.sync.author_email,
    );

    println!("Created lesson: {id}");
    println!("  Category: {cat}");
    println!("  Severity: {sev}");
    println!("  File: {}", path.display());

    Ok(())
}

pub fn handle_lesson_resolve(id: &str, note: Option<&str>) -> Result<()> {
    let path = store::resolve_lesson(id, note)?;

    let _ = index::reindex();
    let _ = index::regenerate_index_md();

    let cfg = config::load().unwrap_or_default();
    let base = config::memory_dir();
    let _ = git::commit(
        &base,
        &format!("memory: resolve lesson {}", &id[..8.min(id.len())]),
        &cfg.memory.sync.author_name,
        &cfg.memory.sync.author_email,
    );

    println!("Resolved lesson: {id}");
    println!("  File: {}", path.display());

    Ok(())
}

// ── Capture command (internal, called by hooks) ──────────────────────────────

pub fn handle_capture(
    task_id: &str,
    role: &str,
    content: &str,
    tool_name: Option<&str>,
    tool_input: Option<&str>,
    tool_output: Option<&str>,
) -> Result<()> {
    use std::io::Write;

    // Look up the task to find project name
    let db_path = crate::db::default_db_path();
    let db = crate::db::Database::open(&db_path)?;
    let task = db.get_task_by_id(task_id)?;
    let project = task
        .as_ref()
        .and_then(|t| t.context.as_ref())
        .and_then(|c| c.project_path.as_ref())
        .map(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let base = config::memory_dir().join("capture").join(&project);
    std::fs::create_dir_all(&base)?;

    let capture_file = base.join(format!("{}.jsonl", task_id));

    // Append event to JSONL
    let mut event = serde_json::json!({
        "ts": Utc::now().to_rfc3339(),
        "role": role,
    });

    if role == "tool" {
        if let Some(name) = tool_name {
            event["name"] = serde_json::Value::String(name.to_string());
        }
        if let Some(input) = tool_input {
            event["input"] = serde_json::Value::String(input.chars().take(4096).collect());
        }
        if let Some(output) = tool_output {
            event["output"] = serde_json::Value::String(output.chars().take(4096).collect());
        }
    } else {
        event["content"] = serde_json::Value::String(content.to_string());
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&capture_file)?;
    writeln!(file, "{}", serde_json::to_string(&event)?)?;

    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn find_memory_by_id_prefix(id_prefix: &str) -> Result<MemoryEntry> {
    let dir = config::memory_dir().join("memories");
    if !dir.is_dir() {
        anyhow::bail!("No memories directory");
    }

    // Scan for files whose memory_id starts with the prefix
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Ok(mem) = store::parse_memory_entry(&path) {
            if mem.memory_id.starts_with(id_prefix) || mem.memory_id == id_prefix {
                return Ok(mem);
            }
        }
    }

    anyhow::bail!("Memory not found: {}", id_prefix)
}

// ── Dedup command ───────────────────────────────────────────────────────────

pub fn handle_dedup(yes: bool) -> Result<()> {
    let all = store::list_memories(None, None, None, None)?;

    // Group session_summary memories by session_id
    let mut by_session: std::collections::HashMap<String, Vec<MemoryEntry>> =
        std::collections::HashMap::new();
    for m in all {
        if m.memory_type == MemoryType::SessionSummary {
            if let Some(ref sid) = m.session_id {
                by_session.entry(sid.clone()).or_default().push(m);
            }
        }
    }

    let mut to_delete: Vec<String> = Vec::new();
    for (sid, mut memories) in by_session {
        if memories.len() <= 1 {
            continue;
        }
        // Sort by created_at descending (newest first)
        memories.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let keep = &memories[0];
        println!(
            "Session {} has {} duplicates — keeping {} ({})",
            &sid[..sid.len().min(8)],
            memories.len(),
            &keep.memory_id[..8],
            keep.created_at.format("%Y-%m-%d %H:%M")
        );
        for dup in &memories[1..] {
            println!(
                "  will delete {} ({})",
                &dup.memory_id[..8],
                dup.created_at.format("%Y-%m-%d %H:%M")
            );
            to_delete.push(dup.memory_id.clone());
        }
    }

    if to_delete.is_empty() {
        println!("No duplicate session_summary memories found.");
        return Ok(());
    }

    if !yes {
        println!(
            "\n{} duplicate(s) would be deleted. Run with --yes to actually delete.",
            to_delete.len()
        );
        return Ok(());
    }

    let mut deleted = 0;
    for id in &to_delete {
        if let Ok(path) = store::forget_memory(id) {
            println!("Deleted: {} ({})", id, path.display());
            deleted += 1;
        } else {
            eprintln!("Failed to delete: {}", id);
        }
    }

    // Rebuild index
    let _ = index::reindex();
    let _ = index::regenerate_index_md();

    println!("\nDeleted {} duplicate(s).", deleted);
    Ok(())
}

// ── Archive command ──────────────────────────────────────────────────────────

pub fn handle_archive(task_id: &str) -> Result<()> {
    match crate::memory::archive::archive_session(task_id)? {
        Some(path) => {
            println!("Archived session {} to {}", task_id, path.display());
        }
        None => {
            println!(
                "Could not archive session {} — no session file found on disk.",
                task_id
            );
            println!("The capture file (if any) is still in ~/.nibble/memory/capture/");
        }
    }
    Ok(())
}

// ── Summarize command ────────────────────────────────────────────────────────

pub fn handle_summarize(task_id: &str, force: bool, from_pi_session: Option<&str>) -> Result<()> {
    let count = if let Some(pi_path) = from_pi_session {
        let path = std::path::PathBuf::from(pi_path);
        crate::memory::summarize::summarize_pi_session(task_id, &path, force)?
    } else {
        crate::memory::summarize::summarize_session(task_id, force)?
    };
    if count > 0 {
        println!(
            "Summarized session {} — wrote {} memory/lesson files.",
            task_id, count
        );
    } else {
        println!(
            "Session {} summarized — nothing worth remembering.",
            task_id
        );
    }
    Ok(())
}
