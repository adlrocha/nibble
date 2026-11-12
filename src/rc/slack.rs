//! Minimal Slack Web API client and the Block Kit messages `nibble rc` posts.
//!
//! Every call is form-encoded (all Web API methods accept that; structured
//! arguments such as `blocks` or `view` travel as JSON strings) and checked
//! for Slack's `"ok": true`. Rate-limited calls (HTTP 429) are retried once
//! after the `Retry-After` delay.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

const API_BASE: &str = "https://slack.com/api";
/// Slack caps a `markdown` block's text (and all markdown blocks of one
/// message together) at 12 000 characters; stay under it with headroom.
pub(crate) const MARKDOWN_LIMIT: usize = 11_000;
/// Longest wait honoured for a rate-limited call.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// A Web API client bound to one token.
pub(crate) struct Slack {
    token: String,
    agent: ureq::Agent,
}

/// Agent session states (`agents.sessions.setStatus`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SessionStatus {
    /// The agent is working on a turn.
    Processing,
    /// Ready for the next message.
    Active,
    /// Waiting on you (a permission prompt or question).
    Suspended,
    /// The session was closed.
    Closed,
}

impl SessionStatus {
    fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Processing => "processing",
            SessionStatus::Active => "active",
            SessionStatus::Suspended => "suspended",
            SessionStatus::Closed => "closed",
        }
    }
}

impl Slack {
    /// Client for `token` (a bot `xoxb-` or app-level `xapp-` token).
    pub(crate) fn new(token: &str) -> Self {
        Self {
            token: token.to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(30))
                .build(),
        }
    }

    /// Call `method` with form `params`; returns the response body when
    /// Slack reports `ok`.
    pub(crate) fn call(&self, method: &str, params: &[(&str, &str)]) -> Result<Value> {
        let url = format!("{API_BASE}/{method}");
        let auth = format!("Bearer {}", self.token);
        let send = || {
            self.agent
                .post(&url)
                .set("Authorization", &auth)
                .send_form(params)
                .map_err(Box::new)
        };
        let resp = match send() {
            Ok(r) => r,
            Err(e) => match *e {
                ureq::Error::Status(429, r) => {
                    let wait = r
                        .header("Retry-After")
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(Duration::from_secs)
                        .unwrap_or(Duration::from_secs(1))
                        .min(MAX_RETRY_AFTER);
                    std::thread::sleep(wait);
                    send().map_err(|e| anyhow::anyhow!("Slack {method}: {e}"))?
                }
                other => bail!("Slack {method}: {other}"),
            },
        };
        let body: Value = resp
            .into_json()
            .with_context(|| format!("Slack {method}: unreadable response"))?;
        check_ok(method, body)
    }

    /// `auth.test`: who this token is. Returns `(team, bot user)`.
    pub(crate) fn auth_test(&self) -> Result<(String, String)> {
        let v = self.call("auth.test", &[])?;
        Ok((str_of(&v, "team"), str_of(&v, "user")))
    }

    /// `apps.connections.open` (app-level token): a Socket Mode URL.
    pub(crate) fn open_connection(&self) -> Result<String> {
        let v = self.call("apps.connections.open", &[])?;
        v["url"]
            .as_str()
            .map(str::to_string)
            .context("apps.connections.open returned no url")
    }

    /// `conversations.open`: the DM channel between the bot and `user`.
    pub(crate) fn open_dm(&self, user: &str) -> Result<String> {
        let v = self.call("conversations.open", &[("users", user)])?;
        v["channel"]["id"]
            .as_str()
            .map(str::to_string)
            .context("conversations.open returned no channel id")
    }

    /// `chat.postMessage`; returns the new message's `ts`.
    pub(crate) fn post(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
        blocks: &Value,
    ) -> Result<String> {
        let blocks = blocks.to_string();
        let mut params = vec![("channel", channel), ("text", text), ("blocks", &blocks)];
        if let Some(ts) = thread_ts {
            params.push(("thread_ts", ts));
        }
        let v = self.call("chat.postMessage", &params)?;
        v["ts"]
            .as_str()
            .map(str::to_string)
            .context("chat.postMessage returned no ts")
    }

    /// `chat.update`: replace a message's text and blocks.
    pub(crate) fn update(&self, channel: &str, ts: &str, text: &str, blocks: &Value) -> Result<()> {
        let blocks = blocks.to_string();
        self.call(
            "chat.update",
            &[
                ("channel", channel),
                ("ts", ts),
                ("text", text),
                ("blocks", &blocks),
            ],
        )
        .map(drop)
    }

    /// `reactions.add`; an emoji that is already there is not an error.
    pub(crate) fn react(&self, channel: &str, ts: &str, emoji: &str) -> Result<()> {
        match self.call(
            "reactions.add",
            &[("channel", channel), ("timestamp", ts), ("name", emoji)],
        ) {
            Err(e) if error_code(&e) == Some("already_reacted") => Ok(()),
            other => other.map(drop),
        }
    }

    /// `chat.getPermalink` for a message.
    pub(crate) fn permalink(&self, channel: &str, ts: &str) -> Result<String> {
        let v = self.call(
            "chat.getPermalink",
            &[("channel", channel), ("message_ts", ts)],
        )?;
        v["permalink"]
            .as_str()
            .map(str::to_string)
            .context("chat.getPermalink returned no permalink")
    }

    /// `agents.sessions.setStatus` for the session on a thread.
    pub(crate) fn set_status(
        &self,
        channel: &str,
        thread_ts: &str,
        status: SessionStatus,
    ) -> Result<()> {
        self.call(
            "agents.sessions.setStatus",
            &[
                ("channel_id", channel),
                ("thread_ts", thread_ts),
                ("status", status.as_str()),
            ],
        )
        .map(drop)
    }

    /// `agents.sessions.rename`: the session's title in Slack's agent view.
    pub(crate) fn rename(&self, channel: &str, thread_ts: &str, title: &str) -> Result<()> {
        self.call(
            "agents.sessions.rename",
            &[
                ("channel_id", channel),
                ("thread_ts", thread_ts),
                ("title", title),
            ],
        )
        .map(drop)
    }

    /// `views.open`: show a modal in response to an interaction.
    pub(crate) fn open_view(&self, trigger_id: &str, view: &Value) -> Result<()> {
        let view = view.to_string();
        self.call("views.open", &[("trigger_id", trigger_id), ("view", &view)])
            .map(drop)
    }
}

/// A Web API call Slack answered with `"ok": false`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SlackError {
    pub method: String,
    /// Slack's error code, e.g. `missing_scope`.
    pub code: String,
    /// The scope Slack says is missing, for `missing_scope`.
    pub needed: Option<String>,
}

impl std::fmt::Display for SlackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Slack {} failed: {}", self.method, self.code)?;
        if let Some(scope) = &self.needed {
            write!(f, " (missing scope: {scope})")?;
        }
        Ok(())
    }
}

impl std::error::Error for SlackError {}

/// Slack's error code, when `e` is a [`SlackError`] (not a network error).
pub(crate) fn error_code(e: &anyhow::Error) -> Option<&str> {
    e.downcast_ref::<SlackError>().map(|s| s.code.as_str())
}

fn check_ok(method: &str, body: Value) -> Result<Value> {
    if body["ok"].as_bool() == Some(true) {
        return Ok(body);
    }
    Err(SlackError {
        method: method.to_string(),
        code: body["error"]
            .as_str()
            .unwrap_or("unknown_error")
            .to_string(),
        needed: body["needed"].as_str().map(str::to_string),
    }
    .into())
}

fn str_of(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// Split `text` into pieces of at most `max` characters, preferring line
/// breaks, then spaces, so markdown survives the cut.
pub(crate) fn chunk_text(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while rest.chars().count() > max {
        let limit = rest.char_indices().nth(max).map_or(rest.len(), |(i, _)| i);
        // Include the first character past the limit: a break right there
        // still yields a full-length chunk.
        let window = rest[limit..]
            .chars()
            .next()
            .map_or(limit, |c| limit + c.len_utf8());
        let head = &rest[..window];
        let cut = head
            .rfind('\n')
            .or_else(|| head.rfind(' '))
            .filter(|&i| i > 0)
            .unwrap_or(limit);
        out.push(rest[..cut].trim_end().to_string());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

/// One-line notification text (what phones and the sidebar preview show).
pub(crate) fn preview(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(empty)");
    let mut out: String = line.chars().take(150).collect();
    if line.chars().count() > 150 {
        out.push('…');
    }
    out
}

/// Blocks for an agent reply: markdown blocks, one per message chunk.
pub(crate) fn markdown_blocks(text: &str) -> Value {
    json!([{ "type": "markdown", "text": text }])
}

/// Most text one context block element takes.
const CONTEXT_LIMIT: usize = 3000;

/// Blocks for a progress text (one the turn went on from): a context block,
/// small and grey, so the turn's reply stands out. `None` when the text is
/// too long for one, to be posted like a reply instead. Context blocks take
/// mrkdwn, so `**bold**` becomes `*bold*`; inline code and plain text read
/// the same in both.
pub(crate) fn progress_blocks(text: &str) -> Option<Value> {
    let text = escape(text.trim()).replace("**", "*");
    (text.chars().count() <= CONTEXT_LIMIT).then(|| note_blocks(&text))
}

/// What a session's card (the message with its Close button) says about it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionCard {
    pub task_id: String,
    pub title: String,
    pub agent: String,
    /// "host" or "sandbox".
    pub location: String,
    pub cwd: Option<String>,
    pub zellij: String,
    pub topic: Option<String>,
}

/// Action id of the session card's Disconnect button (stop relaying; the
/// session keeps running).
pub(crate) const CLOSE_ACTION: &str = "rc_close";
/// Action id of the End session button: stop the agent and close its tab.
pub(crate) const END_ACTION: &str = "rc_end";

/// What a session card offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardState {
    /// Relaying. `endable`: started from Slack, so it can be ended here.
    Open { endable: bool },
    /// No longer relayed; the session runs on in zellij.
    Disconnected,
    /// The agent was stopped and its zellij tab closed.
    Ended,
}

/// A button behind a confirmation dialog.
fn confirmed_button(
    action_id: &str,
    label: &str,
    task_id: &str,
    danger: bool,
    (title, text, yes): (&str, &str, &str),
) -> Value {
    let mut button = json!({
        "type": "button",
        "action_id": action_id,
        "text": { "type": "plain_text", "text": label },
        "value": task_id,
        "confirm": {
            "title": { "type": "plain_text", "text": title },
            "text": { "type": "mrkdwn", "text": text },
            "confirm": { "type": "plain_text", "text": yes },
            "deny": { "type": "plain_text", "text": "Cancel" }
        }
    });
    if danger {
        button["style"] = json!("danger");
        button["confirm"]["style"] = json!("danger");
    }
    button
}
/// Action id of the "Start a session" button offered on a top-level DM.
pub(crate) const NEW_ACTION: &str = "rc_new";

/// A session card: the thread root for `/nibble-rc`, a reply in threads
/// started from Slack. An open card has a Disconnect button, plus End
/// session when the session was started from Slack; a finished one says
/// how it finished instead.
pub(crate) fn session_card_blocks(card: &SessionCard, state: CardState) -> Value {
    let mut facts = format!(
        "*{}* · {} · {} · zellij `{}`",
        escape(&card.title),
        card.agent,
        card.location,
        escape(&card.zellij)
    );
    if let Some(cwd) = &card.cwd {
        facts.push_str(&format!("\n`{}`", escape(cwd)));
    }
    let mut blocks = vec![json!({
        "type": "section",
        "text": { "type": "mrkdwn", "text": facts }
    })];
    if let Some(topic) = card.topic.as_deref().filter(|t| !t.is_empty()) {
        blocks.push(json!({
            "type": "context",
            "elements": [{ "type": "mrkdwn", "text": escape(topic) }]
        }));
    }
    let footer =
        |text: &str| json!({ "type": "context", "elements": [{ "type": "mrkdwn", "text": text }] });
    match state {
        CardState::Open { endable } => {
            blocks.push(footer("Reply in this thread to talk to the session."));
            let mut buttons = vec![confirmed_button(
                CLOSE_ACTION,
                "Disconnect",
                &card.task_id,
                false,
                (
                    "Disconnect from Slack?",
                    "Slack stops relaying this session. It keeps running in zellij.",
                    "Disconnect",
                ),
            )];
            if endable {
                buttons.push(confirmed_button(
                    END_ACTION,
                    "End session",
                    &card.task_id,
                    true,
                    (
                        "End this session?",
                        "This stops the agent and closes its zellij tab. The \
                         conversation stays in the agent's history.",
                        "End session",
                    ),
                ));
            }
            blocks.push(json!({ "type": "actions", "elements": buttons }));
        }
        CardState::Disconnected => {
            blocks.push(footer("Disconnected. The session keeps running in zellij."))
        }
        CardState::Ended => blocks.push(footer(
            "Ended. The agent stopped and its zellij tab was closed.",
        )),
    }
    json!(blocks)
}

/// Fallback text for the session card.
pub(crate) fn session_card_text(card: &SessionCard) -> String {
    format!("{} · {} · {}", card.title, card.agent, card.location)
}

/// Slack caps a button's value at 2000 characters.
const BUTTON_VALUE_MAX: usize = 2000;

/// The Start button's value: the message to start from and its text (as
/// much as fits), which prefills the form's first message.
pub(crate) fn offer_value(message_ts: &str, text: &str) -> String {
    let mut prompt: String = text.to_string();
    loop {
        let v = json!({ "ts": message_ts, "prompt": prompt }).to_string();
        if v.chars().count() <= BUTTON_VALUE_MAX {
            return v;
        }
        let keep = prompt
            .chars()
            .count()
            .saturating_sub(v.chars().count() - BUTTON_VALUE_MAX + 1);
        prompt = prompt.chars().take(keep).collect();
    }
}

/// Reply to a top-level DM: offer to start a session for it.
pub(crate) fn new_session_offer_blocks(message_ts: &str, text: &str) -> Value {
    json!([
        {
            "type": "section",
            "text": { "type": "mrkdwn", "text": "Start a session for this? Pick where it runs." },
            "accessory": {
                "type": "button",
                "action_id": NEW_ACTION,
                "style": "primary",
                "text": { "type": "plain_text", "text": "Start session…" },
                "value": offer_value(message_ts, text)
            }
        }
    ])
}

/// A short status line posted into a thread (context block).
pub(crate) fn note_blocks(text: &str) -> Value {
    json!([{ "type": "context", "elements": [{ "type": "mrkdwn", "text": text }] }])
}

/// Escape the three characters Slack's mrkdwn treats as control.
pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_a_context_block_unless_too_long() {
        let b = progress_blocks("  Checking **a < b** in `x.rs`\n").unwrap();
        assert_eq!(b[0]["type"], "context");
        assert_eq!(b[0]["elements"][0]["text"], "Checking *a &lt; b* in `x.rs`");
        assert!(progress_blocks(&"x".repeat(CONTEXT_LIMIT)).is_some());
        assert!(progress_blocks(&"x".repeat(CONTEXT_LIMIT + 1)).is_none());
    }

    #[test]
    fn ok_responses_pass_and_errors_name_the_scope() {
        assert!(check_ok("m", json!({"ok": true})).is_ok());
        let err = check_ok(
            "chat.postMessage",
            json!({"ok": false, "error": "missing_scope", "needed": "chat:write"}),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "Slack chat.postMessage failed: missing_scope (missing scope: chat:write)"
        );
        assert!(check_ok("m", json!({})).is_err());
        let err = check_ok("m", json!({"ok": false, "error": "not_allowed"})).unwrap_err();
        assert_eq!(error_code(&err), Some("not_allowed"));
        assert_eq!(error_code(&anyhow::anyhow!("network down")), None);
    }

    #[test]
    fn chunks_prefer_line_breaks_and_keep_everything() {
        let text = "aaaa\nbbbb\ncccc";
        assert_eq!(chunk_text(text, 9), vec!["aaaa\nbbbb", "cccc"]);
        let long = "x".repeat(25);
        let parts = chunk_text(&long, 10);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts.concat(), long);
        assert!(chunk_text("  ", 10).is_empty());
        // Multibyte text is cut on character boundaries.
        let emoji = "😀".repeat(12);
        assert!(chunk_text(&emoji, 5).iter().all(|c| c.chars().count() <= 5));
    }

    #[test]
    fn preview_takes_the_first_line_capped() {
        assert_eq!(preview("\n\n  hello \nworld"), "hello");
        assert_eq!(preview(""), "(empty)");
        let p = preview(&"y".repeat(200));
        assert_eq!(p.chars().count(), 151);
        assert!(p.ends_with('…'));
    }

    fn card() -> SessionCard {
        SessionCard {
            task_id: "t1".into(),
            title: "[nibble:main]".into(),
            agent: "claude".into(),
            location: "host".into(),
            cwd: Some("/home/u/nibble".into()),
            zellij: "main".into(),
            topic: Some("fix <the> build".into()),
        }
    }

    #[test]
    fn open_card_has_disconnect_button_with_task_id() {
        let blocks = session_card_blocks(&card(), CardState::Open { endable: false });
        let actions = blocks.as_array().unwrap().last().unwrap();
        assert_eq!(actions["elements"][0]["action_id"], CLOSE_ACTION);
        assert_eq!(actions["elements"][0]["value"], "t1");
        // Sessions sent with /nibble-rc cannot be ended from Slack.
        assert_eq!(actions["elements"].as_array().unwrap().len(), 1);
        assert!(!blocks.to_string().contains(END_ACTION));
        // mrkdwn control characters are escaped.
        assert!(blocks.to_string().contains("fix &lt;the&gt; build"));
    }

    #[test]
    fn offer_value_fits_slack_and_round_trips() {
        let v: Value = serde_json::from_str(&offer_value("1.2", "fix \"it\"")).unwrap();
        assert_eq!(
            (v["ts"].as_str(), v["prompt"].as_str()),
            (Some("1.2"), Some("fix \"it\""))
        );
        let long = offer_value("1.2", &"\"q".repeat(3000));
        assert!(long.chars().count() <= BUTTON_VALUE_MAX);
        let v: Value = serde_json::from_str(&long).unwrap();
        assert_eq!(v["ts"], "1.2");
    }

    #[test]
    fn sessions_started_from_slack_can_be_ended_after_a_confirmation() {
        let blocks = session_card_blocks(&card(), CardState::Open { endable: true });
        let actions = blocks.as_array().unwrap().last().unwrap();
        let end = &actions["elements"][1];
        assert_eq!(end["action_id"], END_ACTION);
        assert_eq!(end["value"], "t1");
        assert_eq!(end["style"], "danger");
        assert_eq!(end["confirm"]["confirm"]["text"], "End session");
    }

    #[test]
    fn finished_cards_have_no_buttons_and_say_how_they_finished() {
        for (state, says) in [
            (CardState::Disconnected, "keeps running"),
            (CardState::Ended, "tab was closed"),
        ] {
            let text = session_card_blocks(&card(), state).to_string();
            assert!(!text.contains(CLOSE_ACTION) && !text.contains(END_ACTION));
            assert!(text.contains(says), "{text}");
        }
    }
}
