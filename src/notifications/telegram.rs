//! Telegram Bot API notification sender.

use crate::config::TelegramConfig;
use anyhow::{Context, Result};

/// Maximum message length enforced by the Telegram Bot API.
const TELEGRAM_MAX_LEN: usize = 4096;

/// Truncation notice prepended when a message is cut.
const TRUNCATION_NOTICE: &str = "[...truncated, showing last 4096 chars]\n\n";

/// Send a plain-text message and return the Telegram message_id.
///
/// The message_id can be stored so that the listener daemon can route user
/// replies back to the originating task.
pub fn send(config: &TelegramConfig, text: &str) -> Result<i64> {
    let body = truncate(text);
    let payload = serde_json::json!({
        "chat_id": config.chat_id,
        "text": body,
        "parse_mode": "HTML",
        "disable_web_page_preview": true,
    });
    post_message(config, &payload)
}

/// Send a message with an inline "↩ Reply" button attached.
///
/// Used for normal completion notifications so the user can tap Reply and have
/// their text injected into the running agent session.
pub fn send_with_reply_button(config: &TelegramConfig, text: &str, task_id: &str) -> Result<i64> {
    let body = truncate(text);
    let payload = serde_json::json!({
        "chat_id": config.chat_id,
        "text": body,
        "parse_mode": "HTML",
        "disable_web_page_preview": true,
        "reply_markup": {
            "inline_keyboard": [[
                {"text": "↩ Reply", "callback_data": format!("reply:{}", task_id)}
            ]]
        }
    });
    post_message(config, &payload)
}

/// Acknowledge a callback query (removes the button-loading spinner on the client).
///
/// Must be called after every callback_query update, even when taking no visible action.
pub fn answer_callback_query(config: &TelegramConfig, callback_query_id: &str) -> Result<()> {
    let url = format!(
        "https://api.telegram.org/bot{}/answerCallbackQuery",
        config.bot_token
    );
    let payload = serde_json::json!({"callback_query_id": callback_query_id});
    ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(&payload)
        .context("answerCallbackQuery HTTP request failed")?;
    Ok(())
}

/// Send a plain text reply to a specific message (for daemon → user prompts).
pub fn send_reply(config: &TelegramConfig, text: &str, reply_to_message_id: i64) -> Result<i64> {
    let payload = serde_json::json!({
        "chat_id": config.chat_id,
        "text": text,
        "reply_to_message_id": reply_to_message_id,
    });
    post_message(config, &payload)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// POST a sendMessage payload and return the resulting Telegram message_id.
fn post_message(config: &TelegramConfig, payload: &serde_json::Value) -> Result<i64> {
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessage",
        config.bot_token
    );

    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(payload)
        .context("HTTP request to Telegram API failed")?;

    if response.status() != 200 {
        let status = response.status();
        let body = response.into_string().unwrap_or_default();
        anyhow::bail!("Telegram API returned {}: {}", status, body);
    }

    let json: serde_json::Value = response
        .into_json()
        .context("Failed to parse Telegram API response")?;

    let message_id = json
        .pointer("/result/message_id")
        .and_then(|v| v.as_i64())
        .context("Telegram response missing result.message_id")?;

    Ok(message_id)
}

/// Truncate `text` to at most `TELEGRAM_MAX_LEN` UTF-8 characters.
///
/// When truncation is necessary the last `TELEGRAM_MAX_LEN` chars are kept and
/// a notice is prepended to signal that earlier content was dropped.
fn truncate(text: &str) -> String {
    let char_count = text.chars().count();
    if char_count <= TELEGRAM_MAX_LEN {
        return text.to_string();
    }

    // Keep the last TELEGRAM_MAX_LEN chars after reserving space for the notice.
    let notice_len = TRUNCATION_NOTICE.chars().count();
    let keep = TELEGRAM_MAX_LEN.saturating_sub(notice_len);

    // Find the byte offset for the last `keep` chars.
    let skip = char_count - keep;
    let tail: String = text.chars().skip(skip).collect();

    format!("{}{}", TRUNCATION_NOTICE, tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_message_unchanged() {
        let msg = "Hello, world!";
        assert_eq!(truncate(msg), msg);
    }

    #[test]
    fn test_message_at_limit_unchanged() {
        let msg = "x".repeat(TELEGRAM_MAX_LEN);
        let result = truncate(&msg);
        assert_eq!(result.chars().count(), TELEGRAM_MAX_LEN);
        assert!(!result.starts_with(TRUNCATION_NOTICE));
    }

    #[test]
    fn test_long_message_is_truncated() {
        let msg = "x".repeat(TELEGRAM_MAX_LEN + 100);
        let result = truncate(&msg);
        assert_eq!(result.chars().count(), TELEGRAM_MAX_LEN);
        assert!(result.starts_with(TRUNCATION_NOTICE));
    }

    #[test]
    fn test_truncation_keeps_tail() {
        // Build a message where we can identify which part is kept.
        let prefix = "A".repeat(5000);
        let suffix = "B".repeat(200);
        let msg = format!("{}{}", prefix, suffix);

        let result = truncate(&msg);
        assert_eq!(result.chars().count(), TELEGRAM_MAX_LEN);
        // The suffix should be fully present at the end.
        assert!(result.ends_with(&suffix));
    }

    #[test]
    fn test_unicode_message_truncation() {
        // Multi-byte chars: each '€' is 3 UTF-8 bytes but 1 char.
        let msg = "€".repeat(TELEGRAM_MAX_LEN + 50);
        let result = truncate(&msg);
        assert_eq!(result.chars().count(), TELEGRAM_MAX_LEN);
    }
}
