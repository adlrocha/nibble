//! `nibble rc daemon`: the Slack side of remote control.
//!
//! One thread serves the Socket Mode connection and, between frames, runs a
//! poll pass over nibble's state:
//!
//! - **Requests** from `/nibble-rc` (host or sandbox) link a session to a
//!   new DM thread, or close the link.
//! - **Pending spawns** (sessions started from Slack) wait for their agent to
//!   register in its new zellij pane, then receive the first message.
//! - **Links** relay status: working → Slack session `processing`, a
//!   permission prompt → a note plus `suspended`, a finished turn → the
//!   agent's last reply in the thread plus `active`.
//!
//! Inbound, a reply in a session thread is typed into the agent's pane;
//! Slack's stop button presses Escape there; the Close button ends the
//! link (the zellij tab stays). Only the configured Slack user is heard.

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::modal::{self, NewSession, Origin};
use super::secrets::Secrets;
use super::slack::{self, SessionCard, SessionStatus, Slack};
use super::socket::{self, Envelope, Read};
use super::store::{self, CardInfo, Link, RequestAction, Response};
use super::{pane, relay, transcript};
use crate::config::RcConfig;
use crate::db::Database;
use crate::models::{SandboxType, Task};
use crate::pane_input;

/// Reconnect when the socket has carried no frame at all (not even one of
/// Slack's pings, which arrive far more often) for this long.
const SILENCE_LIMIT: Duration = Duration::from_secs(180);
/// Longest wait between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How often the heartbeat is written.
const BEAT_EVERY: Duration = Duration::from_secs(30);
/// Lock file (in `~/.nibble`) held by the running daemon.
const DAEMON_LOCK: &str = "rc-daemon";
/// Remembered event ids, to drop Slack's redeliveries.
const SEEN_CAP: usize = 256;

/// Run the daemon until killed.
pub(crate) fn run(cfg: RcConfig, db_path: PathBuf) -> Result<()> {
    // One daemon per machine: a second would relay the same turns and
    // answer the same requests, posting everything twice.
    let _lock = crate::lock::try_lock_in(&crate::db::ensure_data_dir()?, DAEMON_LOCK)?
        .with_context(|| {
            "another nibble rc daemon is already running (systemctl --user status nibble-rc)"
        })?;
    let secrets = super::secrets::load()?;
    let bot = Slack::new(&secrets.bot_token);
    let app = Slack::new(&secrets.app_token);
    let (team, bot_user) = bot.auth_test().context("bot token check failed")?;
    let dm = bot
        .open_dm(&secrets.user_id)
        .context("cannot open the DM with you")?;
    let db = Database::open(&db_path).context("failed to open the nibble database")?;
    log(&format!("connected to {team} as {bot_user}; DM {dm}"));

    let mut daemon = Daemon {
        cfg,
        secrets,
        bot,
        dm,
        db,
        sessions_api: true,
        seen: Seen::default(),
        last_beat: None,
        repos_cache: None,
    };
    // Warm the form's repo list so the first click opens it quickly.
    if let Err(e) = daemon.repos() {
        log(&format!("repo scan failed: {e:#}"));
    }
    let mut backoff = Duration::from_secs(1);
    loop {
        match daemon.serve_once(&app) {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                log(&format!(
                    "connection error: {e:#}; reconnecting in {}s",
                    backoff.as_secs()
                ));
                daemon.idle_for(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

pub(super) fn log(msg: &str) {
    eprintln!(
        "{} nibble rc: {msg}",
        Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    );
}

/// Work to do after a Socket Mode envelope is acknowledged (Slack wants
/// the ack within 3 seconds).
enum After {
    Nothing,
    Start(NewSession),
    /// A button click (`block_actions` payload).
    Action(Value),
    /// Open the new-session form for this trigger.
    OpenForm(String),
}

/// How old the form's repo list may get before the poll pass rescans.
const REPOS_TTL: Duration = Duration::from_secs(300);

pub(super) struct Daemon {
    pub(super) cfg: RcConfig,
    pub(super) secrets: Secrets,
    pub(super) bot: Slack,
    pub(super) dm: String,
    pub(super) db: Database,
    /// False once Slack rejected the agent-session methods (app not
    /// declared as an agent): they are skipped from then on.
    pub(super) sessions_api: bool,
    pub(super) seen: Seen,
    pub(super) last_beat: Option<Instant>,
    pub(super) repos_cache: Option<(Instant, Vec<PathBuf>)>,
}

impl Daemon {
    /// Serve one Socket Mode connection until Slack asks for a reconnect.
    fn serve_once(&mut self, app: &Slack) -> Result<()> {
        let url = app
            .open_connection()
            .context("apps.connections.open failed")?;
        let mut conn = socket::connect(&url)?;
        let poll = Duration::from_millis(self.cfg.poll_ms.max(200));
        let mut last_frame = Instant::now();
        let mut last_poll = Instant::now() - poll;
        loop {
            match conn.read()? {
                Read::Text(text) => {
                    last_frame = Instant::now();
                    match socket::parse_envelope(&text) {
                        Ok(Envelope::Hello) => log("socket ready"),
                        Ok(Envelope::Disconnect { reason }) => {
                            log(&format!("Slack asked to reconnect ({reason})"));
                            return Ok(());
                        }
                        Ok(env) => self.dispatch(&mut conn, env)?,
                        Err(e) => log(&format!("ignoring frame: {e:#}")),
                    }
                }
                // Pings count: Slack keeps a quiet socket alive with them.
                Read::Control => last_frame = Instant::now(),
                Read::Idle => {}
            }
            if last_poll.elapsed() >= poll {
                last_poll = Instant::now();
                if let Err(e) = self.poll_pass() {
                    log(&format!("poll failed: {e:#}"));
                }
            }
            if last_frame.elapsed() > SILENCE_LIMIT {
                log("socket silent; reconnecting");
                return Ok(());
            }
        }
    }

    /// Keep polling while waiting to reconnect, so sessions keep relaying
    /// outbound while Slack's socket is down.
    fn idle_for(&mut self, wait: Duration) {
        let until = Instant::now() + wait;
        while Instant::now() < until {
            let _ = self.poll_pass();
            std::thread::sleep(Duration::from_millis(self.cfg.poll_ms.clamp(200, 5000)));
        }
    }

    fn dispatch(&mut self, conn: &mut socket::Connection, env: Envelope) -> Result<()> {
        let id = env.envelope_id().unwrap_or_default().to_string();
        // Slack redelivers what it did not see acknowledged in 3 seconds
        // (a long poll pass can delay the read): act once, re-ack the rest.
        let key = redelivery_key(&env);
        if let Some(key) = &key {
            if !self.first_sight(key) {
                if !id.is_empty() {
                    conn.send(socket::ack_frame(&id, None))?;
                }
                return Ok(());
            }
        }
        let (ack, after) = match env {
            Envelope::Event { payload, .. } => {
                conn.send(socket::ack_frame(&id, None))?;
                if let Err(e) = self.on_event(&payload) {
                    log(&format!("event failed: {e:#}"));
                }
                return Ok(());
            }
            Envelope::Interactive { payload, .. } => self.on_interactive(payload),
            Envelope::SlashCommand { payload, .. } => self.on_slash(&payload),
            Envelope::Other { kind, .. } => {
                log(&format!("acknowledged unhandled {kind} envelope"));
                (None, After::Nothing)
            }
            Envelope::Hello | Envelope::Disconnect { .. } => (None, After::Nothing),
        };
        // A rejected form stays open with the same view id: forget the key so
        // the corrected resubmission is handled.
        if ack.is_some() {
            if let Some(key) = &key {
                self.forget(key);
            }
        }
        if !id.is_empty() {
            if let Err(e) = conn.send(socket::ack_frame(&id, ack.as_ref())) {
                // Unacknowledged: Slack redelivers it, and it must be acted
                // on then rather than dropped as a repeat.
                if let Some(key) = &key {
                    self.forget(key);
                }
                return Err(e);
            }
        }
        match after {
            After::Nothing => {}
            After::Start(new) => {
                if let Err(e) = self.start_session(&new) {
                    log(&format!("start failed: {e:#}"));
                    let text = format!(":warning: Could not start the session: {e:#}");
                    match &new.origin.thread_ts {
                        Some(ts) => self.note(&new.origin.channel, ts, &text),
                        None => self.say(&new.origin.channel, &text),
                    }
                }
            }
            After::Action(payload) => {
                if let Err(e) = self.on_action(&payload) {
                    log(&format!("action failed: {e:#}"));
                }
            }
            After::OpenForm(trigger) => {
                let origin = Origin {
                    channel: self.dm.clone(),
                    thread_ts: None,
                };
                if let Err(e) = self.open_form(&trigger, &origin, "") {
                    log(&format!("form failed: {e:#}"));
                    self.say(
                        &self.dm.clone(),
                        &format!(":warning: Could not open the form: {e:#}"),
                    );
                }
            }
        }
        Ok(())
    }

    fn allowed(&self, user: Option<&str>) -> bool {
        user == Some(self.secrets.user_id.as_str())
    }

    fn first_sight(&mut self, id: &str) -> bool {
        self.seen.first_sight(id)
    }

    fn forget(&mut self, id: &str) {
        self.seen.forget(id);
    }

    // ── Inbound ───────────────────────────────────────────────────────────

    fn on_event(&mut self, payload: &Value) -> Result<()> {
        let event_id = payload["event_id"].as_str().unwrap_or_default().to_string();
        if !self.first_sight(&event_id) {
            return Ok(());
        }
        let ev = &payload["event"];
        match ev["type"].as_str() {
            Some("message") => self.on_message(ev),
            Some("agent_session_stopped") => {
                if !self.allowed(ev["user"].as_str()) {
                    return Ok(());
                }
                let (Some(channel), Some(thread)) =
                    (ev["channel"].as_str(), ev["thread_ts"].as_str())
                else {
                    return Ok(());
                };
                self.on_stop(channel, thread)
            }
            _ => Ok(()),
        }
    }

    fn on_message(&mut self, ev: &Value) -> Result<()> {
        let incoming = match classify_message(ev, &self.secrets.user_id) {
            Some(m) => m,
            None => return Ok(()),
        };
        match incoming {
            Incoming::TopLevel { channel, ts, text } => {
                let blocks = slack::new_session_offer_blocks(&ts, &text);
                self.bot
                    .post(&channel, Some(&ts), "Start a session for this?", &blocks)
                    .map(drop)
            }
            Incoming::Reply {
                channel,
                thread_ts,
                ts,
                text,
            } => self.on_reply(&channel, &thread_ts, &ts, &text),
        }
    }

    fn on_reply(&mut self, channel: &str, thread_ts: &str, ts: &str, text: &str) -> Result<()> {
        if let Some(mut spawn) = store::spawn_for_thread(&self.db, thread_ts)? {
            spawn.queued.push(text.to_string());
            store::put_spawn(&self.db, &spawn)?;
            let _ = self.bot.react(channel, ts, "hourglass_flowing_sand");
            return Ok(());
        }
        let Some(mut link) = store::link_by_thread(&self.db, channel, thread_ts)? else {
            self.note(
                channel,
                thread_ts,
                "No session here yet. Use *Start session…* above, or `/nibble`.",
            );
            return Ok(());
        };
        if link.closed {
            self.note(channel, thread_ts, "This session is closed. Start a new one with `/nibble`, or send `/nibble-rc` from the agent again.");
            return Ok(());
        }
        let task = self.db.get_task_by_id(&link.task_id)?;
        let target = task
            .as_ref()
            .and_then(|t| pane_input::target(t).map(|(s, p)| (t, s, p)));
        let Some((task, session, pane_id)) = target else {
            self.note(
                channel,
                thread_ts,
                ":warning: nibble does not know this session's zellij pane any more.",
            );
            return Ok(());
        };
        if !pane::agent_present(task, &session, pane_id)? {
            self.note(
                channel,
                thread_ts,
                &format!(":warning: The agent is no longer running in its pane (zellij `{session}`), so nothing was typed. Start the agent again in that session (if the whole session is down, `nibble mux {session}` brings its recorded tabs back), then send `/nibble-rc` from it."),
            );
            return Ok(());
        }
        pane_input::send_text(&session, pane_id, text)?;
        let _ = self.bot.react(channel, ts, "eyes");
        link.watch = relay::after_reply(link.watch);
        link.idle_checks = 0;
        store::put_link(&self.db, &link)?;
        self.session_status(channel, thread_ts, SessionStatus::Processing);
        Ok(())
    }

    fn on_stop(&mut self, channel: &str, thread_ts: &str) -> Result<()> {
        let Some(link) = store::link_by_thread(&self.db, channel, thread_ts)? else {
            return Ok(());
        };
        if link.closed {
            // Nothing to interrupt; keep Slack's state at closed.
            self.session_status(channel, thread_ts, SessionStatus::Closed);
            return Ok(());
        }
        let target = self
            .db
            .get_task_by_id(&link.task_id)?
            .as_ref()
            .and_then(pane_input::target);
        let note = match target {
            Some((session, pane_id)) => match pane_input::send_escape(&session, pane_id) {
                Ok(()) => ":black_square_for_stop: Interrupted.".to_string(),
                Err(e) => format!(":warning: Could not interrupt the agent: {e:#}"),
            },
            None => {
                ":warning: nibble does not know this session's zellij pane any more.".to_string()
            }
        };
        self.note(channel, thread_ts, &note);
        self.session_status(channel, thread_ts, SessionStatus::Active);
        Ok(())
    }

    fn on_interactive(&mut self, payload: Value) -> (Option<Value>, After) {
        if !self.allowed(payload["user"]["id"].as_str()) {
            return (None, After::Nothing);
        }
        match payload["type"].as_str() {
            Some("block_actions") => (None, After::Action(payload)),
            Some("view_submission") if payload["view"]["callback_id"] == modal::CALLBACK_ID => {
                let home = dirs::home_dir();
                match modal::parse_submission(&payload, home.as_deref(), &|p| p.is_dir()) {
                    Ok(new) => (None, After::Start(new)),
                    Err(errors) => (Some(modal::errors_response(&errors)), After::Nothing),
                }
            }
            _ => (None, After::Nothing),
        }
    }

    fn on_action(&mut self, payload: &Value) -> Result<()> {
        let action = &payload["actions"][0];
        match action["action_id"].as_str() {
            Some(slack::CLOSE_ACTION) => action["value"]
                .as_str()
                .map_or(Ok(()), |task| self.close_link(task, "Closed from Slack.")),
            Some(slack::NEW_ACTION) => {
                let trigger = payload["trigger_id"].as_str().context("no trigger_id")?;
                let offer: Value = action["value"]
                    .as_str()
                    .and_then(|v| serde_json::from_str(v).ok())
                    .unwrap_or(Value::Null);
                let origin = Origin {
                    channel: payload["channel"]["id"]
                        .as_str()
                        .unwrap_or(&self.dm)
                        .to_string(),
                    thread_ts: offer["ts"].as_str().map(str::to_string),
                };
                self.open_form(
                    trigger,
                    &origin,
                    offer["prompt"].as_str().unwrap_or_default(),
                )
            }
            _ => Ok(()),
        }
    }

    /// The repo choices for the form. A cached list is used whatever its
    /// age: the poll pass refreshes it, so opening the form never spends
    /// Slack's 3-second trigger budget on a directory walk. Only the very
    /// first form scans synchronously.
    pub(super) fn repos(&mut self) -> Result<Vec<PathBuf>> {
        if let Some((_, repos)) = &self.repos_cache {
            return Ok(repos.clone());
        }
        self.refresh_repos()
    }

    fn refresh_repos(&mut self) -> Result<Vec<PathBuf>> {
        let repos = modal::repo_choices(&self.db, &self.cfg)?;
        self.repos_cache = Some((Instant::now(), repos.clone()));
        Ok(repos)
    }

    fn open_form(&mut self, trigger: &str, origin: &Origin, prompt: &str) -> Result<()> {
        let repos = self.repos()?;
        let home = dirs::home_dir();
        self.bot.open_view(
            trigger,
            &modal::view(origin, &repos, prompt, home.as_deref()),
        )
    }

    fn on_slash(&mut self, payload: &Value) -> (Option<Value>, After) {
        if !self.allowed(payload["user_id"].as_str()) {
            return (
                Some(json!({ "text": "This nibble belongs to someone else." })),
                After::Nothing,
            );
        }
        match payload["text"].as_str().unwrap_or_default().trim() {
            "" | "new" => match payload["trigger_id"].as_str() {
                Some(trigger) => (None, After::OpenForm(trigger.to_string())),
                None => (
                    Some(json!({ "text": "Slack sent no trigger; try again." })),
                    After::Nothing,
                ),
            },
            "list" => (Some(json!({ "text": self.list_text() })), After::Nothing),
            _ => (Some(json!({ "text": SLASH_HELP })), After::Nothing),
        }
    }

    fn list_text(&self) -> String {
        let links = store::links(&self.db).unwrap_or_default();
        let open: Vec<String> = links
            .iter()
            .filter(|l| !l.closed)
            .map(|l| {
                let place = l
                    .permalink
                    .as_deref()
                    .map(|p| format!(" <{p}|thread>"))
                    .unwrap_or_default();
                format!(
                    "• *{}* · {} · {}{place}",
                    slack::escape(&l.card.title),
                    l.card.agent,
                    l.card.location
                )
            })
            .collect();
        if open.is_empty() {
            "No sessions in Slack. Send `/nibble-rc` from an agent, or `/nibble` to start one."
                .to_string()
        } else {
            open.join("\n")
        }
    }

    // ── Links: /nibble-rc and Close ───────────────────────────────────────

    fn start_link(&mut self, task_id: &str) -> Result<String> {
        if let Some(link) = store::link(&self.db, task_id)?.filter(|l| !l.closed) {
            return Ok(format!(
                "Already in Slack{}",
                link.permalink.map(|p| format!(": {p}")).unwrap_or_default()
            ));
        }
        let task = self
            .db
            .get_task_by_id(task_id)?
            .with_context(|| format!("nibble has no task {task_id}"))?;
        let Some((session, _)) = pane_input::target(&task) else {
            anyhow::bail!(
                "this session's zellij pane is unknown to nibble; run the agent inside zellij"
            );
        };
        let card = card_for(&task, &session);
        let topic = crate::status::topic_of(&task).map(str::to_string);
        // The thread is the card itself, so its ts is filled in on posting.
        let mut link = Link::new(&task.task_id, &self.dm, "", card, relay::observe(&task));
        let session_card = session_card(&link, topic.as_deref());
        link.thread_ts = self.bot.post(
            &self.dm,
            None,
            &slack::session_card_text(&session_card),
            &slack::session_card_blocks(&session_card, true),
        )?;
        link.permalink = self.bot.permalink(&self.dm, &link.thread_ts).ok();
        // The card exists now: record the link before anything else can
        // fail, so its thread and Close button always have a session.
        let reply = self.current_reply(&task);
        link.last_reply_id = reply.as_ref().map(|r| r.id.clone());
        store::put_link(&self.db, &link)?;
        if let Some(reply) = reply {
            if let Err(e) = self.post_reply(&link, &format!("*Last reply*\n\n{}", reply.text)) {
                log(&format!("could not post the last reply: {e:#}"));
            }
        }
        let status = relay::status_of(link.watch);
        self.session_status(&link.channel, &link.thread_ts, status);
        self.session_title(
            &link.channel,
            &link.thread_ts,
            topic.as_deref().unwrap_or(&link.card.title),
        );
        Ok(format!(
            "Sent to Slack{}",
            link.permalink.map(|p| format!(": {p}")).unwrap_or_default()
        ))
    }

    pub(super) fn close_link(&mut self, task_id: &str, why: &str) -> Result<()> {
        let Some(mut link) = store::link(&self.db, task_id)? else {
            return Ok(());
        };
        if link.closed {
            return Ok(());
        }
        link.closed = true;
        store::put_link(&self.db, &link)?;
        let card = session_card(&link, None);
        if let Err(e) = self.bot.update(
            &link.channel,
            link.card_message(),
            &slack::session_card_text(&card),
            &slack::session_card_blocks(&card, false),
        ) {
            log(&format!("could not update the session card: {e:#}"));
        }
        self.note(&link.channel, &link.thread_ts, why);
        self.session_status(&link.channel, &link.thread_ts, SessionStatus::Closed);
        Ok(())
    }

    // ── Poll pass ─────────────────────────────────────────────────────────

    fn poll_pass(&mut self) -> Result<()> {
        let now = Utc::now();
        if self.last_beat.is_none_or(|b| b.elapsed() >= BEAT_EVERY) {
            store::beat(&self.db, now)?;
            self.last_beat = Some(Instant::now());
        }
        if self
            .repos_cache
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() >= REPOS_TTL)
        {
            if let Err(e) = self.refresh_repos() {
                log(&format!("repo scan failed: {e:#}"));
            }
        }
        for (task_id, req) in store::requests(&self.db)? {
            let result = match req.action {
                RequestAction::Start => self.start_link(&task_id),
                RequestAction::Stop => match store::link(&self.db, &task_id)? {
                    Some(l) if !l.closed => self
                        .close_link(&task_id, "Closed from the terminal.")
                        .map(|()| {
                            "Closed the Slack thread; the session stays open here.".to_string()
                        }),
                    _ => Ok("This session is not in Slack.".to_string()),
                },
            };
            let resp = match result {
                Ok(message) => Response { ok: true, message },
                Err(e) => Response {
                    ok: false,
                    message: format!("{e:#}"),
                },
            };
            store::answer(&self.db, &task_id, &resp)?;
        }
        for spawn in store::spawns(&self.db)? {
            let thread = spawn.thread_ts.clone();
            let channel = spawn.channel.clone();
            // Errors here are transient (zellij, the database, Slack): keep
            // the start and retry next poll. Definitive outcomes (agent quit,
            // pane closed, timeout) are handled inside and drop the record.
            if let Err(e) = self.advance_spawn(spawn, now) {
                log(&format!("spawn {thread} in {channel}: {e:#}; retrying"));
                if let Some(mut s) = store::spawn_for_thread(&self.db, &thread)? {
                    s.last_error = Some(format!("{e:#}"));
                    store::put_spawn(&self.db, &s)?;
                }
            }
        }
        for link in store::links(&self.db)?.into_iter().filter(|l| !l.closed) {
            let task_id = link.task_id.clone();
            if let Err(e) = self.relay(link) {
                log(&format!("relay {task_id} failed: {e:#}"));
            }
        }
        Ok(())
    }

    // ── Slack helpers ─────────────────────────────────────────────────────

    pub(super) fn current_reply(&self, task: &Task) -> Option<transcript::Reply> {
        let path = crate::session::files::session_file_for(task)?;
        transcript::last_reply(&path, &task.agent_type)
            .ok()
            .flatten()
    }

    pub(super) fn post_reply(&self, link: &Link, text: &str) -> Result<()> {
        for chunk in slack::chunk_text(text, slack::MARKDOWN_LIMIT) {
            self.bot.post(
                &link.channel,
                Some(&link.thread_ts),
                &slack::preview(&chunk),
                &slack::markdown_blocks(&chunk),
            )?;
        }
        Ok(())
    }

    /// Post a top-level message (no thread to put it in).
    pub(super) fn say(&self, channel: &str, text: &str) {
        if let Err(e) = self
            .bot
            .post(channel, None, text, &slack::note_blocks(text))
        {
            log(&format!("could not post a message: {e:#}"));
        }
    }

    pub(super) fn note(&self, channel: &str, thread_ts: &str, text: &str) {
        if let Err(e) = self
            .bot
            .post(channel, Some(thread_ts), text, &slack::note_blocks(text))
        {
            log(&format!("could not post a note: {e:#}"));
        }
    }

    /// Best-effort agent-session status; disabled after Slack rejects the
    /// method (the app is not declared as an agent).
    pub(super) fn session_status(&mut self, channel: &str, thread_ts: &str, status: SessionStatus) {
        if !self.sessions_api {
            return;
        }
        if let Err(e) = self.bot.set_status(channel, thread_ts, status) {
            self.sessions_api_failed(&e);
        }
    }

    pub(super) fn session_title(&mut self, channel: &str, thread_ts: &str, title: &str) {
        if !self.sessions_api {
            return;
        }
        if let Err(e) = self.bot.rename(channel, thread_ts, title) {
            self.sessions_api_failed(&e);
        }
    }

    fn sessions_api_failed(&mut self, e: &anyhow::Error) {
        if sessions_api_unavailable(slack::error_code(e)) {
            self.sessions_api = false;
            log(&format!(
                "agent sessions unavailable ({e:#}); status and titles off"
            ));
        } else {
            log(&format!("agent session update failed: {e:#}"));
        }
    }
}

/// Slack error codes meaning the agent-session methods will never work for
/// this app (not declared as an agent, scope missing), as opposed to a
/// one-off failure.
fn sessions_api_unavailable(code: Option<&str>) -> bool {
    matches!(
        code,
        Some("unknown_method" | "not_allowed" | "missing_scope" | "not_an_agent" | "invalid_auth")
    )
}

const SLASH_HELP: &str =
    "`/nibble` opens the new-session form · `/nibble list` shows sessions in Slack.\n\
From a running agent, `/nibble-rc` sends its session here; `/nibble-rc stop` takes it back.";

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// What identifies an interaction across Slack's redeliveries: a modal's
/// view id for submissions (one form, one session), the envelope id for
/// other clicks and slash commands. Events are deduped by their event id.
fn redelivery_key(env: &Envelope) -> Option<String> {
    match env {
        Envelope::Interactive { payload, .. } if payload["type"] == "view_submission" => payload
            ["view"]["id"]
            .as_str()
            .map(|v| format!("view:{v}"))
            .or_else(|| env.envelope_id().map(|e| format!("env:{e}"))),
        Envelope::Interactive { .. } | Envelope::SlashCommand { .. } => {
            env.envelope_id().map(|e| format!("env:{e}"))
        }
        _ => None,
    }
}

/// Recently handled event and interaction ids, to drop Slack's
/// redeliveries (bounded to [`SEEN_CAP`]).
#[derive(Default)]
pub(super) struct Seen(VecDeque<String>);

impl Seen {
    /// True the first time `id` is seen (an empty id always counts as new).
    fn first_sight(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return true;
        }
        if self.0.iter().any(|s| s == id) {
            return false;
        }
        if self.0.len() >= SEEN_CAP {
            self.0.pop_front();
        }
        self.0.push_back(id.to_string());
        true
    }

    /// Drop a remembered id, so it is acted on again.
    fn forget(&mut self, id: &str) {
        self.0.retain(|s| s != id);
    }
}

/// A DM message the daemon acts on.
#[derive(Debug, PartialEq)]
enum Incoming {
    TopLevel {
        channel: String,
        ts: String,
        text: String,
    },
    Reply {
        channel: String,
        thread_ts: String,
        ts: String,
        text: String,
    },
}

/// Keep only plain messages from `user` in a DM with the bot (no bots,
/// edits, joins or other subtypes).
fn classify_message(ev: &Value, user: &str) -> Option<Incoming> {
    if ev["channel_type"] != "im" || ev["user"].as_str() != Some(user) {
        return None;
    }
    if ev.get("bot_id").is_some_and(|b| !b.is_null())
        || ev.get("subtype").is_some_and(|s| !s.is_null())
    {
        return None;
    }
    let channel = ev["channel"].as_str()?.to_string();
    let ts = ev["ts"].as_str()?.to_string();
    let text = ev["text"].as_str().unwrap_or_default().trim().to_string();
    if text.is_empty() {
        return None;
    }
    match ev["thread_ts"].as_str() {
        Some(thread) if thread != ts => Some(Incoming::Reply {
            channel,
            thread_ts: thread.to_string(),
            ts,
            text: unescape(&text),
        }),
        _ => Some(Incoming::TopLevel {
            channel,
            ts,
            text: unescape(&text),
        }),
    }
}

/// Undo Slack's escaping of `&`, `<` and `>` in message text.
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

pub(super) fn card_for(task: &Task, zellij: &str) -> CardInfo {
    let sandboxed = task.sandbox_type == SandboxType::Podman
        || task
            .context
            .as_ref()
            .is_some_and(|c| c.extra.contains_key("parent_task_id"));
    CardInfo {
        title: task.title.clone(),
        agent: crate::format::agent_display(&task.agent_type).1,
        location: if sandboxed { "sandbox" } else { "host" }.to_string(),
        cwd: task.context.as_ref().and_then(|c| c.project_path.clone()),
        zellij: zellij.to_string(),
    }
}

pub(super) fn session_card(link: &Link, topic: Option<&str>) -> SessionCard {
    SessionCard {
        task_id: link.task_id.clone(),
        title: link.card.title.clone(),
        agent: link.card.agent.clone(),
        location: link.card.location.clone(),
        cwd: link.card.cwd.clone(),
        zellij: link.card.zellij.clone(),
        topic: topic.map(slack::preview),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::task;
    use super::*;

    fn msg(extra: Value) -> Value {
        let mut v = json!({
            "type": "message", "channel_type": "im", "channel": "D1",
            "user": "U1", "ts": "2.0", "text": "run &lt;tests&gt; &amp; report"
        });
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    }

    #[test]
    fn classifies_dm_messages() {
        assert_eq!(
            classify_message(&msg(json!({})), "U1"),
            Some(Incoming::TopLevel {
                channel: "D1".into(),
                ts: "2.0".into(),
                text: "run <tests> & report".into()
            })
        );
        assert!(matches!(
            classify_message(&msg(json!({"thread_ts": "1.0"})), "U1"),
            Some(Incoming::Reply { ref thread_ts, .. }) if thread_ts == "1.0"
        ));
        // A thread's root message is not a reply to itself.
        assert!(matches!(
            classify_message(&msg(json!({"thread_ts": "2.0"})), "U1"),
            Some(Incoming::TopLevel { .. })
        ));
    }

    #[test]
    fn ignores_strangers_bots_subtypes_and_channels() {
        assert!(classify_message(&msg(json!({})), "U2").is_none());
        assert!(classify_message(&msg(json!({"bot_id": "B1"})), "U1").is_none());
        assert!(classify_message(&msg(json!({"subtype": "message_changed"})), "U1").is_none());
        assert!(classify_message(&msg(json!({"channel_type": "channel"})), "U1").is_none());
        assert!(classify_message(&msg(json!({"text": "  "})), "U1").is_none());
    }

    #[test]
    fn seen_ids_dedupe_forget_and_stay_bounded() {
        let mut seen = Seen::default();
        assert!(seen.first_sight("a"));
        assert!(!seen.first_sight("a"));
        seen.forget("a");
        assert!(
            seen.first_sight("a"),
            "a rejected form's resubmission is handled"
        );
        assert!(seen.first_sight("") && seen.first_sight(""));
        for i in 0..SEEN_CAP {
            seen.first_sight(&i.to_string());
        }
        assert!(seen.first_sight("a"), "the oldest id was evicted");
        assert_eq!(seen.0.len(), SEEN_CAP);
    }

    #[test]
    fn redeliveries_share_a_key() {
        let submit = |env: &str| Envelope::Interactive {
            envelope_id: env.into(),
            payload: json!({"type": "view_submission", "view": {"id": "V1"}}),
        };
        assert_eq!(redelivery_key(&submit("e1")), redelivery_key(&submit("e2")));
        let click = Envelope::Interactive {
            envelope_id: "e3".into(),
            payload: json!({"type": "block_actions"}),
        };
        assert_eq!(redelivery_key(&click).as_deref(), Some("env:e3"));
        let event = Envelope::Event {
            envelope_id: "e4".into(),
            payload: json!({}),
        };
        assert_eq!(redelivery_key(&event), None);
    }

    #[test]
    fn only_permanent_errors_turn_agent_sessions_off() {
        assert!(sessions_api_unavailable(Some("missing_scope")));
        assert!(sessions_api_unavailable(Some("unknown_method")));
        assert!(!sessions_api_unavailable(Some("ratelimited")));
        assert!(!sessions_api_unavailable(None));
    }

    #[test]
    fn card_marks_sandbox_windows() {
        let t = task("w", 1, "main", Utc::now(), true);
        assert_eq!(card_for(&t, "main").location, "sandbox");
        let h = task("h", 1, "main", Utc::now(), false);
        assert_eq!(card_for(&h, "main").location, "host");
    }
}
