//! The agent's last reply, read from its session transcript.
//!
//! Both agent families write JSONL transcripts that nibble can already
//! locate for a task (host and sandbox sessions alike, since the agent
//! homes are bind-mounted into sandboxes):
//!
//! - Claude Code: `{"type":"assistant","uuid":…,"message":{"id":…,
//!   "content":[{"type":"text","text":…}, …]}}`; one API message can span
//!   several lines (thinking, text, tool use) sharing `message.id`.
//! - pi / omp: `{"type":"message","id":…,"message":{"role":"assistant",
//!   "content":[{"type":"text","text":…}]}}`.
//!
//! The reply is the last assistant text since the last real user prompt;
//! tool results (user-role lines in Claude, `toolResult` in pi) belong to
//! the same turn, but a tool call or result after a text means that text
//! is not the reply: the turn went on. The final message may not be on
//! disk yet (Claude Code can run the Stop hook that reports the turn idle
//! before flushing that message) or may never come (the turn really
//! ended on a tool result), so `None` does not always mean "not yet".

use anyhow::{Context, Result};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::models::AgentType;

/// How much of the transcript's tail is read; a reply is never further back.
const TAIL_BYTES: u64 = 2 * 1024 * 1024;

/// An assistant reply and the transcript id that identifies it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reply {
    pub id: String,
    pub text: String,
}

/// The last reply in the transcript at `path`.
pub(crate) fn last_reply(path: &Path, agent: &AgentType) -> Result<Option<Reply>> {
    let mut f =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let text = String::from_utf8_lossy(&buf);
    // A tail that starts mid-file starts mid-line: drop the partial line.
    let text = if start > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    Ok(last_reply_in(text, agent))
}

/// [`last_reply`] over transcript text.
pub(crate) fn last_reply_in(text: &str, agent: &AgentType) -> Option<Reply> {
    let lines = text
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok());
    match agent {
        AgentType::ClaudeCode => claude_reply(lines),
        AgentType::Pi | AgentType::Omp => pi_reply(lines),
        _ => None,
    }
}

fn texts(content: &Value) -> Vec<String> {
    match content {
        Value::String(s) if !s.trim().is_empty() => vec![s.clone()],
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// A Claude user line that is a real prompt, not a tool result.
fn is_claude_prompt(line: &Value) -> bool {
    match &line["message"]["content"] {
        Value::String(_) => true,
        Value::Array(blocks) => blocks.iter().any(|b| b["type"] != "tool_result"),
        _ => false,
    }
}

/// Whether a message's content calls a tool (Claude `tool_use`, pi `toolCall`).
fn calls_tool(content: &Value) -> bool {
    content.as_array().is_some_and(|blocks| {
        blocks
            .iter()
            .any(|b| matches!(b["type"].as_str(), Some("tool_use" | "toolCall")))
    })
}

fn claude_reply(lines_rev: impl Iterator<Item = Value>) -> Option<Reply> {
    // Walking backwards: find the newest assistant line with text, then
    // gather earlier text lines of the same API message. Tool activity
    // newer than any text means the final message is still to come.
    let mut found: Option<(String, Vec<String>)> = None;
    for line in lines_rev {
        match line["type"].as_str() {
            Some("assistant") if found.is_none() && calls_tool(&line["message"]["content"]) => {
                return None;
            }
            Some("user") if found.is_none() && !is_claude_prompt(&line) => return None,
            Some("assistant") => {
                let msg_id = line["message"]["id"]
                    .as_str()
                    .or_else(|| line["uuid"].as_str())
                    .unwrap_or_default()
                    .to_string();
                let t = texts(&line["message"]["content"]);
                match &mut found {
                    None if !t.is_empty() => found = Some((msg_id, t)),
                    Some((id, acc)) if *id == msg_id => {
                        let mut t = t;
                        t.append(acc);
                        *acc = t;
                    }
                    Some(_) => break,
                    None => {}
                }
            }
            Some("user") if is_claude_prompt(&line) => break,
            _ => {}
        }
    }
    found.map(|(id, parts)| Reply {
        id,
        text: parts.join("\n\n"),
    })
}

fn pi_reply(lines_rev: impl Iterator<Item = Value>) -> Option<Reply> {
    for line in lines_rev {
        if line["type"] != "message" {
            continue;
        }
        match line["message"]["role"].as_str() {
            Some("assistant") if calls_tool(&line["message"]["content"]) => return None,
            Some("toolResult") => return None,
            Some("assistant") => {
                let t = texts(&line["message"]["content"]);
                if !t.is_empty() {
                    let id = line["id"].as_str().unwrap_or_default().to_string();
                    return Some(Reply {
                        id,
                        text: t.join("\n\n"),
                    });
                }
            }
            Some("user") => return None,
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"fix it"}}
{"type":"assistant","uuid":"a1","message":{"id":"m1","content":[{"type":"text","text":"Looking."},{"type":"tool_use","id":"t1"}]}}
{"type":"user","uuid":"u2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}
{"type":"assistant","uuid":"a2","message":{"id":"m2","content":[{"type":"thinking","thinking":"hm"}]}}
{"type":"assistant","uuid":"a3","message":{"id":"m2","content":[{"type":"text","text":"Fixed the build."}]}}
{"type":"assistant","uuid":"a4","message":{"id":"m2","content":[{"type":"text","text":"Tests pass."}]}}
{"type":"attachment","uuid":"x"}"#;

    #[test]
    fn claude_reply_is_the_final_message_of_the_turn() {
        let r = last_reply_in(CLAUDE, &AgentType::ClaudeCode).unwrap();
        assert_eq!(r.id, "m2");
        assert_eq!(r.text, "Fixed the build.\n\nTests pass.");
    }

    #[test]
    fn claude_turn_without_text_has_no_reply() {
        let t = format!(
            "{CLAUDE}\n{}",
            r#"{"type":"user","uuid":"u9","message":{"role":"user","content":"next"}}"#
        );
        assert!(last_reply_in(&t, &AgentType::ClaudeCode).is_none());
    }

    /// The tail as the daemon can read it when the Stop hook reports idle
    /// before Claude Code has flushed the turn's final message: the newest
    /// text is an earlier one, and the turn went on with tools after it.
    const CLAUDE_UNFLUSHED: &str = r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"log the stats"}}
{"type":"assistant","uuid":"a1","message":{"id":"m1","content":[{"type":"thinking","thinking":"hm"}]}}
{"type":"assistant","uuid":"a2","message":{"id":"m1","content":[{"type":"text","text":"Logging the stats first."}]}}
{"type":"assistant","uuid":"a3","message":{"id":"m1","content":[{"type":"tool_use","id":"t1"}]}}
{"type":"user","uuid":"u2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}
{"type":"api-request"}
{"type":"assistant","uuid":"a4","message":{"id":"m2","content":[{"type":"tool_use","id":"t2"}]}}
{"type":"user","uuid":"u3","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}"#;

    /// The fixture's lines up to and including the one containing `marker`.
    fn through(fixture: &str, marker: &str) -> String {
        let lines: Vec<&str> = fixture.lines().collect();
        let end = lines
            .iter()
            .position(|l| l.contains(marker))
            .expect("marker is in the fixture");
        lines[..=end].join("\n")
    }

    #[test]
    fn claude_text_followed_by_tools_is_not_the_reply_yet() {
        assert!(last_reply_in(CLAUDE_UNFLUSHED, &AgentType::ClaudeCode).is_none());
        // The text and its tool call in one message, nothing after it yet.
        let cut = through(CLAUDE_UNFLUSHED, r#""uuid":"a3""#);
        assert!(last_reply_in(&cut, &AgentType::ClaudeCode).is_none());
        // Once the final message lands, it is the reply.
        let t = format!(
            "{CLAUDE_UNFLUSHED}\n{}",
            r#"{"type":"assistant","uuid":"a5","message":{"id":"m3","content":[{"type":"text","text":"Logged all 16 posts."}]}}"#
        );
        let r = last_reply_in(&t, &AgentType::ClaudeCode).unwrap();
        assert_eq!(
            (r.id.as_str(), r.text.as_str()),
            ("m3", "Logged all 16 posts.")
        );
    }

    #[test]
    fn pi_text_followed_by_a_tool_result_is_not_the_reply_yet() {
        let cut = through(PI, r#""id":"p3""#);
        assert!(last_reply_in(&cut, &AgentType::Pi).is_none());
        // The text that came with the tool call, nothing after it yet.
        let cut = through(PI, r#""id":"p2""#);
        assert!(last_reply_in(&cut, &AgentType::Pi).is_none());
    }

    #[test]
    fn claude_interrupted_turn_has_no_reply() {
        // The tool call's result, then the interrupt Claude Code writes.
        let cut = through(CLAUDE_UNFLUSHED, r#""uuid":"u2""#);
        let t = format!(
            "{cut}\n{}",
            r#"{"type":"user","uuid":"u9","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#
        );
        assert!(last_reply_in(&t, &AgentType::ClaudeCode).is_none());
    }

    const PI: &str = r#"{"type":"message","id":"p1","message":{"role":"user","content":[{"type":"text","text":"go"}]}}
{"type":"message","id":"p2","message":{"role":"assistant","content":[{"type":"text","text":"Running tests"},{"type":"toolCall","id":"c"}]}}
{"type":"message","id":"p3","message":{"role":"toolResult","content":[{"type":"text","text":"ok"}]}}
{"type":"message","id":"p4","message":{"role":"assistant","content":[{"type":"text","text":"All green."}]}}
{"type":"custom","id":"p5"}"#;

    #[test]
    fn pi_reply_is_the_last_assistant_text() {
        let r = last_reply_in(PI, &AgentType::Pi).unwrap();
        assert_eq!(
            r,
            Reply {
                id: "p4".into(),
                text: "All green.".into()
            }
        );
        assert_eq!(last_reply_in(PI, &AgentType::Omp).unwrap().id, "p4");
    }

    #[test]
    fn pi_reply_stops_at_the_latest_prompt() {
        let t = format!(
            "{PI}\n{}",
            r#"{"type":"message","id":"p6","message":{"role":"user","content":[{"type":"text","text":"more"}]}}"#
        );
        assert!(last_reply_in(&t, &AgentType::Pi).is_none());
    }

    #[test]
    fn garbage_and_unknown_agents_yield_nothing() {
        assert!(last_reply_in("{oops\n\n", &AgentType::ClaudeCode).is_none());
        assert!(last_reply_in(PI, &AgentType::Hermes).is_none());
    }

    #[test]
    fn reads_the_tail_of_a_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let filler = format!("{}\n", r#"{"type":"custom","id":"f"}"#).repeat(80_000);
        std::fs::write(&path, format!("{filler}{PI}\n")).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > TAIL_BYTES);
        let r = last_reply(&path, &AgentType::Pi).unwrap().unwrap();
        assert_eq!(r.id, "p4");
    }
}
