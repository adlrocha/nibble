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

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::agent_input;
use crate::config::TelegramConfig;
use crate::db::Database;
use crate::notifications::telegram;

const POLL_TIMEOUT_SECS: u64 = 30;
const POLL_OFFSET_KEY: &str = "telegram_poll_offset";
const PENDING_REPLY_PREFIX: &str = "pending_reply:";

// ── Public entry point ────────────────────────────────────────────────────────

/// Run the long-polling daemon indefinitely.  Call from `main` for the
/// `Commands::Listen` variant.
pub fn run(db: &Database, config: &TelegramConfig) -> Result<()> {
    eprintln!("[listen] Starting Telegram long-polling daemon…");

    let mut offset: i64 = db
        .kv_get(POLL_OFFSET_KEY)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

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
    match agent_input::inject(&task, text) {
        Ok(()) => {
            eprintln!("[listen] Injection succeeded");
            send_notice(config, chat_id, "📨 Message sent to agent.")?;
        }
        Err(e) => {
            eprintln!("[listen] Injection FAILED: {e:#}");
            send_notice(config, chat_id, &format!("❌ Could not inject: {e}"))?;
        }
    }
    Ok(())
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
