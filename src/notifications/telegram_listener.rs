//! Telegram long-polling listener daemon.
//!
//! Runs as a blocking loop (`agent-inbox listen`), calling `getUpdates` with a
//! 30-second long-poll timeout.  For each incoming update it:
//!
//! - Verifies the sender is the authorised user (chat_id whitelist).
//! - Routes **callback queries** (inline button taps):
//!   - `reply:{task_id}` → records a pending-reply state and prompts the user
//!     to type their message.
//! - Routes **text messages**:
//!   - If the message is a reply to a known bot message → injects into the
//!     associated task via `claude --resume`.
//!   - If there is a pending-reply state for this chat → injects into the
//!     pending task and clears the state.
//!   - Otherwise → ignored (with a polite notice).
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
use crate::db::Database;
use crate::notifications::telegram;

const POLL_TIMEOUT_SECS: u64 = 30;
const POLL_OFFSET_KEY: &str = "telegram_poll_offset";
const PENDING_REPLY_PREFIX: &str = "pending_reply:";
/// Run a prune pass every this many polling loops (~5 minutes at 30s/poll).
const PRUNE_EVERY_N_POLLS: u32 = 10;
/// Send a "still working" heartbeat this often during a long inject turn.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(120);

// ── Public entry point ────────────────────────────────────────────────────────

/// Run the long-polling daemon indefinitely.  Call from `main` for the
/// `Commands::Listen` variant.
pub fn run(db: &Database, config: &TelegramConfig) -> Result<()> {
    eprintln!("[listen] Starting Telegram long-polling daemon…");

    let mut offset: i64 = db
        .kv_get(POLL_OFFSET_KEY)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut poll_count: u32 = 0;

    loop {
        let updates = match get_updates(config, offset) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("[listen] getUpdates error: {e}. Retrying in 5s…");
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        for update in &updates {
            let update_id = update
                .get("update_id")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            offset = offset.max(update_id + 1);

            eprintln!("[listen] Processing update_id={update_id}");
            if let Err(e) = handle_update(update, config, db) {
                eprintln!("[listen] Error handling update {update_id}: {e:#}");
            }
        }

        // Persist offset after every batch (even if empty) to survive restarts.
        let _ = db.kv_set(POLL_OFFSET_KEY, &offset.to_string());

        // Periodically prune stale tasks (dead PIDs / gone containers).
        poll_count += 1;
        if poll_count % PRUNE_EVERY_N_POLLS == 0 {
            if let Err(e) = crate::prune_stale_tasks(db) {
                eprintln!("[listen] prune error: {e:#}");
            }
        }
    }
}

// ── Update dispatch ───────────────────────────────────────────────────────────

fn handle_update(
    update: &serde_json::Value,
    config: &TelegramConfig,
    db: &Database,
) -> Result<()> {
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

    let from_id = cq
        .pointer("/from/id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
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

    let chat_id = cq
        .pointer("/message/chat/id")
        .and_then(|v| v.as_i64())
        .unwrap_or(from_id);

    if let Some(task_id) = data.strip_prefix("reply:") {
        // Persist pending-reply state in the DB so it survives daemon restarts.
        let key = format!("{}{}", PENDING_REPLY_PREFIX, chat_id);
        let _ = db.kv_set(&key, task_id);
        let _ = telegram::send_reply(
            config,
            "✏️ Type your reply and send it:",
            cq.pointer("/message/message_id")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
        );
    }

    Ok(())
}

// ── Text message handler ──────────────────────────────────────────────────────

fn handle_message(
    msg: &serde_json::Value,
    config: &TelegramConfig,
    db: &Database,
) -> Result<()> {
    // Ignore messages sent by the bot itself.
    if msg.pointer("/from/is_bot").and_then(|v| v.as_bool()).unwrap_or(false) {
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

    eprintln!("[listen] Text message from {from_id}: {:?}", &text[..text.len().min(80)]);

    // Priority 0a: /sandboxes command — list running sandboxes with reply buttons.
    if text.trim() == "/sandboxes" {
        return handle_sandboxes_command(config, db, chat_id);
    }

    // Priority 0b: /spawn <repo_path> [task description]
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
            let desc = parts.next().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
            (path.map(str::to_string), desc)
        };
        return handle_spawn_command(config, db, chat_id, repo_path, task_desc);
    }

    // Priority 1: pending-reply state persisted in DB (survives daemon restarts).
    let pending_key = format!("{}{}", PENDING_REPLY_PREFIX, chat_id);
    if let Ok(Some(task_id)) = db.kv_get(&pending_key) {
        if !task_id.is_empty() {
            eprintln!("[listen] Routing to pending task {task_id}");
            let _ = db.kv_delete(&pending_key);
            return route_text_to_task(&task_id, &text, config, db, chat_id);
        }
        let _ = db.kv_delete(&pending_key);
    }

    // Priority 2: direct reply to a known bot message.
    if let Some(reply_to) = msg.get("reply_to_message") {
        if let Some(orig_id) = reply_to.get("message_id").and_then(|v| v.as_i64()) {
            eprintln!("[listen] Direct reply to message_id={orig_id}");
            if let Some(task_id) = db.get_task_id_by_message_id(orig_id)? {
                return route_text_to_task(&task_id, &text, config, db, chat_id);
            }
        }
    }

    eprintln!("[listen] No routing found, sending hint");
    // Unknown message — send a hint.
    send_notice(
        config,
        chat_id,
        "ℹ️ Reply to a task notification to send a message to that agent.",
    )?;

    Ok(())
}

fn handle_sandboxes_command(
    config: &TelegramConfig,
    db: &Database,
    _chat_id: i64,
) -> Result<()> {
    use crate::sandbox::podman::PodmanSandbox;
    use crate::sandbox::{ContainerStatus, Sandbox};

    let sandbox = PodmanSandbox::new();
    let states = db.list_container_states()?;

    // Ask Podman directly — DB task status is unreliable because sandbox tasks
    // flip between Running/Completed on every turn.
    let mut running: Vec<(String, String)> = Vec::new(); // (task_id, label)
    for (task_id, container_name, repo_path, _created) in &states {
        if let Ok(ContainerStatus::Running) = sandbox.status(container_name) {
            // Use the last component of the repo path as the display label.
            let label = std::path::Path::new(repo_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(repo_path.as_str())
                .to_string();
            running.push((task_id.clone(), label));
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
            "agent-inbox-sandbox:latest".to_string(),
            false, // fresh
            None,  // session_id — auto-detect
        ) {
            Ok(task_id) => {
                let short_id = &task_id[..task_id.len().min(8)];
                let msg = format!(
                    "✅ Sandbox started for `{repo_label}`\nTask ID: `{short_id}`\n\nUse `/sandboxes` to interact."
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
            send_notice(config, chat_id, "⚠️ Task not found.")?;
            return Ok(());
        }
    };

    eprintln!("[listen] Injecting into task {task_id}, pid={:?}", task.pid);

    // Acknowledge immediately so the user knows their message was received.
    send_notice(config, chat_id, "📨 Message sent to agent.")?;

    // Spawn the actual inject in a background thread so the listener loop stays
    // live and can process other Telegram updates while Claude is working.
    let config_clone = config.clone();
    let task_id_owned = task_id.to_string();
    let text_owned = text.to_string();
    let db_path = crate::db::default_db_path();

    thread::spawn(move || {
        inject_with_heartbeat(&task, &text_owned, &config_clone, &task_id_owned, &db_path);
    });

    Ok(())
}

/// How long to wait after the inject process exits before checking whether the
/// Stop hook has already sent a notification.  The Stop hook runs as a separate
/// process launched by Claude Code immediately after it finishes — giving it a
/// few seconds to write its bot_message row prevents a false safety-net fire.
const STOP_HOOK_GRACE_SECS: u64 = 15;

/// Run inject inside a background thread, sending periodic heartbeats and a
/// safety-net completion notification when the Claude process exits.
fn inject_with_heartbeat(
    task: &crate::models::Task,
    text: &str,
    config: &TelegramConfig,
    task_id: &str,
    db_path: &std::path::Path,
) {
    // Record the Unix timestamp just before we start so that we can later ask
    // "did a bot_message get inserted for this task after we started?"  That is
    // a more reliable proxy for "did the Stop hook send the notification" than
    // comparing attention_reason, which may not be updated on every turn.
    let turn_start_unix = chrono::Utc::now().timestamp();

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
    loop {
        // Check if process has finished (non-blocking).
        match child.try_wait() {
            Ok(Some(_status)) => break, // done
            Ok(None) => {}              // still running
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
    eprintln!("[listen] inject done for task {short_id} in {}s", elapsed.as_secs());

    // Give the Stop hook time to run and record its bot_message before we check.
    // The hook is a separate process spawned by Claude Code right after it exits,
    // so the inject child may exit slightly before the hook completes.
    eprintln!("[listen] waiting {STOP_HOOK_GRACE_SECS}s for Stop hook…");
    thread::sleep(Duration::from_secs(STOP_HOOK_GRACE_SECS));

    // Safety-net: check whether the Stop hook already sent a Telegram message
    // for this task since the turn started.  If it did, suppress the fallback
    // so the user only gets one notification.  If it didn't (hook timed out,
    // container crashed, or network error), send the fallback ourselves.
    let db = match Database::open(db_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[listen] safety-net: could not open DB: {e:#}");
            return;
        }
    };

    let hook_notified = db
        .has_bot_message_since(task_id, turn_start_unix)
        .unwrap_or(false);

    if !hook_notified {
        // Stop hook didn't send a notification — send a generic completion ping.
        eprintln!("[listen] safety-net: Stop hook didn't notify for {short_id}, sending fallback");
        let elapsed_str = if elapsed.as_secs() < 60 {
            format!("{}s", elapsed.as_secs())
        } else {
            format!("{}m {}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
        };
        let msg = format!("✅ Agent turn complete ({elapsed_str})");
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

    // Use a slightly longer HTTP timeout than the long-poll timeout.
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(POLL_TIMEOUT_SECS + 10))
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
    // Build a minimal payload targeting the specific chat_id (which may differ
    // from config.chat_id if the bot is in a group, but for personal bots they
    // are the same).
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessage",
        config.bot_token
    );
    let payload = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
    });
    let _ = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(&payload);
    Ok(())
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
        let allowed = config.allowed_username.trim_start_matches('@').to_lowercase();
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
        assert!(is_authorised(&cfg("123", "adlrocha"), 123, Some("adlrocha")));
    }

    #[test]
    fn test_correct_id_wrong_username_rejected() {
        assert!(!is_authorised(&cfg("123", "adlrocha"), 123, Some("attacker")));
    }

    #[test]
    fn test_at_prefix_stripped_from_config() {
        // Stored as "@adlrocha" in config but should still match sender "adlrocha"
        assert!(is_authorised(&cfg("123", "@adlrocha"), 123, Some("adlrocha")));
    }

    #[test]
    fn test_at_prefix_stripped_from_sender() {
        assert!(is_authorised(&cfg("123", "adlrocha"), 123, Some("@adlrocha")));
    }

    #[test]
    fn test_username_check_case_insensitive() {
        assert!(is_authorised(&cfg("123", "AdlRocha"), 123, Some("adlrocha")));
    }

    #[test]
    fn test_no_username_in_message_with_required_username() {
        // Sender has no username set — must be rejected when a username is required
        assert!(!is_authorised(&cfg("123", "adlrocha"), 123, None));
    }
}
