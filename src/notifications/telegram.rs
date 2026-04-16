//! Telegram Bot API notification sender.

use crate::config::TelegramConfig;
use anyhow::{Context, Result};

/// Maximum message length enforced by the Telegram Bot API.
const TELEGRAM_MAX_LEN: usize = 4096;

/// Send a plain-text message and return the Telegram message_id of the last chunk.
///
/// If the text exceeds the Telegram limit it is split into successive messages.
/// The message_id of the final message is returned.
pub fn send(config: &TelegramConfig, text: &str) -> Result<i64> {
    let chunks = annotate_chunks(split_chunks(text));
    let last = chunks.len().saturating_sub(1);
    let mut final_id = 0i64;
    for (i, chunk) in chunks.iter().enumerate() {
        let payload = serde_json::json!({
            "chat_id": config.chat_id,
            "text": chunk,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        let id = post_message(config, &payload)?;
        if i == last {
            final_id = id;
        }
    }
    Ok(final_id)
}

/// Send a message with an inline "↩ Reply" button on the last chunk.
///
/// Long messages are split into successive plain messages with the reply button
/// only attached to the final one.
pub fn send_with_reply_button(config: &TelegramConfig, text: &str, task_id: &str) -> Result<i64> {
    let chunks = annotate_chunks(split_chunks(text));
    let last = chunks.len().saturating_sub(1);
    let mut final_id = 0i64;
    for (i, chunk) in chunks.iter().enumerate() {
        let payload = if i == last {
            serde_json::json!({
                "chat_id": config.chat_id,
                "text": chunk,
                "parse_mode": "HTML",
                "disable_web_page_preview": true,
                "reply_markup": {
                    "inline_keyboard": [[
                        {"text": "↩ Reply", "callback_data": format!("reply:{}", task_id)}
                    ]]
                }
            })
        } else {
            serde_json::json!({
                "chat_id": config.chat_id,
                "text": chunk,
                "parse_mode": "HTML",
                "disable_web_page_preview": true,
            })
        };
        let id = post_message(config, &payload)?;
        if i == last {
            final_id = id;
        }
    }
    Ok(final_id)
}

/// Send a ForceReply prompt replying to `reply_to_message_id`.
///
/// Returns the message_id of the sent prompt so it can be stored in
/// bot_messages for explicit reply gesture routing.
pub fn send_reply(config: &TelegramConfig, text: &str, reply_to_message_id: i64) -> Result<i64> {
    let payload = serde_json::json!({
        "chat_id": config.chat_id,
        "text": text,
        "reply_to_message_id": reply_to_message_id,
        "reply_markup": { "force_reply": true, "selective": true },
    });
    post_message(config, &payload)
}

pub fn send_sandbox_list(
    config: &TelegramConfig,
    sandboxes: &[(&str, &str)], // (task_id, repo_label)
) -> Result<i64> {
    if sandboxes.is_empty() {
        return send(config, "🤖 No running sandboxes.");
    }

    let rows: Vec<Vec<serde_json::Value>> = sandboxes
        .iter()
        .map(|(task_id, repo)| {
            vec![serde_json::json!({
                "text": format!("↩ {}", repo),
                "callback_data": format!("reply:{}", task_id),
            })]
        })
        .collect();

    let lines: Vec<String> = sandboxes
        .iter()
        .map(|(task_id, repo)| {
            format!(
                "• <b>{repo}</b>  <code>{}</code>",
                &task_id[..task_id.len().min(8)]
            )
        })
        .collect();

    let text = format!(
        "🤖 <b>Running sandboxes</b>\n\n{}\n\nTap ↩ next to a sandbox, then type your message.",
        lines.join("\n")
    );

    let payload = serde_json::json!({
        "chat_id": config.chat_id,
        "text": text,
        "parse_mode": "HTML",
        "disable_web_page_preview": true,
        "reply_markup": { "inline_keyboard": rows },
    });
    post_message(config, &payload)
}

/// Acknowledge a callback query (removes the button-loading spinner on the client).
///
/// Must be called after every callback_query update, even when taking no visible action.
pub fn answer_callback_query(config: &TelegramConfig, callback_query_id: &str) -> Result<()> {
    answer_callback_query_with_text(config, callback_query_id, "")
}

/// Acknowledge a callback query and show a toast notification to the user.
pub fn answer_callback_query_with_text(
    config: &TelegramConfig,
    callback_query_id: &str,
    text: &str,
) -> Result<()> {
    let url = format!(
        "https://api.telegram.org/bot{}/answerCallbackQuery",
        config.bot_token
    );
    let mut payload = serde_json::json!({"callback_query_id": callback_query_id});
    if !text.is_empty() {
        payload["text"] = serde_json::Value::String(text.to_string());
        payload["show_alert"] = serde_json::Value::Bool(false);
    }
    ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(&payload)
        .context("answerCallbackQuery HTTP request failed")?;
    Ok(())
}

/// Register the bot's command menu with Telegram (call once at daemon startup).
///
/// This makes `/help`, `/sandboxes`, `/cron`, and `/spawn` appear in the
/// Telegram command picker when the user types `/` in the chat.
pub fn register_commands(config: &TelegramConfig) -> Result<()> {
    let url = format!(
        "https://api.telegram.org/bot{}/setMyCommands",
        config.bot_token
    );
    let payload = serde_json::json!({
        "commands": [
            { "command": "sandboxes", "description": "List running sandboxes and send a message" },
            { "command": "spawn",     "description": "Spawn a new sandbox: /spawn /path/to/repo [task]" },
            { "command": "cron",      "description": "List scheduled cron jobs: /cron list [repo]" },
            { "command": "help",      "description": "Show available commands" },
        ]
    });
    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(&payload)
        .context("setMyCommands HTTP request failed")?;
    if response.status() != 200 {
        let status = response.status();
        let body = response.into_string().unwrap_or_default();
        anyhow::bail!("setMyCommands returned {}: {}", status, body);
    }
    Ok(())
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

/// Prepend a `(n/total)` counter to each chunk when there is more than one.
///
/// Single-chunk messages are returned unchanged so the counter never appears
/// for short responses.
fn annotate_chunks(chunks: Vec<String>) -> Vec<String> {
    let total = chunks.len();
    if total <= 1 {
        return chunks;
    }
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| format!("({}/{})\n{}", i + 1, total, chunk))
        .collect()
}

/// Split `text` into chunks of at most `TELEGRAM_MAX_LEN` characters,
/// keeping each chunk as close to the limit as possible.
///
/// Strategy: iterate char by char, splitting at the last newline before the
/// limit whenever possible, otherwise splitting hard at the limit.
fn split_chunks(text: &str) -> Vec<String> {
    if text.chars().count() <= TELEGRAM_MAX_LEN {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        let char_count = remaining.chars().count();
        if char_count <= TELEGRAM_MAX_LEN {
            chunks.push(remaining.to_string());
            break;
        }

        // Find the byte offset of the TELEGRAM_MAX_LEN-th character.
        let limit_byte = remaining
            .char_indices()
            .nth(TELEGRAM_MAX_LEN)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());

        let candidate = &remaining[..limit_byte];

        // Prefer splitting at the last newline within the candidate window.
        let split_byte = candidate.rfind('\n').map(|i| i + 1).unwrap_or(limit_byte);

        chunks.push(remaining[..split_byte].to_string());
        remaining = &remaining[split_byte..];
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_message_single_chunk() {
        let msg = "Hello, world!";
        let chunks = split_chunks(msg);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], msg);
    }

    #[test]
    fn test_message_at_limit_single_chunk() {
        let msg = "x".repeat(TELEGRAM_MAX_LEN);
        let chunks = split_chunks(&msg);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_long_message_splits_into_multiple_chunks() {
        let msg = "x".repeat(TELEGRAM_MAX_LEN + 100);
        let chunks = split_chunks(&msg);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= TELEGRAM_MAX_LEN);
        }
    }

    #[test]
    fn test_split_preserves_all_content() {
        let msg = "line\n".repeat(2000); // ~10000 chars
        let chunks = split_chunks(&msg);
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, msg);
    }

    #[test]
    fn test_split_respects_line_boundaries() {
        // Each line is 100 chars + newline; 40 lines = 4040 chars > limit.
        // The split should happen at a newline so intermediate chunks end with '\n'.
        let line = format!("{}\n", "a".repeat(100));
        let msg = line.repeat(40);
        let chunks = split_chunks(&msg);
        for chunk in &chunks[..chunks.len().saturating_sub(1)] {
            assert!(
                chunk.ends_with('\n'),
                "intermediate chunk should end at newline boundary"
            );
        }
    }

    #[test]
    fn test_chunks_are_near_full() {
        // 4000 lines of 1 char each — chunks should be close to 4096, not tiny.
        let msg = "x\n".repeat(4000); // 8000 chars total
        let chunks = split_chunks(&msg);
        // Each intermediate chunk should use at least 90% of the limit.
        for chunk in &chunks[..chunks.len().saturating_sub(1)] {
            assert!(
                chunk.chars().count() >= TELEGRAM_MAX_LEN * 9 / 10,
                "chunk is too small: {} chars",
                chunk.chars().count()
            );
        }
    }

    #[test]
    fn test_single_line_exceeding_limit_is_hard_split() {
        let msg = "x".repeat(TELEGRAM_MAX_LEN * 2 + 500);
        let chunks = split_chunks(&msg);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= TELEGRAM_MAX_LEN);
        }
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, msg);
    }

    #[test]
    fn test_annotate_chunks_single_unchanged() {
        let chunks = annotate_chunks(vec!["hello".to_string()]);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn test_annotate_chunks_multiple_prefixed() {
        let chunks = annotate_chunks(vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_eq!(chunks[0], "(1/3)\na");
        assert_eq!(chunks[1], "(2/3)\nb");
        assert_eq!(chunks[2], "(3/3)\nc");
    }

    #[test]
    fn test_unicode_split() {
        let msg = "€".repeat(TELEGRAM_MAX_LEN + 50);
        let chunks = split_chunks(&msg);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= TELEGRAM_MAX_LEN);
        }
    }
}
