//! The texts of an agent's current turn, read from its session transcript.
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
//! A turn's texts are every assistant text since the last real user
//! prompt; tool results (user-role lines in Claude, `toolResult` in pi)
//! belong to the same turn. A text with a tool call, a tool result or
//! another message after it is progress: the turn went on. So is a Claude
//! text whose line says `stop_reason: "tool_use"`, before its tool call is
//! on disk (Claude Code stamps each line of a message with the message's
//! stop reason). The newest text
//! with nothing after it is the turn's reply once the turn is over. That
//! final message may not be on disk yet (Claude Code can run the Stop hook
//! that reports the turn idle before flushing it) or may never come (the
//! turn really ended on a tool result), so a missing reply does not always
//! mean "not yet".

use anyhow::{Context, Result};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::models::AgentType;

/// How much of the transcript's tail is read; a reply is never further back.
const TAIL_BYTES: u64 = 2 * 1024 * 1024;

/// An assistant text (progress or a reply) and the transcript id that
/// identifies it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reply {
    pub id: String,
    pub text: String,
}

/// The current turn in the transcript at `path`.
pub(crate) fn turn(path: &Path, agent: &AgentType) -> Result<Turn> {
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
    Ok(turn_in(text, agent))
}

/// The texts of the turn since the last real user prompt, oldest first.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Turn {
    /// Texts the turn has moved on from: a tool call, a tool result or
    /// another message came after each.
    pub progress: Vec<Reply>,
    /// The newest text, when nothing but non-tool lines came after it: the
    /// turn's reply once the turn is over.
    pub reply: Option<Reply>,
}

impl Turn {
    /// The id of the newest text in the turn.
    pub(crate) fn newest_id(&self) -> Option<String> {
        self.reply
            .as_ref()
            .or(self.progress.last())
            .map(|r| r.id.clone())
    }

    /// The part of the turn after the text with id `cursor`; all of it when
    /// `cursor` is not in this turn (a new turn, or nothing posted yet).
    pub(crate) fn since(&self, cursor: Option<&str>) -> Turn {
        let ids: Vec<&str> = self
            .progress
            .iter()
            .chain(&self.reply)
            .map(|r| r.id.as_str())
            .collect();
        match cursor.and_then(|c| ids.iter().rposition(|id| *id == c)) {
            None => self.clone(),
            Some(at) => Turn {
                progress: self.progress.iter().skip(at + 1).cloned().collect(),
                reply: self.reply.clone().filter(|_| at + 1 < ids.len()),
            },
        }
    }
}

/// [`turn`] over transcript text.
pub(crate) fn turn_in(text: &str, agent: &AgentType) -> Turn {
    let lines = text
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok());
    let events = match agent {
        AgentType::ClaudeCode => claude_events(lines),
        AgentType::Pi | AgentType::Omp => pi_events(lines),
        _ => Vec::new(),
    };
    turn_of(events)
}

/// The turn's reply as [`turn_in`] sees it.
#[cfg(test)]
fn last_reply_in(text: &str, agent: &AgentType) -> Option<Reply> {
    turn_in(text, agent).reply
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

/// What happened in a turn, in order.
#[derive(Debug)]
enum Event {
    /// Text of the message with this id.
    Text(String, Vec<String>),
    /// A tool call or tool result.
    Tool,
}

/// Texts become replies: consecutive texts of one message (Claude writes a
/// message's blocks on separate lines sharing `message.id`) are one; the
/// newest is the reply unless a tool came after it. Ids are assumed unique
/// per text: no message has text after its own tool call, which the API
/// never produces.
fn turn_of(events: Vec<Event>) -> Turn {
    let mut texts: Vec<(String, Vec<String>)> = Vec::new();
    let mut tool_last = false;
    for event in events {
        match event {
            Event::Tool => tool_last = true,
            Event::Text(id, mut t) => {
                match texts.last_mut() {
                    Some((last, acc)) if *last == id && !tool_last => acc.append(&mut t),
                    _ => texts.push((id, t)),
                }
                tool_last = false;
            }
        }
    }
    let mut progress: Vec<Reply> = texts
        .into_iter()
        .map(|(id, parts)| Reply {
            id,
            text: parts.join("\n\n"),
        })
        .collect();
    let reply = if tool_last { None } else { progress.pop() };
    Turn { progress, reply }
}

/// Claude's turn: the lines after the newest real prompt, oldest first.
fn claude_events(lines_rev: impl Iterator<Item = Value>) -> Vec<Event> {
    let mut events = Vec::new();
    for line in lines_rev {
        match line["type"].as_str() {
            Some("user") if is_claude_prompt(&line) => break,
            Some("user") => events.push(Event::Tool),
            Some("assistant") => {
                let content = &line["message"]["content"];
                // Reversed later: a line's tool call comes after its text.
                // Claude Code stamps every line of a message with its
                // `stop_reason`, so a text whose message goes on to call a
                // tool is known as progress before that call is on disk.
                if calls_tool(content) || line["message"]["stop_reason"] == "tool_use" {
                    events.push(Event::Tool);
                }
                let t = texts(content);
                if !t.is_empty() {
                    let id = line["message"]["id"]
                        .as_str()
                        .or_else(|| line["uuid"].as_str())
                        .unwrap_or_default()
                        .to_string();
                    events.push(Event::Text(id, t));
                }
            }
            _ => {}
        }
    }
    events.reverse();
    events
}

/// pi / omp's turn: the messages after the newest user message, oldest first.
fn pi_events(lines_rev: impl Iterator<Item = Value>) -> Vec<Event> {
    let mut events = Vec::new();
    for line in lines_rev.filter(|l| l["type"] == "message") {
        let content = &line["message"]["content"];
        match line["message"]["role"].as_str() {
            Some("user") => break,
            Some("toolResult") => events.push(Event::Tool),
            Some("assistant") => {
                if calls_tool(content) {
                    events.push(Event::Tool);
                }
                let t = texts(content);
                if !t.is_empty() {
                    let id = line["id"].as_str().unwrap_or_default().to_string();
                    events.push(Event::Text(id, t));
                }
            }
            _ => {}
        }
    }
    events.reverse();
    events
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

    fn ids(replies: &[Reply]) -> Vec<&str> {
        replies.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn texts_before_tools_are_progress_in_both_formats() {
        let t = turn_in(CLAUDE, &AgentType::ClaudeCode);
        assert_eq!(ids(&t.progress), ["m1"]);
        assert_eq!(t.progress[0].text, "Looking.");
        assert_eq!(t.reply.unwrap().id, "m2");

        let t = turn_in(CLAUDE_UNFLUSHED, &AgentType::ClaudeCode);
        assert_eq!(ids(&t.progress), ["m1"]);
        assert_eq!(t.progress[0].text, "Logging the stats first.");
        assert_eq!(t.reply, None);

        for agent in [AgentType::Pi, AgentType::Omp] {
            let t = turn_in(PI, &agent);
            assert_eq!(ids(&t.progress), ["p2"]);
            assert_eq!(t.progress[0].text, "Running tests");
            assert_eq!(t.reply.unwrap().id, "p4");
        }
    }

    #[test]
    fn a_text_still_being_written_is_not_progress_yet() {
        // Claude: the text line is on disk, its tool call is not.
        let cut = through(CLAUDE_UNFLUSHED, r#""uuid":"a2""#);
        let t = turn_in(&cut, &AgentType::ClaudeCode);
        assert!(t.progress.is_empty());
        assert_eq!(t.reply.unwrap().id, "m1");
        // A later message moves an earlier text without tools to progress.
        let t = format!(
            "{cut}\n{}",
            r#"{"type":"assistant","uuid":"b1","message":{"id":"m9","content":[{"type":"text","text":"More."}]}}"#
        );
        let t = turn_in(&t, &AgentType::ClaudeCode);
        assert_eq!(ids(&t.progress), ["m1"]);
        assert_eq!(t.reply.unwrap().id, "m9");
    }

    #[test]
    fn claude_stop_reason_marks_a_text_as_progress_before_its_tool_call() {
        // Cut right after the text line; its tool call is not on disk yet.
        let line = |stop: &str| {
            let prompt = r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"go"}}"#;
            format!(
                r#"{prompt}
{{"type":"assistant","uuid":"a1","message":{{"id":"m1","stop_reason":{stop},"content":[{{"type":"text","text":"Checking."}}]}}}}"#
            )
        };
        let t = turn_in(&line(r#""tool_use""#), &AgentType::ClaudeCode);
        assert_eq!(ids(&t.progress), ["m1"]);
        assert_eq!(t.reply, None);
        for stop in [r#""end_turn""#, "null"] {
            let t = turn_in(&line(stop), &AgentType::ClaudeCode);
            assert!(t.progress.is_empty());
            assert_eq!(t.reply.unwrap().id, "m1");
        }
    }

    #[test]
    fn since_skips_what_was_posted() {
        let t = turn_in(CLAUDE, &AgentType::ClaudeCode);
        assert_eq!(t.newest_id().as_deref(), Some("m2"));
        // Nothing posted from this turn yet: all of it.
        assert_eq!(t.since(None), t);
        assert_eq!(t.since(Some("an older turn")), t);
        // The progress text was posted: only the reply is left.
        let rest = t.since(Some("m1"));
        assert!(rest.progress.is_empty());
        assert_eq!(rest.reply.unwrap().id, "m2");
        // Everything posted.
        assert_eq!(t.since(Some("m2")), Turn::default());

        let t = turn_in(CLAUDE_UNFLUSHED, &AgentType::ClaudeCode);
        assert_eq!(t.newest_id().as_deref(), Some("m1"));
        assert_eq!(t.since(Some("m1")), Turn::default());
        assert_eq!(Turn::default().newest_id(), None);
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
        let r = turn(&path, &AgentType::Pi).unwrap().reply.unwrap();
        assert_eq!(r.id, "p4");
    }
}
