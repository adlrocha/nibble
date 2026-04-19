//! Telegram long-polling listener daemon.
//!
//! Runs as a blocking loop (`nibble listen`), calling `getUpdates` with a
//! 30-second long-poll timeout.  For each incoming update it:
//!
//! - Verifies the sender is the authorised user (chat_id whitelist).
//! - Routes **callback queries** (inline button taps):
//!   - `reply:{task_id}` → stores pending_reply in kv_store and sends an
//!     acknowledgment message (stored in bot_messages) so the user's next
//!     message routes to the correct task via either mechanism.
//! - Routes **text messages**:
//!   - If a pending_reply is set (user tapped a reply button) → injects into
//!     the associated task via `claude --resume`.
//!   - If the message is a reply to a known bot message (ack or notification)
//!     → injects into the associated task.
//!   - Otherwise → ignored (with a polite notice).
//!
//! ## Routing mechanism
//!
//! Every bot message that can receive a reply has its `message_id` stored in
//! the `bot_messages` table linked to a `task_id`.  When the user's reply
//! arrives, `reply_to_message.message_id` is looked up in that table to find
//! the target task.  This is stateless and survives daemon restarts.
//!
//! Additionally, tapping an inline "↩ Reply" button stores a `pending_reply`
//! key in kv_store so the user's *next* free-text message routes to that task
//! without needing an explicit reply gesture.
//!
//! The current `poll_offset` is persisted to the SQLite kv_store after every
//! batch so that a daemon restart does not re-process old updates.
//!
//! ## Long-running turns
//!
//! Injection (`claude --resume`) can take many minutes for complex tasks.
//! Rather than blocking the listener loop, injection is dispatched to a
//! background thread.  That thread:
//! - Sends a "⏳ working…" heartbeat to Telegram every 2 minutes.
//! - After the Claude process exits, sends a safety-net completion notification
//!   if the Stop hook inside the container didn't already send one (e.g. because
//!   the hook timed out or the container crashed).

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::agent_input;
use crate::config::TelegramConfig;
use crate::cron;
use crate::db::Database;
use crate::models::TaskStatus;
use crate::notifications::telegram;
use crate::sandbox::podman::PodmanSandbox;
use crate::sandbox::{Sandbox, SandboxHealth};

const POLL_TIMEOUT_SECS: u64 = 30;
const POLL_OFFSET_KEY: &str = "telegram_poll_offset";
const PENDING_REPLY_PREFIX: &str = "pending_reply:";
/// Run a prune pass every this many polling loops (~5 minutes at 30s/poll).
const PRUNE_EVERY_N_POLLS: u32 = 10;
/// Check for due cron jobs every this many polling loops (~1 minute at 30s/poll).
const CRON_CHECK_EVERY_N_POLLS: u32 = 2;
/// Send a "still working" heartbeat this often during a long inject turn.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(120);

// ── Public entry point ────────────────────────────────────────────────────────

/// Run the long-polling daemon indefinitely.  Call from `main` for the
/// `Commands::Listen` variant.
pub fn run(db: &Database, config: &TelegramConfig) -> Result<()> {
    // Prevent running inside a sandbox container — the host binary is bind-mounted
    // into containers and an agent or hook could accidentally start a second listener,
    // causing HTTP 409 conflicts with the host's listener.
    if std::path::Path::new("/home/node/.nibble").exists() && std::env::var("AGENT_TASK_ID").is_ok()
    {
        anyhow::bail!(
            "Refusing to run 'listen' inside a sandbox container (AGENT_TASK_ID is set). \
             The Telegram listener must run on the host only."
        );
    }

    eprintln!("[listen] Starting Telegram long-polling daemon…");

    // Register bot command menu so /commands appear in Telegram's picker.
    match telegram::register_commands(config) {
        Ok(()) => eprintln!("[listen] Command menu registered with Telegram"),
        Err(e) => eprintln!("[listen] Warning: failed to register commands: {e}"),
    }

    // Clear any stale running flags left by a previous crash.
    if let Err(e) = db.reset_all_cron_running_flags() {
        eprintln!("[listen] Warning: failed to reset cron running flags: {e}");
    }

    let mut offset: i64 = db
        .kv_get(POLL_OFFSET_KEY)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let db_path = crate::db::default_db_path();
    let mut poll_count: u32 = 0;
    let mut retry_delay_secs: u64 = 5;
    let mut consecutive_errors: u32 = 0;

    loop {
        match get_updates(config, offset) {
            Ok(updates) => {
                retry_delay_secs = 5;
                consecutive_errors = 0;
                let fresh_db = match Database::open(&db_path) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("[listen] Failed to open DB: {e:#}. Retrying…");
                        thread::sleep(Duration::from_secs(5));
                        continue;
                    }
                };

                eprintln!(
                    "[listen] Poll ok: {} update(s), offset={offset}",
                    updates.len()
                );

                for update in &updates {
                    let update_id = update
                        .get("update_id")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    offset = offset.max(update_id + 1);

                    if let Err(e) = handle_update(update, config, &fresh_db) {
                        eprintln!("[listen] Error handling update {update_id}: {e:#}");
                    }
                }

                let _ = fresh_db.kv_set(POLL_OFFSET_KEY, &offset.to_string());

                poll_count += 1;
                if poll_count % PRUNE_EVERY_N_POLLS == 0 {
                    eprintln!("[listen] Running periodic prune…");
                    if let Err(e) = crate::prune_stale_tasks(&fresh_db) {
                        eprintln!("[listen] prune error: {e:#}");
                    }
                    eprintln!("[listen] Prune done");
                }
                if poll_count % CRON_CHECK_EVERY_N_POLLS == 0 {
                    if let Err(e) = check_and_run_cron_jobs(&fresh_db, config) {
                        eprintln!("[listen] cron error: {e:#}");
                    }
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors == 1 {
                    eprintln!("[listen] getUpdates error: {e:#}. Retrying with backoff (suppressing further repeats)…");
                }
                thread::sleep(Duration::from_secs(retry_delay_secs));
                retry_delay_secs = (retry_delay_secs * 2).min(300);
            }
        }
    }
}

// ── Update dispatch ───────────────────────────────────────────────────────────

fn handle_update(update: &serde_json::Value, config: &TelegramConfig, db: &Database) -> Result<()> {
    let update_type = if update.get("callback_query").is_some() {
        "callback_query"
    } else if update.get("message").is_some() {
        "message"
    } else {
        "unknown"
    };
    eprintln!("[listen] handle_update type={update_type}");

    if let Some(cq) = update.get("callback_query") {
        return handle_callback_query(cq, config, db);
    }

    if let Some(msg) = update.get("message") {
        return handle_message(msg, config, db);
    }

    Ok(())
}

// ── Callback query handler (inline button taps) ───────────────────────────────

fn handle_callback_query(
    cq: &serde_json::Value,
    config: &TelegramConfig,
    db: &Database,
) -> Result<()> {
    let cq_id = cq
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let from_id = cq.pointer("/from/id").and_then(|v| v.as_i64()).unwrap_or(0);
    let from_username = cq.pointer("/from/username").and_then(|v| v.as_str());

    // Always acknowledge first (removes the loading spinner).
    let _ = telegram::answer_callback_query(config, &cq_id);

    if !is_authorised(config, from_id, from_username) {
        eprintln!("[listen] Ignoring callback from unauthorised user {from_id}");
        return Ok(());
    }

    let data = match cq.get("data").and_then(|v| v.as_str()) {
        Some(d) => d,
        None => return Ok(()),
    };

    eprintln!("[listen] Callback from user {from_id}: data={data:?}");

    if let Some(task_id) = data.strip_prefix("reply:") {
        let chat_id = cq
            .pointer("/message/chat/id")
            .and_then(|v| v.as_i64())
            .unwrap_or(from_id);

        // Validate task exists before setting up routing — fail early with a
        // clear error rather than letting the user type a message that will
        // then be rejected.
        match db.get_task_by_id(task_id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                eprintln!("[listen] Callback reply: task {task_id} not found in DB");
                let _ = telegram::answer_callback_query_with_text(
                    config,
                    &cq_id,
                    &format!("⚠️ Task {} not found", &task_id[..task_id.len().min(8)]),
                );
                return Ok(());
            }
            Err(e) => {
                eprintln!("[listen] DB error looking up task {task_id}: {e:#}");
            }
        }

        let key = format!("{}{}", PENDING_REPLY_PREFIX, chat_id);
        if let Err(e) = db.kv_set(&key, task_id) {
            eprintln!("[listen] Failed to store pending reply: {e:#}");
        }

        let sandbox_msg_id = cq
            .pointer("/message/message_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        match telegram::send_reply(config, "✏️ Type your reply and send it:", sandbox_msg_id) {
            Ok(prompt_msg_id) => {
                if let Err(e) = db.insert_bot_message(prompt_msg_id, task_id) {
                    eprintln!("[listen] Failed to store prompt message_id: {e:#}");
                }
            }
            Err(e) => eprintln!("[listen] Failed to send reply prompt: {e:#}"),
        }

        eprintln!("[listen] Pending reply set: chat={chat_id} task={task_id}");
    }

    Ok(())
}

// ── Text message handler ──────────────────────────────────────────────────────

fn handle_message(msg: &serde_json::Value, config: &TelegramConfig, db: &Database) -> Result<()> {
    // Ignore messages sent by the bot itself.
    if msg
        .pointer("/from/is_bot")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Ok(());
    }

    let from_id = msg
        .pointer("/from/id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let from_username = msg.pointer("/from/username").and_then(|v| v.as_str());

    if !is_authorised(config, from_id, from_username) {
        eprintln!("[listen] Ignoring message from unauthorised user {from_id}");
        return Ok(());
    }

    let chat_id = msg
        .pointer("/chat/id")
        .and_then(|v| v.as_i64())
        .unwrap_or(from_id);

    let text = match msg.get("text").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return Ok(()),
    };

    eprintln!(
        "[listen] Text message from {from_id}: {:?}",
        &text[..text.len().min(80)]
    );

    // Priority 0: Help command
    if text.trim() == "/help" || text.trim().starts_with("/help@") {
        return handle_help_command(config, chat_id);
    }

    if text.trim() == "/sandboxes" || text.trim().starts_with("/sandboxes@") {
        return handle_sandboxes_command(config, db, chat_id);
    }

    if text.trim() == "/cron"
        || text.trim().starts_with("/cron ")
        || text.trim().starts_with("/cron@")
    {
        let args = text.trim().strip_prefix("/cron").unwrap_or("").trim();
        return handle_cron_command(config, db, chat_id, args);
    }

    // Priority 0c: /spawn <repo_path> [task description]
    let trimmed = text.trim_start();
    let spawn_args = if trimmed == "/spawn" {
        Some("")
    } else if let Some(rest) = trimmed.strip_prefix("/spawn ") {
        Some(rest)
    } else {
        None
    };
    if let Some(args) = spawn_args {
        let args = args.trim();
        let (repo_path, task_desc) = if args.is_empty() {
            (None, None)
        } else {
            let mut parts = args.splitn(2, char::is_whitespace);
            let path = parts.next().map(str::trim).filter(|s| !s.is_empty());
            let desc = parts
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            (path.map(str::to_string), desc)
        };
        return handle_spawn_command(config, db, chat_id, repo_path, task_desc);
    }

    // Priority 1: pending-reply set by tapping an inline ↩ Reply button.
    // Consume it so the next message goes back to normal routing.
    let pending_key = format!("{}{}", PENDING_REPLY_PREFIX, chat_id);
    match db.kv_get(&pending_key) {
        Ok(Some(task_id)) if !task_id.is_empty() => {
            eprintln!("[listen] Routing via pending reply to task {task_id}");
            let _ = db.kv_delete(&pending_key);
            return route_text_to_task(&task_id, &text, config, db, chat_id);
        }
        Ok(Some(empty)) => {
            eprintln!("[listen] Pending reply key existed but was empty: {empty:?}");
            let _ = db.kv_delete(&pending_key);
        }
        Ok(None) => {
            eprintln!("[listen] No pending reply for chat={chat_id}");
        }
        Err(e) => {
            eprintln!("[listen] DB error reading pending reply: {e:#}");
        }
    }

    // Priority 2: explicit Telegram reply to a known bot message.
    if let Some(reply_to) = msg.get("reply_to_message") {
        if let Some(orig_id) = reply_to.get("message_id").and_then(|v| v.as_i64()) {
            eprintln!("[listen] Reply to message_id={orig_id}");
            if let Some(task_id) = db.get_task_id_by_message_id(orig_id)? {
                return route_text_to_task(&task_id, &text, config, db, chat_id);
            }
            eprintln!("[listen] message_id={orig_id} not found in bot_messages");
        }
    }

    eprintln!("[listen] No routing found — not a pending reply or reply to a known message");
    send_notice(
        config,
        chat_id,
        "ℹ️ Tap ↩ Reply on a sandbox notification or use /sandboxes to start a conversation.",
    )?;

    Ok(())
}

fn handle_sandboxes_command(config: &TelegramConfig, db: &Database, _chat_id: i64) -> Result<()> {
    let sandbox = PodmanSandbox::new();
    let states = db.list_container_states()?;

    eprintln!(
        "[sandboxes] checking {} container state entries",
        states.len()
    );

    let mut running: Vec<(String, String)> = Vec::new();
    let mut restarted: Vec<String> = Vec::new();
    for (task_id, container_name, repo_path, _, _created) in &states {
        let health = sandbox.health_check(container_name);
        eprintln!("[sandboxes] container={container_name} health={health:?}");
        match health {
            crate::sandbox::SandboxHealth::Healthy | crate::sandbox::SandboxHealth::Degraded => {
                let label = std::path::Path::new(repo_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(repo_path.as_str())
                    .to_string();
                running.push((task_id.clone(), label));
            }
            crate::sandbox::SandboxHealth::Stopped => {
                // Auto-restart stopped containers so the user can interact.
                eprintln!("[sandboxes] container={container_name} stopped → attempting restart");
                if sandbox.start(container_name).is_ok() {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    if sandbox.health_check(container_name) != crate::sandbox::SandboxHealth::Dead {
                        let label = std::path::Path::new(repo_path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(repo_path.as_str())
                            .to_string();
                        running.push((task_id.clone(), label.clone()));
                        restarted.push(label);
                        if let Ok(Some(mut task)) = db.get_task_by_id(task_id) {
                            if task.status != TaskStatus::Running {
                                task.set_running();
                                let _ = db.update_task(&task);
                            }
                        }
                    }
                }
            }
            crate::sandbox::SandboxHealth::Dead => {
                eprintln!("[sandboxes] container={container_name} dead/gone — skipping");
            }
        }
    }

    if running.is_empty() {
        telegram::send(config, "🤖 No running sandboxes.")?;
        return Ok(());
    }

    let sandboxes: Vec<(&str, &str)> = running
        .iter()
        .map(|(id, label)| (id.as_str(), label.as_str()))
        .collect();

    telegram::send_sandbox_list(config, &sandboxes)?;

    if !restarted.is_empty() {
        let msg = format!(
            "🔄 Restarted stopped sandbox{}: {}",
            if restarted.len() > 1 { "s" } else { "" },
            restarted.join(", ")
        );
        send_notice(config, config.chat_id.parse().unwrap_or(0), &msg)?;
    }

    Ok(())
}

fn handle_help_command(config: &TelegramConfig, chat_id: i64) -> Result<()> {
    let help_text = r#"🐾 *Nibble Commands*

*/help* - Show this help message

*/sandboxes* - List running sandboxes with reply buttons

*/spawn <repo_path> [task]* - Spawn a new sandbox
  Example: `/spawn ~/projects/myapp fix bug`

*/cron* - Manage scheduled prompts
  `/cron list` - List all cron jobs
  `/cron list <repo_name>` - List cron jobs for a specific repo

*Reply to any task notification* to send a message to that agent.
"#;

    send_notice(config, chat_id, help_text)?;
    Ok(())
}

fn handle_cron_command(
    config: &TelegramConfig,
    db: &Database,
    chat_id: i64,
    args: &str,
) -> Result<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();

    if parts.is_empty() || parts[0] == "list" {
        // Optional filter: if the arg looks like a path prefix, match against repo_path basename
        let repo_filter: Option<String> = if parts.len() > 1 {
            let arg = parts[1];
            // If it starts with '/', treat as canonical path; otherwise match as repo basename suffix
            if arg.starts_with('/') {
                Some(arg.to_string())
            } else {
                // Find all cron jobs whose repo basename matches the arg
                let all_jobs = db.list_cron_jobs(None)?;
                let matched: Vec<_> = all_jobs
                    .iter()
                    .filter(|j| {
                        std::path::Path::new(&j.repo_path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with(arg))
                            .unwrap_or(false)
                    })
                    .map(|j| j.repo_path.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                match matched.len() {
                    0 => {
                        send_notice(
                            config,
                            chat_id,
                            &format!("⚠️ No cron jobs found for repo matching: {}", arg),
                        )?;
                        return Ok(());
                    }
                    1 => Some(matched.into_iter().next().unwrap()),
                    _ => None, // show all if ambiguous
                }
            }
        } else {
            None
        };

        let jobs = db.list_cron_jobs(repo_filter.as_deref())?;

        if jobs.is_empty() {
            if repo_filter.is_some() {
                send_notice(config, chat_id, "No cron jobs for this repo.")?;
            } else {
                send_notice(config, chat_id, "No cron jobs configured.")?;
            }
            return Ok(());
        }

        let now = chrono::Utc::now();
        let mut lines = vec!["🕐 *Cron Jobs*\n".to_string()];

        for job in jobs {
            let repo_label = std::path::Path::new(&job.repo_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&job.repo_path);
            let label = job.label.as_deref().unwrap_or("unnamed");
            let status = if job.enabled { "✅" } else { "⏹️" };

            let next_str = if job.next_run <= now {
                "due now".to_string()
            } else {
                let diff = job.next_run.signed_duration_since(now);
                let total_secs = diff.num_seconds();
                let hours = total_secs / 3600;
                let mins = (total_secs % 3600) / 60;
                let secs = total_secs % 60;
                if hours >= 24 {
                    format!("in {}d", diff.num_days())
                } else if hours > 0 {
                    format!("in {}h {}min", hours, mins)
                } else if mins > 0 {
                    format!("in {}min {}s", mins, secs)
                } else {
                    format!("in {}s", secs)
                }
            };

            let exp_str = match job.expires_at {
                Some(exp) => format!(" | Expires: {}", exp.format("%Y-%m-%d")),
                None => String::new(),
            };
            lines.push(format!(
                "{} *{}*\n  Repo: `{}` | Next: {}{}\n  Schedule: `{}`\n",
                status, label, repo_label, next_str, exp_str, job.schedule
            ));
        }

        send_notice(config, chat_id, &lines.join("\n"))?;
        return Ok(());
    }

    // Unknown subcommand
    send_notice(config, chat_id, "Unknown /cron command. Try:\n/cron list")?;
    Ok(())
}

fn handle_spawn_command(
    config: &TelegramConfig,
    _db: &Database,
    chat_id: i64,
    repo_path: Option<String>,
    task_desc: Option<String>,
) -> Result<()> {
    let repo_path = match repo_path {
        Some(p) => p,
        None => {
            send_notice(
                config,
                chat_id,
                "Usage: `/spawn /path/to/repo [optional task description]`",
            )?;
            return Ok(());
        }
    };

    let repo_label = std::path::Path::new(&repo_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&repo_path)
        .to_string();

    send_notice(
        config,
        chat_id,
        &format!("⏳ Spawning sandbox for `{repo_label}`…"),
    )?;

    let config_clone = config.clone();
    let db_path = crate::db::default_db_path();

    thread::spawn(move || {
        // Open a fresh DB connection for the background thread.
        let db = match crate::db::Database::open(&db_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[spawn] Could not open DB: {e:#}");
                let _ = telegram::send(&config_clone, &format!("❌ Spawn failed: {e}"));
                return;
            }
        };

        match crate::cmd_sandbox_spawn(
            &db,
            repo_path,
            task_desc,
            "nibble-sandbox:latest".to_string(),
            false, // fresh
            None,  // session_id
            true,  // no_attach
            false, // opencode
            false, // factory
            false, // hermes
            false, // pi
        ) {
            Ok(task_id) => {
                let msg = format!(
                    "✅ Sandbox started for `{repo_label}`\n\nUse `/sandboxes` to interact."
                );
                let _ = telegram::send_with_reply_button(&config_clone, &msg, &task_id)
                    .map(|msg_id| db.insert_bot_message(msg_id, &task_id));
            }
            Err(e) => {
                eprintln!("[spawn] cmd_sandbox_spawn failed: {e:#}");
                let _ = telegram::send(&config_clone, &format!("❌ Spawn failed: {e}"));
            }
        }
    });

    Ok(())
}

fn route_text_to_task(
    task_id: &str,
    text: &str,
    config: &TelegramConfig,
    db: &Database,
    chat_id: i64,
) -> Result<()> {
    let task = match db.get_task_by_id(task_id)? {
        Some(t) => t,
        None => {
            eprintln!("[listen] Task not found: {task_id}");
            send_notice(
                config,
                chat_id,
                &format!(
                    "⚠️ Task {} not found. Use /sandboxes to see available sandboxes.",
                    &task_id[..task_id.len().min(8)]
                ),
            )?;
            return Ok(());
        }
    };

    eprintln!("[listen] Injecting into task {task_id}, pid={:?}", task.pid);

    // Check container health before attempting inject — this gives us a clear
    // error message early rather than a cryptic "session ended" later.
    if let Some(ref container_id) = task.container_id {
        if let Err(e) = agent_input::check_container_health(container_id) {
            eprintln!("[listen] Container health check failed for {task_id}: {e:#}");
            send_notice(config, chat_id, &format!("❌ Cannot send message: {e}"))?;
            return Ok(());
        }
    }

    // Record the bot message count BEFORE we start the inject.
    // This is used by the safety-net to detect if Stop hook sent a notification.
    // Using a count rather than a timestamp avoids clock-skew and SQLite WAL issues.
    let messages_before = db.bot_message_count_for_task(task_id).unwrap_or(0);

    // Acknowledge immediately so the user knows their message was received.
    // Store the ack message_id in bot_messages so the user can reply to this
    // ack to send a follow-up message to the same sandbox.
    match send_notice_returning_id(config, chat_id, "📨 Message sent to agent.") {
        Ok(ack_msg_id) => {
            if let Err(e) = db.insert_bot_message(ack_msg_id, &task.task_id) {
                eprintln!("[listen] Failed to store ack message_id: {e:#}");
            }
        }
        Err(e) => eprintln!("[listen] Failed to send ack: {e:#}"),
    }

    // Spawn the actual inject in a background thread so the listener loop stays
    // live and can process other Telegram updates while Claude is working.
    let config_clone = config.clone();
    let task_id_owned = task_id.to_string();
    let text_owned = text.to_string();
    let db_path = crate::db::default_db_path();

    thread::spawn(move || {
        inject_with_heartbeat(
            &task,
            &text_owned,
            &config_clone,
            &task_id_owned,
            &db_path,
            messages_before,
        );
    });

    Ok(())
}

/// Maximum time to wait for the Stop hook to send its notification after the
/// inject process exits.  We poll every second and fire the safety-net as soon
/// as we see the hook's bot_message row, so in the happy path latency is ~1s.
/// The upper bound only matters if the hook is slow or crashes.
const STOP_HOOK_TIMEOUT_SECS: u64 = 30;

/// Run inject inside a background thread, sending periodic heartbeats and a
/// safety-net completion notification when the Claude process exits.
///
/// `messages_before` is the bot_message row count for this task recorded just
/// before the inject was started.  After the process exits we wait to see if
/// a new row appears (count increases), which means the Stop hook already sent
/// the completion notification.  Using a count rather than a timestamp avoids
/// clock-skew and SQLite WAL snapshot issues that caused duplicate notifications.
fn inject_with_heartbeat(
    task: &crate::models::Task,
    text: &str,
    config: &TelegramConfig,
    task_id: &str,
    db_path: &std::path::Path,
    messages_before: i64,
) {
    // Spawn the Claude process.
    let mut child = match agent_input::inject_returning_child(task, text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[listen] inject failed: {e:#}");
            let _ = telegram::send(config, &format!("❌ Could not start agent turn: {e}"));
            return;
        }
    };

    let start = Instant::now();
    let short_id = &task_id[..task_id.len().min(8)];

    // Poll the child process, sending heartbeats every HEARTBEAT_INTERVAL.
    let mut last_heartbeat = Instant::now();
    let mut exit_status: Option<std::process::ExitStatus> = None;
    loop {
        // Check if process has finished (non-blocking).
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_status = Some(status);
                break;
            } // done
            Ok(None) => {} // still running
            Err(e) => {
                eprintln!("[listen] inject wait error: {e:#}");
                break;
            }
        }

        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            let elapsed_min = start.elapsed().as_secs() / 60;
            let msg = format!("⏳ Agent still working… ({elapsed_min}m elapsed)");
            eprintln!("[listen] heartbeat for task {short_id}: {msg}");
            let _ = telegram::send(config, &msg);
            last_heartbeat = Instant::now();
        }

        thread::sleep(Duration::from_secs(5));
    }

    let elapsed = start.elapsed();
    let exit_code = exit_status.and_then(|s| s.code());
    let success = exit_status.map_or(false, |s| s.success());
    eprintln!(
        "[listen] inject done for task {short_id} in {}s, exit={:?}",
        elapsed.as_secs(),
        exit_code
    );

    // Poll until the Stop hook records its bot_message row, or until the timeout
    // expires.  Polling every second means we fire the safety-net within ~1s of
    // the hook completing in the happy path, instead of always waiting a fixed delay.
    let db = match Database::open(db_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[listen] safety-net: could not open DB: {e:#}");
            return;
        }
    };

    let mut hook_notified = false;
    for i in 0..STOP_HOOK_TIMEOUT_SECS {
        thread::sleep(Duration::from_secs(1));
        let count_now = db
            .bot_message_count_for_task(task_id)
            .unwrap_or(messages_before);
        if count_now > messages_before {
            hook_notified = true;
            eprintln!(
                "[listen] Stop hook notified after {}s, suppressing safety-net",
                i + 1
            );
            break;
        }
    }

    if !hook_notified {
        // Stop hook didn't send a notification — send a fallback notification.
        eprintln!("[listen] safety-net: Stop hook didn't notify for {short_id}, sending fallback");
        let elapsed_str = if elapsed.as_secs() < 60 {
            format!("{}s", elapsed.as_secs())
        } else {
            format!("{}m {}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
        };

        // If the process exited with an error, report that instead of "complete".
        let msg = if !success {
            if let Some(code) = exit_code {
                format!("❌ Agent exited with error (code {code}) after {elapsed_str}")
            } else {
                format!("❌ Agent exited with error after {elapsed_str}")
            }
        } else {
            format!("✅ Agent turn complete ({elapsed_str})")
        };

        if let Ok(text) = crate::build_notification_text(&db, Some(task_id), &msg, false) {
            let _ = telegram::send_with_reply_button(config, &text, task_id)
                .map(|msg_id| db.insert_bot_message(msg_id, task_id));
        }
    }
}

// ── Telegram API ──────────────────────────────────────────────────────────────

/// Call getUpdates with long-polling.  Returns an empty vec on timeout.
fn get_updates(config: &TelegramConfig, offset: i64) -> Result<Vec<serde_json::Value>> {
    let url = format!(
        "https://api.telegram.org/bot{}/getUpdates",
        config.bot_token
    );

    let payload = serde_json::json!({
        "offset": offset,
        "timeout": POLL_TIMEOUT_SECS,
        "allowed_updates": ["message", "callback_query"],
    });

    // Use separate connect and read timeouts.  The long-poll keeps the TCP
    // connection open for up to POLL_TIMEOUT_SECS waiting for updates, so the
    // read timeout must be longer than that.  A single combined timeout of
    // POLL_TIMEOUT_SECS+10 was too tight and fired before Telegram responded.
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(POLL_TIMEOUT_SECS + 15))
        .timeout_write(Duration::from_secs(10))
        .build();

    let response = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .send_json(&payload)
        .context("getUpdates HTTP request failed")?;

    if response.status() != 200 {
        let status = response.status();
        let body = response.into_string().unwrap_or_default();
        anyhow::bail!("getUpdates returned {}: {}", status, body);
    }

    let json: serde_json::Value = response
        .into_json()
        .context("Failed to parse getUpdates response")?;

    let updates = json
        .get("result")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    Ok(updates)
}

/// Send a short notice message to the user (no reply button, no keyboard).
fn send_notice(config: &TelegramConfig, chat_id: i64, text: &str) -> Result<()> {
    let _ = send_notice_returning_id(config, chat_id, text)?;
    Ok(())
}

fn send_notice_returning_id(config: &TelegramConfig, chat_id: i64, text: &str) -> Result<i64> {
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessage",
        config.bot_token
    );
    let payload = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
    });
    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(&payload)
        .context("send_notice HTTP request failed")?;

    let json: serde_json::Value = response
        .into_json()
        .context("Failed to parse send_notice response")?;

    let msg_id = json
        .pointer("/result/message_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    Ok(msg_id)
}

// ── Auth ──────────────────────────────────────────────────────────────────────

/// Return true only when the sender passes every configured whitelist check.
///
/// Two independent checks are applied:
///
/// 1. **Numeric chat_id** — the sender's `from.id` must equal `config.chat_id`.
///    This is always enforced because `chat_id` is required for the bot to work.
///
/// 2. **Username** — when `config.allowed_username` is non-empty the sender's
///    `from.username` must match it (case-insensitive, leading `@` stripped).
///    Both checks must pass; either one failing is sufficient to reject.
///
/// Using two independent factors (something you are: numeric ID, something you
/// chose: username) means an attacker who discovers the bot token still cannot
/// interact with your machine unless they also control your Telegram account.
fn is_authorised(config: &TelegramConfig, from_id: i64, from_username: Option<&str>) -> bool {
    // Check 1: numeric chat_id must match.
    let id_ok = config
        .chat_id
        .parse::<i64>()
        .map(|id| id == from_id)
        .unwrap_or(false);

    if !id_ok {
        return false;
    }

    // Check 2: username must match when one is configured.
    if !config.allowed_username.is_empty() {
        let allowed = config
            .allowed_username
            .trim_start_matches('@')
            .to_lowercase();
        let sender = from_username
            .unwrap_or("")
            .trim_start_matches('@')
            .to_lowercase();
        if sender != allowed {
            eprintln!(
                "[listen] Rejecting message from id={from_id}: \
                 username @{sender} is not in the allowed list"
            );
            return false;
        }
    }

    true
}

// ── Cron job execution ────────────────────────────────────────────────────────

/// Find a healthy sandbox for `repo_path`, or spawn a new one.
/// Returns the Task to inject into.
fn find_or_spawn_for_cron(
    db: &Database,
    repo_path: &str,
    config: &TelegramConfig,
) -> Result<crate::models::Task> {
    let sandbox = PodmanSandbox::new();

    // Walk all containers for this repo_path (newest first) and return the first healthy one.
    for (task_id, container_name) in db.get_all_containers_by_repo_path(repo_path)? {
        let Some(task) = db.get_task_by_id(&task_id)? else {
            continue;
        };
        match sandbox.health_check(&container_name) {
            SandboxHealth::Healthy => return Ok(task),
            SandboxHealth::Stopped => {
                eprintln!("[cron] Container {container_name} stopped → restarting for cron");
                match sandbox.start(&container_name) {
                    Ok(()) => {
                        if sandbox.health_check(&container_name) == SandboxHealth::Healthy {
                            return Ok(task);
                        }
                        eprintln!("[cron] Container {container_name} not healthy after start, trying next");
                    }
                    Err(e) => {
                        eprintln!("[cron] Failed to restart {container_name}: {e:#}, trying next");
                    }
                }
            }
            status => {
                eprintln!(
                    "[cron] Container {container_name} for {repo_path} is {status:?}, trying next"
                );
            }
        }
    }

    // No healthy container — spawn one.
    eprintln!("[cron] Spawning sandbox for {repo_path}");
    let repo_label = std::path::Path::new(repo_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(repo_path);
    let _ = telegram::send(
        config,
        &format!("⚙️ Spawning sandbox for '{repo_label}' (cron trigger)…"),
    );

    let cfg = crate::config::load().unwrap_or_default();
    let new_task_id = crate::cmd_sandbox_spawn(
        db,
        repo_path.to_string(),
        None,
        "nibble-sandbox:latest".to_string(),
        false,
        None,
        true,
        false,
        cfg.factory.enabled,
        false,
        false,
    )?;

    db.get_task_by_id(&new_task_id)?
        .ok_or_else(|| anyhow::anyhow!("[cron] Spawned task {new_task_id} not found in DB"))
}

/// Check for due cron jobs and execute them.
fn check_and_run_cron_jobs(db: &Database, config: &TelegramConfig) -> Result<()> {
    let now = chrono::Utc::now();
    let due_jobs = db.get_due_cron_jobs(now)?;

    for job in due_jobs {
        let job_id = job.id.unwrap_or(0);
        let label = job.label.clone().unwrap_or_else(|| "unnamed".to_string());
        let repo_label = std::path::Path::new(&job.repo_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&job.repo_path)
            .to_string();
        eprintln!(
            "[cron] Job {} ('{}') due for repo {}",
            job_id, label, repo_label
        );

        // Auto-disable if the job has expired.
        if let Some(exp) = job.expires_at {
            if now >= exp {
                eprintln!("[cron] Job {job_id} ('{label}') expired at {exp}, disabling");
                let mut expired_job = job.clone();
                expired_job.enabled = false;
                let _ = db.update_cron_job(&expired_job);
                let _ = telegram::send(
                    config,
                    &format!("⏹️ Cron job '{label}' expired and has been disabled."),
                );
                continue;
            }
        }

        // Skip if a previous execution is still in-flight.
        if job.skip_if_running && job.running {
            let _ = telegram::send(
                config,
                &format!("⏭️ Cron job '{label}' skipped: previous run still in progress"),
            );
            eprintln!("[cron] Job {job_id} skipped (running flag set)");
            continue;
        }

        // Mark running + advance timestamps before spawning the thread so that
        // the next cron tick sees running=true and skips accordingly.
        let mut updated_job = job.clone();
        updated_job.running = true;
        updated_job.last_run = Some(now);
        match cron::compute_next_run(&job.schedule, now) {
            Ok(next) => updated_job.next_run = next,
            Err(e) => {
                eprintln!("[cron] Failed to compute next run for job {job_id}: {e}");
                updated_job.enabled = false;
            }
        }
        let _ = db.update_cron_job(&updated_job);

        let prompt_preview = if job.prompt.chars().count() > 200 {
            format!("{}…", job.prompt.chars().take(200).collect::<String>())
        } else {
            job.prompt.clone()
        };
        let _ = telegram::send(
            config,
            &format!(
            "🕐 Cron job '{label}' starting\n📁 Repo: {repo_label}\n📝 Prompt: {prompt_preview}"
        ),
        );

        // Dispatch to background thread: find-or-spawn sandbox, then inject.
        let db_path = crate::db::default_db_path();
        let config_clone = config.clone();
        let prompt_clone = job.prompt.clone();
        let repo_path_clone = job.repo_path.clone();

        thread::spawn(move || {
            // Open a fresh DB connection for the thread.
            let db = match Database::open(&db_path) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[cron] Thread DB open failed: {e}");
                    return;
                }
            };

            let task = match find_or_spawn_for_cron(&db, &repo_path_clone, &config_clone) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("[cron] Job {job_id} failed to find/spawn sandbox: {e:#}");
                    let _ = telegram::send(
                        &config_clone,
                        &format!("❌ Cron job '{label}' failed: {e:#}"),
                    );
                    let _ = db.set_cron_job_running(job_id, false);
                    return;
                }
            };

            // Read messages_before inside the thread (task is now known).
            let messages_before = db.bot_message_count_for_task(&task.task_id).unwrap_or(0);
            let task_id_clone = task.task_id.clone();

            inject_with_heartbeat(
                &task,
                &prompt_clone,
                &config_clone,
                &task_id_clone,
                &db_path,
                messages_before,
            );

            // Clear the running flag when the injection finishes.
            if let Ok(db2) = Database::open(&db_path) {
                let _ = db2.set_cron_job_running(job_id, false);
            }
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TelegramConfig;

    fn cfg(chat_id: &str, allowed_username: &str) -> TelegramConfig {
        TelegramConfig {
            enabled: true,
            bot_token: "tok".to_string(),
            chat_id: chat_id.to_string(),
            allowed_username: allowed_username.to_string(),
        }
    }

    #[test]
    fn test_correct_id_no_username_required() {
        assert!(is_authorised(&cfg("123", ""), 123, Some("anyuser")));
    }

    #[test]
    fn test_wrong_id_rejected() {
        assert!(!is_authorised(&cfg("123", ""), 999, Some("anyuser")));
    }

    #[test]
    fn test_correct_id_and_username() {
        assert!(is_authorised(
            &cfg("123", "adlrocha"),
            123,
            Some("adlrocha")
        ));
    }

    #[test]
    fn test_correct_id_wrong_username_rejected() {
        assert!(!is_authorised(
            &cfg("123", "adlrocha"),
            123,
            Some("attacker")
        ));
    }

    #[test]
    fn test_at_prefix_stripped_from_config() {
        // Stored as "@adlrocha" in config but should still match sender "adlrocha"
        assert!(is_authorised(
            &cfg("123", "@adlrocha"),
            123,
            Some("adlrocha")
        ));
    }

    #[test]
    fn test_at_prefix_stripped_from_sender() {
        assert!(is_authorised(
            &cfg("123", "adlrocha"),
            123,
            Some("@adlrocha")
        ));
    }

    #[test]
    fn test_username_check_case_insensitive() {
        assert!(is_authorised(
            &cfg("123", "AdlRocha"),
            123,
            Some("adlrocha")
        ));
    }

    #[test]
    fn test_no_username_in_message_with_required_username() {
        // Sender has no username set — must be rejected when a username is required
        assert!(!is_authorised(&cfg("123", "adlrocha"), 123, None));
    }
}
