//! `nibble rc daemon`: the Slack side of remote control.
//!
//! The socket loop polls nibble's state; normal outbound posts are retained in
//! a durable queue and delivered by a separate worker, so slow Slack requests
//! and rate limits cannot block reading/acknowledging socket frames:
//!
//! - **Requests** from `/nibble-rc` (host or sandbox) link a session to a
//!   new DM thread, or close the link.
//! - **Pending spawns** (sessions started from Slack) wait for their agent to
//!   register in its new zellij pane, then receive the first message.
//! - **Links** relay status: working → Slack session `processing`, a
//!   permission prompt → a note plus `suspended`, each text a running turn
//!   moves on from → a progress note, a finished turn → the agent's reply
//!   in the thread plus `active`.
//!
//! Inbound, a reply in a session thread is typed into the agent's pane;
//! Slack's stop button presses Escape there; the Close button ends the
//! link (the zellij tab stays). Only the configured Slack user is heard.

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::modal::{self, NewSession, Origin};
use super::secrets::Secrets;
use super::slack::{self, CardState, SessionCard, SessionStatus, Slack};
use super::socket::{self, Envelope, Read};
use super::store::{self, CardInfo, Link, RequestAction, Response};
use super::{delivery, pane, relay, transcript};
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
/// Lock file (in the private state dir) held by the running daemon.
const DAEMON_LOCK: &str = "rc-daemon";
/// Remembered event ids, to drop Slack's redeliveries.
const SEEN_CAP: usize = 256;

/// Run the daemon until killed.
pub(crate) fn run(cfg: RcConfig, db_path: PathBuf) -> Result<()> {
    // One daemon per machine: a second would relay the same turns and
    // answer the same requests, posting everything twice.
    let state = store::open_state()?;
    let _lock = crate::lock::try_lock_in(&store::state_dir(), DAEMON_LOCK)?.with_context(|| {
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
        secrets: secrets.clone(),
        bot,
        dm,
        db,
        state,
        sessions_api: true,
        seen: Seen::default(),
        last_beat: None,
        repos_cache: None,
        transcript_lens: HashMap::new(),
        delivery: Some(delivery::Worker::start(secrets.bot_token.clone())?),
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
    /// Links and pending starts: the daemon's private state (see `store`).
    pub(super) state: store::StateDb,
    /// False once Slack rejected the agent-session methods (app not
    /// declared as an agent): they are skipped from then on.
    pub(super) sessions_api: bool,
    pub(super) seen: Seen,
    pub(super) last_beat: Option<Instant>,
    pub(super) repos_cache: Option<(Instant, Vec<PathBuf>)>,
    /// Each linked task's transcript size when it was last read, so a turn
    /// that wrote nothing new is not read again.
    pub(super) transcript_lens: HashMap<String, u64>,
    pub(super) delivery: Option<delivery::Worker>,
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
        self.dispatch_with(&mut |frame| conn.send(frame), env)
    }

    fn dispatch_with(
        &mut self,
        send: &mut impl FnMut(String) -> Result<()>,
        env: Envelope,
    ) -> Result<()> {
        let id = env.envelope_id().unwrap_or_default().to_string();
        // Slack redelivers what it did not see acknowledged in 3 seconds
        // (a long poll pass can delay the read): act once, re-ack the rest.
        let key = redelivery_key(&env);
        if let Some(key) = &key {
            if !self.first_sight(key) {
                if !id.is_empty() {
                    send(socket::ack_frame(&id, None))?;
                }
                return Ok(());
            }
        }
        let (ack, after) = match env {
            Envelope::Event { payload, .. } => {
                send(socket::ack_frame(&id, None))?;
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
            if let Err(e) = send(socket::ack_frame(&id, ack.as_ref())) {
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
                self.queue(&[Self::message(
                    &channel,
                    Some(&ts),
                    "Start a session for this?",
                    &blocks,
                )])
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
        if let Some(mut spawn) = store::spawn_for_thread(&self.state, thread_ts)? {
            spawn.queued.push(text.to_string());
            store::put_spawn(&self.state, &spawn)?;
            self.queue_reaction(channel, ts, "hourglass_flowing_sand");
            return Ok(());
        }
        let Some(mut link) = store::link_by_thread(&self.state, channel, thread_ts)? else {
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
        self.queue_reaction(channel, ts, "eyes");
        link.watch = relay::after_reply(link.watch);
        link.idle_checks = 0;
        store::put_link(&self.state, &link)?;
        self.session_status(channel, thread_ts, SessionStatus::Processing);
        Ok(())
    }

    fn on_stop(&mut self, channel: &str, thread_ts: &str) -> Result<()> {
        let Some(link) = store::link_by_thread(&self.state, channel, thread_ts)? else {
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
            Some(slack::CLOSE_ACTION) => action["value"].as_str().map_or(Ok(()), |task| {
                self.close_link(
                    task,
                    "Disconnected from Slack. The session keeps running in zellij.",
                )
            }),
            Some(slack::END_ACTION) => action["value"]
                .as_str()
                .map_or(Ok(()), |task| self.end_link(task)),
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
        let links = store::links(&self.state).unwrap_or_default();
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
        if let Some(link) = store::link(&self.state, task_id)?.filter(|l| !l.closed) {
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
            &slack::session_card_blocks(&session_card, CardState::Open { endable: false }),
        )?;
        link.permalink = self.bot.permalink(&self.dm, &link.thread_ts).ok();
        // The card exists now: record the link before anything else can
        // fail, so its thread and Close button always have a session.
        let (cursor, shown) = relay::start_point(self.current_turn(&task), link.watch);
        link.last_reply_id = cursor;
        let deliveries = shown
            .map(|reply| {
                Self::reply_deliveries(&link, &format!("*Last reply*\n\n{}", reply.text), false)
            })
            .unwrap_or_default();
        store::save_relay(&self.state, &link, &deliveries)?;
        self.wake_delivery();
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

    /// Stop relaying a session; it keeps running in zellij.
    pub(super) fn close_link(&mut self, task_id: &str, why: &str) -> Result<()> {
        match store::link(&self.state, task_id)? {
            Some(link) if !link.closed => self.finish_link(link, why, CardState::Disconnected),
            _ => Ok(()),
        }
    }

    /// End a session started from Slack: close the zellij tab the daemon
    /// opened for it (which stops the agent), then the thread.
    fn end_link(&mut self, task_id: &str) -> Result<()> {
        let Some(link) = store::link(&self.state, task_id)? else {
            return Ok(());
        };
        match may_end(&link) {
            EndCheck::AlreadyClosed => return Ok(()),
            EndCheck::Allowed => {}
            EndCheck::NotStartedFromSlack => {
                self.note(
                    &link.channel,
                    &link.thread_ts,
                    "Only sessions started from Slack can be ended here; this one is left running.",
                );
                return Ok(());
            }
            EndCheck::InSandbox => {
                self.note(
                    &link.channel,
                    &link.thread_ts,
                    "Sandbox sessions cannot be ended from Slack yet: closing the tab would \
                     leave the agent running in its container. This one is left running.",
                );
                return Ok(());
            }
        }
        let target = self
            .db
            .get_task_by_id(task_id)?
            .as_ref()
            .and_then(pane_input::target);
        if let Some((session, pane)) = target {
            if let Err(e) = pane::end_session(&session, pane) {
                self.note(
                    &link.channel,
                    &link.thread_ts,
                    &format!("Could not close the session's zellij tab: {e:#}"),
                );
                return Ok(());
            }
        }
        self.finish_link(
            link,
            "Ended from Slack: the agent stopped and its zellij tab was closed. \
             Its conversation stays in the agent's history (`claude --resume`, or \
             pi's session list).",
            CardState::Ended,
        )
    }

    fn finish_link(&mut self, mut link: Link, why: &str, state: CardState) -> Result<()> {
        link.closed = true;
        let card = session_card(&link, None);
        let text = slack::session_card_text(&card);
        let blocks = slack::session_card_blocks(&card, state).to_string();
        let mut deliveries = vec![store::Delivery::new(
            &link.channel,
            "chat.update",
            &[
                ("channel", &link.channel),
                ("ts", link.card_message()),
                ("text", &text),
                ("blocks", &blocks),
            ],
        )];
        deliveries.push(Self::message(
            &link.channel,
            Some(&link.thread_ts),
            why,
            &slack::note_blocks(why),
        ));
        if self.sessions_api {
            deliveries.push(Self::status_delivery(
                &link.channel,
                &link.thread_ts,
                SessionStatus::Closed,
            ));
        }
        store::save_relay(&self.state, &link, &deliveries)?;
        self.transcript_lens.remove(&link.task_id);
        self.wake_delivery();
        Ok(())
    }

    // ── Poll pass ─────────────────────────────────────────────────────────

    fn poll_pass(&mut self) -> Result<()> {
        if self
            .delivery
            .as_ref()
            .is_some_and(delivery::Worker::finished)
        {
            log("delivery worker stopped; restarting it");
            self.delivery = Some(delivery::Worker::start(self.secrets.bot_token.clone())?);
        }
        self.sessions_api = !store::sessions_disabled(&self.state)?;
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
                RequestAction::Stop => match store::link(&self.state, &task_id)? {
                    Some(l) if !l.closed => self
                        .close_link(
                            &task_id,
                            "Disconnected from the terminal. The session keeps running in zellij.",
                        )
                        .map(|()| {
                            "Disconnected the Slack thread; the session stays open here."
                                .to_string()
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
        for spawn in store::spawns(&self.state)? {
            let thread = spawn.thread_ts.clone();
            let channel = spawn.channel.clone();
            // Errors here are transient (zellij, the database, Slack): keep
            // the start and retry next poll. Definitive outcomes (agent quit,
            // pane closed, timeout) are handled inside and drop the record.
            if let Err(e) = self.advance_spawn(spawn, now) {
                log(&format!("spawn {thread} in {channel}: {e:#}; retrying"));
                if let Some(mut s) = store::spawn_for_thread(&self.state, &thread)? {
                    s.last_error = Some(format!("{e:#}"));
                    store::put_spawn(&self.state, &s)?;
                }
            }
        }
        for link in store::links(&self.state)?.into_iter().filter(|l| !l.closed) {
            let task_id = link.task_id.clone();
            if let Err(e) = self.relay(link) {
                log(&format!("relay {task_id} failed: {e:#}"));
            }
        }
        Ok(())
    }

    // ── Slack helpers ─────────────────────────────────────────────────────

    /// The task's current turn; empty when its transcript can't be read.
    pub(super) fn current_turn(&self, task: &Task) -> transcript::Turn {
        crate::session::files::session_file_for(task)
            .and_then(|path| transcript::turn(&path, &task.agent_type).ok())
            .unwrap_or_default()
    }

    pub(super) fn wake_delivery(&self) {
        if let Some(worker) = &self.delivery {
            worker.wake();
        }
    }

    pub(super) fn queue(&self, deliveries: &[store::Delivery]) -> Result<()> {
        store::enqueue(&self.state, deliveries)?;
        self.wake_delivery();
        Ok(())
    }

    pub(super) fn message(
        channel: &str,
        thread: Option<&str>,
        text: &str,
        blocks: &Value,
    ) -> store::Delivery {
        let blocks = blocks.to_string();
        let mut params = vec![("channel", channel), ("text", text), ("blocks", &blocks)];
        if let Some(ts) = thread {
            params.push(("thread_ts", ts));
        }
        store::Delivery::new(channel, "chat.postMessage", &params)
    }

    pub(super) fn reply_deliveries(
        link: &Link,
        text: &str,
        progress: bool,
    ) -> Vec<store::Delivery> {
        if progress {
            if let Some(blocks) = slack::progress_blocks(text) {
                return vec![Self::message(
                    &link.channel,
                    Some(&link.thread_ts),
                    &slack::preview(text),
                    &blocks,
                )];
            }
        }
        slack::chunk_text(text, slack::MARKDOWN_LIMIT)
            .into_iter()
            .map(|chunk| {
                Self::message(
                    &link.channel,
                    Some(&link.thread_ts),
                    &slack::preview(&chunk),
                    &slack::markdown_blocks(&chunk),
                )
            })
            .collect()
    }

    pub(super) fn say(&self, channel: &str, text: &str) {
        if let Err(e) = self.queue(&[Self::message(
            channel,
            None,
            text,
            &slack::note_blocks(text),
        )]) {
            log(&format!("could not queue a message: {e:#}"));
        }
    }

    pub(super) fn note(&self, channel: &str, thread_ts: &str, text: &str) {
        if let Err(e) = self.queue(&[Self::message(
            channel,
            Some(thread_ts),
            text,
            &slack::note_blocks(text),
        )]) {
            log(&format!("could not queue a note: {e:#}"));
        }
    }

    fn queue_reaction(&self, channel: &str, ts: &str, emoji: &str) {
        if let Err(e) = self.queue(&[store::Delivery::new(
            channel,
            "reactions.add",
            &[("channel", channel), ("timestamp", ts), ("name", emoji)],
        )]) {
            log(&format!("could not queue reaction: {e:#}"));
        }
    }

    pub(super) fn status_delivery(
        channel: &str,
        thread_ts: &str,
        status: SessionStatus,
    ) -> store::Delivery {
        store::Delivery::new(
            channel,
            "agents.sessions.setStatus",
            &[
                ("channel_id", channel),
                ("thread_ts", thread_ts),
                ("status", status.as_str()),
            ],
        )
    }

    pub(super) fn session_status(&mut self, channel: &str, thread_ts: &str, status: SessionStatus) {
        if self.sessions_api {
            if let Err(e) = self.queue(&[Self::status_delivery(channel, thread_ts, status)]) {
                log(&format!("could not queue session status: {e:#}"));
            }
        }
    }

    pub(super) fn session_title(&mut self, channel: &str, thread_ts: &str, title: &str) {
        if self.sessions_api {
            if let Err(e) = self.queue(&[store::Delivery::new(
                channel,
                "agents.sessions.rename",
                &[
                    ("channel_id", channel),
                    ("thread_ts", thread_ts),
                    ("title", title),
                ],
            )]) {
                log(&format!("could not queue session title: {e:#}"));
            }
        }
    }
}

/// Slack error codes meaning the agent-session methods will never work for
/// this app (not declared as an agent, scope missing), as opposed to a
/// one-off failure.
pub(super) fn sessions_api_unavailable(code: Option<&str>) -> bool {
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

/// Whether End session may act on a link.
#[derive(Debug, PartialEq, Eq)]
enum EndCheck {
    Allowed,
    AlreadyClosed,
    /// Sent with `/nibble-rc` from a session you opened: never closed from
    /// Slack, whatever the button press claims.
    NotStartedFromSlack,
    /// Closing a sandbox session's tab ends only its `podman exec` client;
    /// the agent keeps running in the container.
    InSandbox,
}

fn may_end(link: &Link) -> EndCheck {
    if link.closed {
        EndCheck::AlreadyClosed
    } else if !link.from_slack {
        EndCheck::NotStartedFromSlack
    } else if !endable_location(link) {
        EndCheck::InSandbox
    } else {
        EndCheck::Allowed
    }
}

/// Whether ending the session's tab stops its agent: true on the host.
pub(super) fn endable_location(link: &Link) -> bool {
    link.card.location == "host"
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

    #[test]
    fn output_burst_and_slow_http_do_not_delay_socket_acknowledgements() {
        use std::sync::mpsc;
        let dir = tempfile::tempdir().unwrap();
        let state = store::open_state_in(&dir.path().join("private")).unwrap();
        let worker_state = store::open_state_in(&dir.path().join("private")).unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let bot = Slack::for_test(format!("http://{}", server.server_addr()));
        let burst: Vec<_> = (0..100)
            .map(|_| store::Delivery::new("D1", "chat.postMessage", &[("text", "progress")]))
            .collect();
        store::enqueue(&state, &burst).unwrap();
        let (release, wait) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let pending = store::deliveries(&worker_state).unwrap();
            let params: Vec<_> = pending[0]
                .params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            bot.call("chat.postMessage", &params).unwrap();
            wait.recv().unwrap();
        });
        let stalled = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let mut daemon = Daemon {
            cfg: RcConfig::default(),
            secrets: Secrets {
                bot_token: String::new(),
                app_token: String::new(),
                user_id: "U1".into(),
            },
            bot: Slack::for_test("http://127.0.0.1:1".into()),
            dm: "D1".into(),
            db: Database::open(dir.path().join("tasks.db")).unwrap(),
            state,
            sessions_api: true,
            seen: Seen::default(),
            last_beat: None,
            repos_cache: Some((Instant::now(), vec![])),
            transcript_lens: HashMap::new(),
            delivery: None,
        };
        let closing = crate::rc::test_support::link("closing", "9.9");
        store::put_link(&daemon.state, &closing).unwrap();
        daemon
            .finish_link(closing, "closed", CardState::Disconnected)
            .unwrap();
        assert!(
            store::link(&daemon.state, "closing")
                .unwrap()
                .unwrap()
                .closed
        );
        assert_eq!(store::deliveries(&daemon.state).unwrap().len(), 103);
        let mut frames = Vec::new();
        daemon.dispatch_with(&mut |frame| { frames.push(frame); Ok(()) }, Envelope::Event {
            envelope_id: "ack-now".into(), payload: json!({"event_id": "e1", "event": {"type": "message", "channel_type": "im", "channel": "D1", "user": "U1", "ts": "1.0", "text": "new session"}}),
        }).unwrap();
        assert_eq!(frames, vec![socket::ack_frame("ack-now", None)]);
        assert_eq!(store::deliveries(&daemon.state).unwrap().len(), 104);
        // The HTTP request was still waiting throughout dispatch. No timing
        // threshold or real Slack/Zellij session is needed to prove isolation.
        stalled
            .respond(tiny_http::Response::from_string("{\"ok\":true}"))
            .unwrap();
        release.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn only_open_sessions_started_from_slack_can_be_ended() {
        let mut l = super::super::test_support::link("t1", "1.1");
        assert_eq!(may_end(&l), EndCheck::NotStartedFromSlack);
        l.from_slack = true;
        assert_eq!(l.card.location, "host");
        assert_eq!(may_end(&l), EndCheck::Allowed);
        l.card.location = "sandbox".into();
        assert_eq!(may_end(&l), EndCheck::InSandbox);
        l.card.location = "host".into();
        l.closed = true;
        assert_eq!(may_end(&l), EndCheck::AlreadyClosed);
    }

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
