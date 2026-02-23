//! Telegram Bot API notification sender.

use crate::config::TelegramConfig;
use anyhow::{Context, Result};

/// Maximum message length enforced by the Telegram Bot API.
const TELEGRAM_MAX_LEN: usize = 4096;

/// Truncation notice prepended when a message is cut.
const TRUNCATION_NOTICE: &str = "[...truncated, showing last 4096 chars]\n\n";

/// Send a plain-text message to a Telegram chat.
///
/// If `text` exceeds `TELEGRAM_MAX_LEN` characters the message is truncated to
/// the *last* `TELEGRAM_MAX_LEN` characters so the most recent output is
/// preserved (a truncation notice is prepended).
pub fn send(config: &TelegramConfig, text: &str) -> Result<()> {
    let body = truncate(text);
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessage",
        config.bot_token
    );

    let payload = serde_json::json!({
        "chat_id": config.chat_id,
        "text": body,
        "parse_mode": "HTML",
        "disable_web_page_preview": true,
    });

    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(&payload)
        .context("HTTP request to Telegram API failed")?;

    if response.status() != 200 {
        let status = response.status();
        let body = response.into_string().unwrap_or_default();
        anyhow::bail!("Telegram API returned {}: {}", status, body);
    }

    Ok(())
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
