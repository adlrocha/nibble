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
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::modal::{self, NewSession, Origin};
use super::secrets::Secrets;
use super::slack::{self, SessionCard, SessionStatus, Slack};
use super::socket::{self, Envelope, Read};
use super::store::{
    self, CardInfo, Link, Location, PendingSpawn, RequestAction, Response, SpawnAgent, Watch,
};
use super::{pane, transcript};
use crate::config::RcConfig;
use crate::db::Database;
use crate::models::{AgentType, SandboxType, Task, TaskStatus};

/// Reconnect when Slack has been silent this long (it pings far more often).
const SILENCE_LIMIT: Duration = Duration::from_secs(180);
/// Longest wait between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How often the heartbeat is written.
const BEAT_EVERY: Duration = Duration::from_secs(30);
/// Polls an idle task may show no new reply before the turn is considered
/// over without one (the transcript can lag the idle report; and a message
/// that never started a turn must not wait forever).
const IDLE_GIVE_UP: u32 = 20;
/// Grace period after an agent reports its session before the first
/// message is typed, so its TUI is reading input.
const READY_GRACE_SECS: i64 = 2;
/// Remembered event ids, to drop Slack's redeliveries.
const SEEN_CAP: usize = 256;

/// Run the daemon until killed.
pub(crate) fn run(cfg: RcConfig, db_path: PathBuf) -> Result<()> {
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
        seen: VecDeque::new(),
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

fn log(msg: &str) {
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

/// How long the repo list for the form is reused. Opening a form must beat
/// Slack's 3-second trigger expiry, so the directory walk is not redone on
/// every click.
const REPOS_TTL: Duration = Duration::from_secs(300);

struct Daemon {
    cfg: RcConfig,
    secrets: Secrets,
    bot: Slack,
    dm: String,
    db: Database,
    /// False once Slack rejected the agent-session methods (app not
    /// declared as an agent): they are skipped from then on.
    sessions_api: bool,
    seen: VecDeque<String>,
    last_beat: Option<Instant>,
    repos_cache: Option<(Instant, Vec<PathBuf>)>,
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
            _ => (None, After::Nothing),
        };
        if !id.is_empty() {
            conn.send(socket::ack_frame(&id, ack.as_ref()))?;
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

    /// True the first time an event id is seen.
    fn first_sight(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return true;
        }
        if self.seen.iter().any(|s| s == id) {
            return false;
        }
        if self.seen.len() >= SEEN_CAP {
            self.seen.pop_front();
        }
        self.seen.push_back(id.to_string());
        true
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
        let target = task.as_ref().and_then(pane_target);
        let Some((session, pane_id)) = target else {
            self.note(
                channel,
                thread_ts,
                ":warning: nibble does not know this session's zellij pane any more.",
            );
            return Ok(());
        };
        if !pane::pane_alive(&session, pane_id)? {
            self.note(
                channel,
                thread_ts,
                &format!(":warning: The session's pane is gone (zellij `{session}`). Reopen it and send `/nibble-rc` again."),
            );
            return Ok(());
        }
        pane::send_text(&session, pane_id, text)?;
        let _ = self.bot.react(channel, ts, "eyes");
        // Expect a turn: a fast one may finish between two polls.
        link.watch = Some(Watch::Working);
        link.idle_checks = 0;
        store::put_link(&self.db, &link)?;
        self.session_status(channel, thread_ts, SessionStatus::Processing);
        Ok(())
    }

    fn on_stop(&mut self, channel: &str, thread_ts: &str) -> Result<()> {
        let Some(link) = store::link_by_thread(&self.db, channel, thread_ts)? else {
            return Ok(());
        };
        if let Some((session, pane_id)) = self
            .db
            .get_task_by_id(&link.task_id)?
            .as_ref()
            .and_then(pane_target)
        {
            pane::send_escape(&session, pane_id)?;
            self.note(channel, thread_ts, ":black_square_for_stop: Interrupted.");
        }
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

    /// The repo choices for the form, cached for [`REPOS_TTL`].
    fn repos(&mut self) -> Result<Vec<PathBuf>> {
        if let Some((at, repos)) = &self.repos_cache {
            if at.elapsed() < REPOS_TTL {
                return Ok(repos.clone());
            }
        }
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

    // ── Starting sessions from Slack ──────────────────────────────────────

    fn start_session(&mut self, new: &NewSession) -> Result<()> {
        let channel = new.origin.channel.clone();
        let thread_ts = match &new.origin.thread_ts {
            Some(ts) => ts.clone(),
            None => {
                let blocks = slack::markdown_blocks(&new.prompt);
                self.bot
                    .post(&channel, None, &slack::preview(&new.prompt), &blocks)?
            }
        };
        let cwd = new.cwd.to_string_lossy().to_string();
        self.note(
            &channel,
            &thread_ts,
            &format!(
                ":hourglass_flowing_sand: Starting {} on {} in `{}` (zellij `{}`)…",
                new.agent.as_str(),
                new.location.as_str(),
                slack::escape(&cwd),
                self.cfg.zellij_session
            ),
        );
        let session = self.cfg.zellij_session.clone();
        crate::mux::ensure_background_session(&self.db, &session)?;
        let script = launch_script(
            new.location,
            new.agent,
            &new.cwd,
            &std::env::current_exe().context("cannot locate nibble binary")?,
            &crate::mux::wrappers_dir()?,
            &std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
        );
        let tab_name = tab_name_for(&new.cwd);
        let pane_id = pane::open_tab(&session, &tab_name, &cwd, &script)?;
        store::put_spawn(
            &self.db,
            &PendingSpawn {
                channel,
                thread_ts,
                location: new.location,
                agent: new.agent,
                cwd,
                prompt: new.prompt.clone(),
                zellij_session: session,
                pane_id,
                tab_name,
                started_at: Utc::now(),
                task_id: None,
                ready_at: None,
                queued: Vec::new(),
            },
        )
    }

    /// Advance one pending spawn: find its agent, wait for its TUI, hand it
    /// the first message and link the thread.
    fn advance_spawn(&mut self, mut spawn: PendingSpawn, now: DateTime<Utc>) -> Result<()> {
        let timeout =
            chrono::Duration::seconds(i64::try_from(self.cfg.start_timeout_secs).unwrap_or(240));
        if now - spawn.started_at > timeout {
            store::delete_spawn(&self.db, &spawn.thread_ts)?;
            self.note(
                &spawn.channel,
                &spawn.thread_ts,
                &format!(
                    ":warning: The agent did not come up within {}s. Look at tab `{}` in zellij `{}`.",
                    self.cfg.start_timeout_secs, spawn.tab_name, spawn.zellij_session
                ),
            );
            return Ok(());
        }
        if spawn.task_id.is_none() {
            let tasks = self.db.list_tasks()?;
            spawn.task_id = find_spawned_task(&tasks, &spawn).map(|t| t.task_id.clone());
            if spawn.task_id.is_none() {
                if !pane::pane_alive(&spawn.zellij_session, spawn.pane_id).unwrap_or(true) {
                    store::delete_spawn(&self.db, &spawn.thread_ts)?;
                    self.note(
                        &spawn.channel,
                        &spawn.thread_ts,
                        ":warning: The agent's pane closed before it started.",
                    );
                    return Ok(());
                }
                return store::put_spawn(&self.db, &spawn);
            }
        }
        let task_id = spawn.task_id.clone().unwrap_or_default();
        let Some(task) = self.db.get_task_by_id(&task_id)? else {
            spawn.task_id = None;
            return store::put_spawn(&self.db, &spawn);
        };
        if spawn.ready_at.is_none() {
            if !session_known(&task) {
                return store::put_spawn(&self.db, &spawn);
            }
            spawn.ready_at = Some(now);
            return store::put_spawn(&self.db, &spawn);
        }
        if spawn
            .ready_at
            .is_some_and(|r| (now - r).num_seconds() < READY_GRACE_SECS)
        {
            return Ok(());
        }
        // Ready: link first, so replies arriving meanwhile find the link.
        let card = card_for(&task, &spawn.zellij_session);
        let link = Link {
            task_id: task.task_id.clone(),
            channel: spawn.channel.clone(),
            thread_ts: spawn.thread_ts.clone(),
            created_at: now,
            closed: false,
            watch: Some(Watch::Working),
            last_reply_id: self.current_reply(&task).map(|r| r.id),
            idle_checks: 0,
            permalink: self.bot.permalink(&spawn.channel, &spawn.thread_ts).ok(),
            card,
        };
        store::put_link(&self.db, &link)?;
        store::delete_spawn(&self.db, &spawn.thread_ts)?;
        for msg in std::iter::once(&spawn.prompt).chain(spawn.queued.iter()) {
            pane::send_text(&spawn.zellij_session, spawn.pane_id, msg)?;
        }
        let session_card = session_card(&link, Some(&spawn.prompt));
        self.bot.post(
            &link.channel,
            Some(&link.thread_ts),
            &slack::session_card_text(&session_card),
            &slack::session_card_blocks(&session_card, true),
        )?;
        self.session_status(&link.channel, &link.thread_ts, SessionStatus::Processing);
        self.session_title(
            &link.channel,
            &link.thread_ts,
            &slack::preview(&spawn.prompt),
        );
        Ok(())
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
        let Some((session, _)) = pane_target(&task) else {
            anyhow::bail!(
                "this session's zellij pane is unknown to nibble; run the agent inside zellij"
            );
        };
        let card = card_for(&task, &session);
        let topic = crate::status::topic_of(&task).map(str::to_string);
        let mut link = Link {
            task_id: task.task_id.clone(),
            channel: self.dm.clone(),
            thread_ts: String::new(),
            created_at: Utc::now(),
            closed: false,
            watch: Some(observe(&task)),
            last_reply_id: None,
            idle_checks: 0,
            permalink: None,
            card,
        };
        let session_card = session_card(&link, topic.as_deref());
        link.thread_ts = self.bot.post(
            &self.dm,
            None,
            &slack::session_card_text(&session_card),
            &slack::session_card_blocks(&session_card, true),
        )?;
        link.permalink = self.bot.permalink(&self.dm, &link.thread_ts).ok();
        if let Some(reply) = self.current_reply(&task) {
            self.post_reply(&link, &format!("*Last reply*\n\n{}", reply.text))?;
            link.last_reply_id = Some(reply.id);
        }
        store::put_link(&self.db, &link)?;
        let status = match link.watch {
            Some(Watch::Working) => SessionStatus::Processing,
            Some(Watch::Blocked) => SessionStatus::Suspended,
            _ => SessionStatus::Active,
        };
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

    fn close_link(&mut self, task_id: &str, why: &str) -> Result<()> {
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
            &link.thread_ts,
            &slack::session_card_text(&card),
            &slack::session_card_blocks(&card, false),
        ) {
            log(&format!("could not update the root message: {e:#}"));
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
            if let Err(e) = self.advance_spawn(spawn, now) {
                log(&format!("spawn {thread} failed: {e:#}"));
                store::delete_spawn(&self.db, &thread)?;
                self.note(
                    &channel,
                    &thread,
                    &format!(":warning: Could not start the session: {e:#}"),
                );
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

    fn relay(&mut self, mut link: Link) -> Result<()> {
        let Some(task) = self.db.get_task_by_id(&link.task_id)? else {
            return self.close_link(&link.task_id, "The session is gone from nibble.");
        };
        let observed = observe(&task);
        let step = next_step(&link, observed, &|| self.current_reply(&task));
        for effect in &step.effects {
            match effect {
                Effect::Status(s) => self.session_status(&link.channel, &link.thread_ts, *s),
                Effect::Blocked => {
                    let reason = task.attention_reason.as_deref().unwrap_or("Needs input");
                    self.note(
                        &link.channel,
                        &link.thread_ts,
                        &format!(
                            ":raised_hand: *{}*. Reply here to answer (a number picks a menu option), or answer at the terminal.",
                            slack::escape(reason)
                        ),
                    );
                }
                Effect::Reply(reply) => self.post_reply(&link, &reply.text)?,
                Effect::NoReply => self.note(
                    &link.channel,
                    &link.thread_ts,
                    "The turn ended without a text reply (check the pane for details).",
                ),
                Effect::Close => {
                    return self.close_link(&link.task_id, "The agent exited.");
                }
            }
        }
        if step.changed(&link) {
            link.watch = Some(step.watch);
            link.idle_checks = step.idle_checks;
            if let Some(id) = step.reply_id {
                link.last_reply_id = Some(id);
            }
            // Re-read before writing: a reply may have closed the link meanwhile.
            if store::link(&self.db, &link.task_id)?.is_some_and(|l| !l.closed) {
                store::put_link(&self.db, &link)?;
            }
        }
        Ok(())
    }

    // ── Slack helpers ─────────────────────────────────────────────────────

    fn current_reply(&self, task: &Task) -> Option<transcript::Reply> {
        let path = crate::quota_watch::session_file_for(task)?;
        transcript::last_reply(&path, &task.agent_type)
            .ok()
            .flatten()
    }

    fn post_reply(&self, link: &Link, text: &str) -> Result<()> {
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
    fn say(&self, channel: &str, text: &str) {
        if let Err(e) = self
            .bot
            .post(channel, None, text, &slack::note_blocks(text))
        {
            log(&format!("could not post a message: {e:#}"));
        }
    }

    fn note(&self, channel: &str, thread_ts: &str, text: &str) {
        if let Err(e) = self
            .bot
            .post(channel, Some(thread_ts), text, &slack::note_blocks(text))
        {
            log(&format!("could not post a note: {e:#}"));
        }
    }

    /// Best-effort agent-session status; disabled after Slack rejects the
    /// method (the app is not declared as an agent).
    fn session_status(&mut self, channel: &str, thread_ts: &str, status: SessionStatus) {
        if !self.sessions_api {
            return;
        }
        if let Err(e) = self.bot.set_status(channel, thread_ts, status) {
            self.sessions_api_failed(&e);
        }
    }

    fn session_title(&mut self, channel: &str, thread_ts: &str, title: &str) {
        if !self.sessions_api {
            return;
        }
        if let Err(e) = self.bot.rename(channel, thread_ts, title) {
            self.sessions_api_failed(&e);
        }
    }

    fn sessions_api_failed(&mut self, e: &anyhow::Error) {
        let msg = e.to_string();
        if [
            "unknown_method",
            "not_allowed",
            "missing_scope",
            "not_an_agent",
            "invalid_auth",
        ]
        .iter()
        .any(|code| msg.contains(code))
        {
            self.sessions_api = false;
            log(&format!(
                "agent sessions unavailable ({msg}); status and titles off"
            ));
        } else {
            log(&format!("agent session update failed: {msg}"));
        }
    }
}

const SLASH_HELP: &str =
    "`/nibble` opens the new-session form · `/nibble list` shows sessions in Slack.\n\
From a running agent, `/nibble-rc` sends its session here; `/nibble-rc stop` takes it back.";

// ── Pure helpers ──────────────────────────────────────────────────────────────

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

/// The zellij session and pane hosting a task's agent.
fn pane_target(task: &Task) -> Option<(String, u32)> {
    let session = crate::status::session_of(task)?.to_string();
    let pane = crate::status::pane_id_of(task)?;
    Some((session, pane))
}

/// The task's state as the relay sees it.
fn observe(task: &Task) -> Watch {
    // The reported status as is: the sidebar's "stale running looks idle"
    // display rule would make a long tool run look finished and lose the
    // reply that follows it.
    match task.status {
        TaskStatus::Exited => Watch::Exited,
        _ if task.attention_reason.is_some() => Watch::Blocked,
        TaskStatus::Running => Watch::Working,
        TaskStatus::Completed => Watch::Idle,
    }
}

#[derive(Debug, PartialEq)]
enum Effect {
    Status(SessionStatus),
    Blocked,
    Reply(transcript::Reply),
    /// The turn ended without any text to relay.
    NoReply,
    Close,
}

#[derive(Debug, PartialEq)]
struct Step {
    watch: Watch,
    idle_checks: u32,
    reply_id: Option<String>,
    effects: Vec<Effect>,
}

impl Step {
    fn changed(&self, link: &Link) -> bool {
        link.watch != Some(self.watch)
            || link.idle_checks != self.idle_checks
            || self.reply_id.is_some()
    }
}

/// What to relay for a link whose task is now `observed`. The transcript is
/// only read when a turn may have just finished.
fn next_step(link: &Link, observed: Watch, reply: &dyn Fn() -> Option<transcript::Reply>) -> Step {
    let prev = link.watch;
    let same = |effects| Step {
        watch: observed,
        idle_checks: 0,
        reply_id: None,
        effects,
    };
    match observed {
        Watch::Exited => same(vec![Effect::Close]),
        Watch::Working if prev == Some(Watch::Working) => Step {
            watch: Watch::Working,
            idle_checks: 0,
            reply_id: None,
            effects: vec![],
        },
        Watch::Working => same(vec![Effect::Status(SessionStatus::Processing)]),
        Watch::Blocked if prev == Some(Watch::Blocked) => same(vec![]),
        Watch::Blocked => same(vec![
            Effect::Blocked,
            Effect::Status(SessionStatus::Suspended),
        ]),
        Watch::Idle if prev == Some(Watch::Idle) || prev.is_none() => same(vec![]),
        Watch::Idle => {
            match reply().filter(|r| link.last_reply_id.as_deref() != Some(r.id.as_str())) {
                Some(r) => Step {
                    watch: Watch::Idle,
                    idle_checks: 0,
                    reply_id: Some(r.id.clone()),
                    effects: vec![Effect::Reply(r), Effect::Status(SessionStatus::Active)],
                },
                None if link.idle_checks + 1 >= IDLE_GIVE_UP => {
                    same(vec![Effect::NoReply, Effect::Status(SessionStatus::Active)])
                }
                // Not yet: keep the previous state and count the miss.
                None => Step {
                    watch: prev.unwrap_or(Watch::Idle),
                    idle_checks: link.idle_checks + 1,
                    reply_id: None,
                    effects: vec![],
                },
            }
        }
    }
}

/// Whether the agent reported its conversation (its TUI is up).
fn session_known(task: &Task) -> bool {
    let Some(ctx) = task.context.as_ref() else {
        return false;
    };
    match task.agent_type {
        AgentType::ClaudeCode => ctx.claude_session_id.is_some(),
        _ => ctx.extra.contains_key("pi_session_path"),
    }
}

/// The task an agent started in `spawn`'s pane registered: same zellij
/// session and pane, registered after the spawn began. A sandbox attach
/// registers both the sandbox and its window; the window is the agent.
fn find_spawned_task<'a>(tasks: &'a [Task], spawn: &PendingSpawn) -> Option<&'a Task> {
    let since = spawn.started_at - chrono::Duration::seconds(5);
    tasks
        .iter()
        .filter(|t| t.created_at >= since)
        .filter(|t| t.status != TaskStatus::Exited)
        .filter(|t| {
            pane_target(t).is_some_and(|(s, p)| s == spawn.zellij_session && p == spawn.pane_id)
        })
        .max_by_key(|t| {
            let window = t
                .context
                .as_ref()
                .is_some_and(|c| c.extra.contains_key("parent_task_id"));
            (window, t.created_at)
        })
}

fn card_for(task: &Task, zellij: &str) -> CardInfo {
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

fn session_card(link: &Link, topic: Option<&str>) -> SessionCard {
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

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The command a new session's pane runs: the agent through its nibble
/// wrapper (host; Claude with `--fresh` so it doesn't resume the repo's
/// last conversation) or a `--btw` sandbox attach (a side session that
/// leaves the repo's main one alone), then the user's shell.
fn launch_script(
    location: Location,
    agent: SpawnAgent,
    cwd: &Path,
    nibble_bin: &Path,
    wrappers: &Path,
    shell: &str,
) -> String {
    let run = match location {
        Location::Host => {
            let (wrapper, args) = match agent {
                SpawnAgent::Claude => ("claude-wrapper", " --fresh"),
                SpawnAgent::Pi => ("pi-wrapper", ""),
                SpawnAgent::Omp => ("omp-wrapper", ""),
            };
            format!(
                "{}{args}",
                sh_quote(&wrappers.join(wrapper).to_string_lossy())
            )
        }
        Location::Sandbox => {
            let flag = match agent {
                SpawnAgent::Claude => "",
                SpawnAgent::Pi => " --pi",
                SpawnAgent::Omp => " --omp",
            };
            format!(
                "{} sandbox attach {} --btw{flag}",
                sh_quote(&nibble_bin.to_string_lossy()),
                sh_quote(&cwd.to_string_lossy())
            )
        }
    };
    format!("{run}; exec {}", sh_quote(shell))
}

/// The zellij tab name for a session started in `cwd`.
fn tab_name_for(cwd: &Path) -> String {
    let base = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "session".to_string());
    format!("{base} (slack)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TaskContext;
    use std::collections::HashMap;

    fn link(watch: Option<Watch>, last: Option<&str>, idle_checks: u32) -> Link {
        Link {
            task_id: "t".into(),
            channel: "D1".into(),
            thread_ts: "1.1".into(),
            created_at: Utc::now(),
            closed: false,
            watch,
            last_reply_id: last.map(str::to_string),
            idle_checks,
            permalink: None,
            card: CardInfo {
                title: "t".into(),
                agent: "Claude Code".into(),
                location: "host".into(),
                cwd: None,
                zellij: "main".into(),
            },
        }
    }

    fn reply(id: &str) -> Option<transcript::Reply> {
        Some(transcript::Reply {
            id: id.into(),
            text: format!("text {id}"),
        })
    }

    #[test]
    fn finished_turn_posts_the_new_reply_once() {
        let l = link(Some(Watch::Working), Some("old"), 0);
        let s = next_step(&l, Watch::Idle, &|| reply("new"));
        assert_eq!(s.watch, Watch::Idle);
        assert_eq!(s.reply_id.as_deref(), Some("new"));
        assert_eq!(s.effects[0], Effect::Reply(reply("new").unwrap()));
        assert_eq!(s.effects[1], Effect::Status(SessionStatus::Active));

        // Already idle: nothing, and the transcript is not even read.
        let l = link(Some(Watch::Idle), Some("new"), 0);
        let s = next_step(&l, Watch::Idle, &|| panic!("must not read"));
        assert!(s.effects.is_empty());
        assert!(!s.changed(&l));
    }

    #[test]
    fn idle_without_a_new_reply_waits_then_gives_up() {
        let l = link(Some(Watch::Working), Some("same"), 0);
        let s = next_step(&l, Watch::Idle, &|| reply("same"));
        assert_eq!((s.watch, s.idle_checks), (Watch::Working, 1));
        assert!(s.effects.is_empty());
        let l = link(Some(Watch::Working), Some("same"), IDLE_GIVE_UP - 1);
        let s = next_step(&l, Watch::Idle, &|| None);
        assert_eq!((s.watch, s.idle_checks), (Watch::Idle, 0));
        assert_eq!(
            s.effects,
            vec![Effect::NoReply, Effect::Status(SessionStatus::Active)]
        );
    }

    #[test]
    fn long_running_turns_stay_working() {
        // An hour without reports is still a working turn for the relay.
        let mut t = task(
            "t",
            1,
            "main",
            Utc::now() - chrono::Duration::hours(1),
            false,
        );
        t.status = TaskStatus::Running;
        assert_eq!(observe(&t), Watch::Working);
        t.attention_reason = Some("Permission required: Bash".into());
        assert_eq!(observe(&t), Watch::Blocked);
        t.attention_reason = None;
        t.status = TaskStatus::Completed;
        assert_eq!(observe(&t), Watch::Idle);
        t.status = TaskStatus::Exited;
        assert_eq!(observe(&t), Watch::Exited);
    }

    #[test]
    fn working_blocked_and_exit_transitions() {
        let s = next_step(&link(Some(Watch::Idle), None, 0), Watch::Working, &|| None);
        assert_eq!(s.effects, vec![Effect::Status(SessionStatus::Processing)]);
        let s = next_step(
            &link(Some(Watch::Working), None, 3),
            Watch::Working,
            &|| None,
        );
        assert!(s.effects.is_empty());
        assert_eq!(s.idle_checks, 0);

        let s = next_step(
            &link(Some(Watch::Working), None, 0),
            Watch::Blocked,
            &|| None,
        );
        assert_eq!(
            s.effects,
            vec![Effect::Blocked, Effect::Status(SessionStatus::Suspended)]
        );
        let s = next_step(
            &link(Some(Watch::Blocked), None, 0),
            Watch::Blocked,
            &|| None,
        );
        assert!(s.effects.is_empty());
        // Approved, then finished: the reply is posted.
        let s = next_step(&link(Some(Watch::Blocked), None, 0), Watch::Idle, &|| {
            reply("r")
        });
        assert!(matches!(s.effects[0], Effect::Reply(_)));

        let s = next_step(&link(Some(Watch::Working), None, 0), Watch::Exited, &|| {
            None
        });
        assert_eq!(s.effects, vec![Effect::Close]);
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

    fn task(id: &str, pane: u32, session: &str, created: DateTime<Utc>, window: bool) -> Task {
        let mut extra = HashMap::new();
        crate::models::stamp_zellij(&mut extra, Some(pane), Some(session));
        if window {
            extra.insert("parent_task_id".into(), json!("sandbox"));
        }
        let mut t = Task::new(
            id.to_string(),
            AgentType::ClaudeCode,
            "[repo]".to_string(),
            None,
            None,
        );
        t.context = Some(TaskContext {
            extra,
            ..Default::default()
        });
        t.created_at = created;
        t.updated_at = created;
        t
    }

    fn spawn(started: DateTime<Utc>) -> PendingSpawn {
        PendingSpawn {
            channel: "D1".into(),
            thread_ts: "1.1".into(),
            location: Location::Sandbox,
            agent: SpawnAgent::Claude,
            cwd: "/r".into(),
            prompt: "go".into(),
            zellij_session: "main".into(),
            pane_id: 7,
            tab_name: "r (slack)".into(),
            started_at: started,
            task_id: None,
            ready_at: None,
            queued: vec![],
        }
    }

    #[test]
    fn finds_the_agent_registered_in_the_new_pane() {
        let now = Utc::now();
        let old = now - chrono::Duration::hours(1);
        let tasks = vec![
            task("stale", 7, "main", old, false), // old pane-id reuse
            task("other-pane", 8, "main", now, false),
            task("other-session", 7, "work", now, false),
            task("sandbox", 7, "main", now, false),
            task("window", 7, "main", now, true),
        ];
        assert_eq!(
            find_spawned_task(&tasks, &spawn(now)).unwrap().task_id,
            "window"
        );
        assert!(find_spawned_task(&tasks[..3], &spawn(now)).is_none());
    }

    #[test]
    fn readiness_needs_the_agent_session() {
        let mut t = task("t", 1, "main", Utc::now(), false);
        assert!(!session_known(&t));
        t.context.as_mut().unwrap().claude_session_id = Some("s".into());
        assert!(session_known(&t));
        let mut p = task("p", 1, "main", Utc::now(), false);
        p.agent_type = AgentType::Pi;
        assert!(!session_known(&p));
        p.context
            .as_mut()
            .unwrap()
            .extra
            .insert("pi_session_path".into(), json!("/x.jsonl"));
        assert!(session_known(&p));
    }

    #[test]
    fn launch_scripts_per_location_and_agent() {
        let bin = Path::new("/bin/nib ble");
        let wr = Path::new("/w");
        let cwd = Path::new("/r/it's");
        assert_eq!(
            launch_script(Location::Host, SpawnAgent::Claude, cwd, bin, wr, "/bin/zsh"),
            "'/w/claude-wrapper' --fresh; exec '/bin/zsh'"
        );
        assert_eq!(
            launch_script(Location::Host, SpawnAgent::Omp, cwd, bin, wr, "/bin/zsh"),
            "'/w/omp-wrapper'; exec '/bin/zsh'"
        );
        assert_eq!(
            launch_script(Location::Sandbox, SpawnAgent::Pi, cwd, bin, wr, "/bin/sh"),
            "'/bin/nib ble' sandbox attach '/r/it'\\''s' --btw --pi; exec '/bin/sh'"
        );
        assert_eq!(tab_name_for(cwd), "it's (slack)");
    }

    #[test]
    fn card_marks_sandbox_windows() {
        let t = task("w", 1, "main", Utc::now(), true);
        assert_eq!(card_for(&t, "main").location, "sandbox");
        let h = task("h", 1, "main", Utc::now(), false);
        assert_eq!(card_for(&h, "main").location, "host");
    }
}
