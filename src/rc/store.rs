//! What `nibble rc` remembers, kept as JSON in a `kv_store`, in two places:
//!
//! In nibble's database (`~/.nibble`, which sandboxes mount read-write),
//! only what agents legitimately exchange with the daemon:
//! - `rc:req:<task_id>` / `rc:res:<task_id>`: a `/nibble-rc` request from an
//!   agent (host or sandbox) and the daemon's answer. Requests go through
//!   the daemon so Slack tokens never have to be readable inside a sandbox.
//! - `rc:heartbeat`: when the daemon last ran a poll pass.
//!
//! In the daemon's private state database ([`open_state`], under
//! `$XDG_STATE_HOME`, which no sandbox mounts), what decides where text is
//! typed and posted, so a sandboxed agent cannot forge it:
//! - `rc:link:<task_id>`: a session ↔ Slack thread link ([`Link`]).
//! - `rc:spawn:<thread_ts>`: a session started from Slack whose agent has
//!   not registered yet ([`PendingSpawn`]).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::db::Database;

const LINK_PREFIX: &str = "rc:link:";

/// `$XDG_STATE_HOME/nibble/rc` (default `~/.local/state/nibble/rc`): the
/// daemon's private state and lock, out of every sandbox's reach. Its own
/// directory, so making it private leaves `nibble/` (market history) alone.
pub(crate) fn state_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".local")
                .join("state")
        })
        .join("nibble")
        .join("rc")
}

/// The daemon's private state database. A distinct type from nibble's
/// shared [`Database`], so links and pending starts cannot be read from or
/// written to the database sandboxes can write.
pub(crate) struct StateDb(Database);

impl StateDb {
    /// The underlying database, for records kept by other rc modules.
    pub(super) fn db(&self) -> &Database {
        &self.0
    }
}

/// Open (creating, mode 0700) the daemon's private state database.
pub(crate) fn open_state() -> Result<StateDb> {
    open_state_in(&state_dir())
}

/// [`open_state`] in `dir`.
pub(crate) fn open_state_in(dir: &std::path::Path) -> Result<StateDb> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to restrict {}", dir.display()))?;
    Database::open(dir.join("rc.db"))
        .map(StateDb)
        .context("failed to open the rc state database")
}
const REQ_PREFIX: &str = "rc:req:";
const RES_PREFIX: &str = "rc:res:";
const SPAWN_PREFIX: &str = "rc:spawn:";
const HEARTBEAT_KEY: &str = "rc:heartbeat";

/// The session state the daemon last relayed for a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Watch {
    Working,
    #[default]
    Idle,
    Blocked,
    Exited,
}

/// What the session card shows; kept so it can be redrawn on close.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CardInfo {
    pub title: String,
    pub agent: String,
    pub location: String,
    pub cwd: Option<String>,
    pub zellij: String,
}

/// A nibble task relayed to a Slack thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Link {
    pub task_id: String,
    pub channel: String,
    /// The root message: the thread is identified by it.
    pub thread_ts: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub closed: bool,
    /// Last relayed state.
    #[serde(default)]
    pub watch: Watch,
    /// Transcript id of the newest text the thread has (progress or reply),
    /// or chose to skip: texts are relayed only after it, so none twice.
    #[serde(default)]
    pub last_reply_id: Option<String>,
    /// The permission prompt last posted to the thread, so a different
    /// prompt in the same blocked spell is posted too.
    #[serde(default)]
    pub blocked_reason: Option<String>,
    /// Polls since the task went idle without a new reply in its
    /// transcript (the transcript may lag the idle report briefly).
    #[serde(default)]
    pub idle_checks: u32,
    #[serde(default)]
    pub permalink: Option<String>,
    /// The session card (the message with the Close button), when it is not
    /// the thread's root: a thread started from your own DM message, or
    /// from the prompt `/nibble` posts, carries the card as a reply.
    #[serde(default)]
    pub card_ts: Option<String>,
    pub card: CardInfo,
    /// Started from Slack (`/nibble` or a DM) in a tab the daemon opened:
    /// only these can be ended from Slack, which closes that tab.
    #[serde(default)]
    pub from_slack: bool,
}

impl Link {
    /// A freshly opened link; the caller fills in `permalink`, `card_ts` and
    /// `last_reply_id` once known.
    pub(crate) fn new(
        task_id: &str,
        channel: &str,
        thread_ts: &str,
        card: CardInfo,
        watch: Watch,
    ) -> Self {
        Link {
            task_id: task_id.to_string(),
            channel: channel.to_string(),
            thread_ts: thread_ts.to_string(),
            created_at: Utc::now(),
            closed: false,
            watch,
            last_reply_id: None,
            blocked_reason: None,
            idle_checks: 0,
            permalink: None,
            card_ts: None,
            card,
            from_slack: false,
        }
    }

    /// The message to rewrite when the session closes.
    pub(crate) fn card_message(&self) -> &str {
        self.card_ts.as_deref().unwrap_or(&self.thread_ts)
    }
}

/// What an agent asks the daemon to do with its own session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequestAction {
    Start,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Request {
    pub action: RequestAction,
    pub at: DateTime<Utc>,
}

/// The daemon's answer to a [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Response {
    pub ok: bool,
    pub message: String,
}

/// Where a session started from Slack runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Location {
    Host,
    Sandbox,
}

impl Location {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Location::Host => "host",
            Location::Sandbox => "sandbox",
        }
    }
}

/// Agents a session can be started with from Slack: the ones `nibble mux`
/// can restore, since these sessions open in a mux-recorded session (omp
/// panes are not restored, so omp is offered only through `/nibble-rc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SpawnAgent {
    Claude,
    Pi,
}

impl SpawnAgent {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SpawnAgent::Claude => "claude",
            SpawnAgent::Pi => "pi",
        }
    }
}

/// A session started from Slack, waiting for its agent to register.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PendingSpawn {
    pub channel: String,
    pub thread_ts: String,
    pub location: Location,
    pub agent: SpawnAgent,
    pub cwd: String,
    pub prompt: String,
    pub zellij_session: String,
    /// The pane zellij created for the agent.
    pub pane_id: u32,
    pub tab_name: String,
    pub started_at: DateTime<Utc>,
    /// The agent's task, once it registered in the pane.
    #[serde(default)]
    pub task_id: Option<String>,
    /// When the agent reported its session (its TUI is up).
    #[serde(default)]
    pub ready_at: Option<DateTime<Utc>>,
    /// Replies sent in the thread before the agent was ready, in order.
    #[serde(default)]
    pub queued: Vec<String>,
    /// How many of the first message and queued replies were typed, so a
    /// retried start never types one twice.
    #[serde(default)]
    pub typed: usize,
    /// The transcript's newest text before anything was typed (`Some(None)`:
    /// none yet), taken once so a replayed start never mistakes the agent's
    /// answer to the first message for an old text.
    #[serde(default)]
    pub baseline_reply: Option<Option<String>>,
    /// The last transient failure, reported if the start times out.
    #[serde(default)]
    pub last_error: Option<String>,
}

fn get<T: DeserializeOwned>(db: &Database, key: &str) -> Result<Option<T>> {
    db.kv_get(key)?
        .map(|raw| serde_json::from_str(&raw).with_context(|| format!("corrupt rc record {key}")))
        .transpose()
}

fn put<T: Serialize>(db: &Database, key: &str, value: &T) -> Result<()> {
    db.kv_set(key, &serde_json::to_string(value)?)
}

fn all<T: DeserializeOwned>(db: &Database, prefix: &str) -> Result<Vec<(String, T)>> {
    let mut out = Vec::new();
    for key in db.kv_keys_with_prefix(prefix)? {
        // A record deleted between listing and reading is simply skipped;
        // a corrupt one is skipped with a warning instead of wedging the
        // daemon.
        match get::<T>(db, &key) {
            Ok(Some(v)) => out.push((key[prefix.len()..].to_string(), v)),
            Ok(None) => {}
            Err(e) => eprintln!("nibble rc: {e:#}"),
        }
    }
    Ok(out)
}

pub(crate) fn link(state: &StateDb, task_id: &str) -> Result<Option<Link>> {
    get(&state.0, &format!("{LINK_PREFIX}{task_id}"))
}

pub(crate) fn put_link(state: &StateDb, link: &Link) -> Result<()> {
    put(&state.0, &format!("{LINK_PREFIX}{}", link.task_id), link)
}

pub(crate) fn links(state: &StateDb) -> Result<Vec<Link>> {
    Ok(all(&state.0, LINK_PREFIX)?
        .into_iter()
        .map(|(_, l)| l)
        .collect())
}

/// The link whose thread is `thread_ts` in `channel`: the open one when a
/// closed thread later got a new session, else the closed one.
pub(crate) fn link_by_thread(
    state: &StateDb,
    channel: &str,
    thread_ts: &str,
) -> Result<Option<Link>> {
    let mut matching: Vec<Link> = links(state)?
        .into_iter()
        .filter(|l| l.channel == channel && l.thread_ts == thread_ts)
        .collect();
    matching.sort_by_key(|l| (!l.closed, l.created_at));
    Ok(matching.pop())
}

pub(crate) fn put_request(db: &Database, task_id: &str, req: &Request) -> Result<()> {
    db.kv_delete(&format!("{RES_PREFIX}{task_id}"))?;
    put(db, &format!("{REQ_PREFIX}{task_id}"), req)
}

/// Pending requests, oldest first.
pub(crate) fn requests(db: &Database) -> Result<Vec<(String, Request)>> {
    let mut reqs = all::<Request>(db, REQ_PREFIX)?;
    reqs.sort_by_key(|(_, r)| r.at);
    Ok(reqs)
}

/// Answer a request (and drop it).
pub(crate) fn answer(db: &Database, task_id: &str, resp: &Response) -> Result<()> {
    put(db, &format!("{RES_PREFIX}{task_id}"), resp)?;
    db.kv_delete(&format!("{REQ_PREFIX}{task_id}"))
}

/// Take the daemon's answer to this task's request, if it has one.
pub(crate) fn take_response(db: &Database, task_id: &str) -> Result<Option<Response>> {
    let key = format!("{RES_PREFIX}{task_id}");
    let resp = get(db, &key)?;
    if resp.is_some() {
        db.kv_delete(&key)?;
    }
    Ok(resp)
}

/// Withdraw an unanswered request (the CLI gave up waiting).
pub(crate) fn withdraw_request(db: &Database, task_id: &str) -> Result<()> {
    db.kv_delete(&format!("{REQ_PREFIX}{task_id}"))
}

pub(crate) fn put_spawn(state: &StateDb, spawn: &PendingSpawn) -> Result<()> {
    put(
        &state.0,
        &format!("{SPAWN_PREFIX}{}", spawn.thread_ts),
        spawn,
    )
}

pub(crate) fn spawns(state: &StateDb) -> Result<Vec<PendingSpawn>> {
    Ok(all(&state.0, SPAWN_PREFIX)?
        .into_iter()
        .map(|(_, s)| s)
        .collect())
}

pub(crate) fn spawn_for_thread(state: &StateDb, thread_ts: &str) -> Result<Option<PendingSpawn>> {
    get(&state.0, &format!("{SPAWN_PREFIX}{thread_ts}"))
}

pub(crate) fn delete_spawn(state: &StateDb, thread_ts: &str) -> Result<()> {
    state.0.kv_delete(&format!("{SPAWN_PREFIX}{thread_ts}"))
}

pub(crate) fn beat(db: &Database, now: DateTime<Utc>) -> Result<()> {
    db.kv_set(HEARTBEAT_KEY, &now.to_rfc3339())
}

pub(crate) fn last_beat(db: &Database) -> Result<Option<DateTime<Utc>>> {
    Ok(db
        .kv_get(HEARTBEAT_KEY)?
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|t| t.with_timezone(&Utc)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("t.db")).unwrap();
        (dir, db)
    }

    fn state() -> (tempfile::TempDir, StateDb) {
        let dir = tempfile::tempdir().unwrap();
        let state = open_state_in(&dir.path().join("rc")).unwrap();
        (dir, state)
    }

    fn sample(task: &str, ts: &str) -> Link {
        crate::rc::test_support::link(task, ts)
    }

    #[test]
    fn links_saved_before_end_session_existed_cannot_be_ended() {
        let mut v = serde_json::to_value(sample("a", "1.1")).unwrap();
        v.as_object_mut().unwrap().remove("from_slack");
        let l: Link = serde_json::from_value(v).unwrap();
        assert!(!l.from_slack);
    }

    #[test]
    fn links_round_trip_and_resolve_by_thread() {
        let (_d, state) = state();
        put_link(&state, &sample("a", "1.1")).unwrap();
        put_link(&state, &sample("b", "2.2")).unwrap();
        assert_eq!(link(&state, "a").unwrap().unwrap().thread_ts, "1.1");
        assert_eq!(
            link_by_thread(&state, "D1", "2.2")
                .unwrap()
                .unwrap()
                .task_id,
            "b"
        );
        assert!(link_by_thread(&state, "D2", "2.2").unwrap().is_none());
        // A closed thread that got a new session resolves to the open one.
        let mut old = sample("old", "2.2");
        old.closed = true;
        put_link(&state, &old).unwrap();
        assert_eq!(
            link_by_thread(&state, "D1", "2.2")
                .unwrap()
                .unwrap()
                .task_id,
            "b"
        );
        assert_eq!(links(&state).unwrap().len(), 3);
    }

    #[test]
    fn requests_are_answered_once() {
        let (_d, db) = db();
        let req = Request {
            action: RequestAction::Start,
            at: Utc::now(),
        };
        put_request(&db, "t", &req).unwrap();
        assert_eq!(requests(&db).unwrap(), vec![("t".to_string(), req)]);
        answer(
            &db,
            "t",
            &Response {
                ok: true,
                message: "done".into(),
            },
        )
        .unwrap();
        assert!(requests(&db).unwrap().is_empty());
        assert_eq!(take_response(&db, "t").unwrap().unwrap().message, "done");
        assert!(take_response(&db, "t").unwrap().is_none());
    }

    #[test]
    fn a_new_request_clears_a_stale_answer() {
        let (_d, db) = db();
        answer(
            &db,
            "t",
            &Response {
                ok: false,
                message: "old".into(),
            },
        )
        .unwrap();
        put_request(
            &db,
            "t",
            &Request {
                action: RequestAction::Stop,
                at: Utc::now(),
            },
        )
        .unwrap();
        assert!(take_response(&db, "t").unwrap().is_none());
    }

    #[test]
    fn corrupt_records_are_skipped() {
        let (_d, state) = state();
        state.0.kv_set("rc:link:bad", "{not json").unwrap();
        put_link(&state, &sample("ok", "3.3")).unwrap();
        assert_eq!(links(&state).unwrap().len(), 1);
    }

    #[test]
    fn close_rewrites_the_card_not_a_root_the_bot_does_not_own() {
        let mut l = sample("t", "1.1");
        assert_eq!(l.card_message(), "1.1");
        l.card_ts = Some("1.2".into());
        assert_eq!(l.card_message(), "1.2");
        // Links stored before card_ts existed still load.
        let mut v = serde_json::to_value(sample("t", "1.1")).unwrap();
        v.as_object_mut().unwrap().remove("card_ts");
        assert_eq!(serde_json::from_value::<Link>(v).unwrap().card_ts, None);
    }

    #[test]
    fn pending_starts_live_only_in_the_private_state() {
        let (_d, state) = state();
        let (_e, shared) = db();
        let spawn = PendingSpawn {
            channel: "D1".into(),
            thread_ts: "1.1".into(),
            location: Location::Host,
            agent: SpawnAgent::Claude,
            cwd: "/r".into(),
            prompt: "go".into(),
            zellij_session: "main".into(),
            pane_id: 7,
            tab_name: "r (slack)".into(),
            started_at: Utc::now(),
            task_id: None,
            ready_at: None,
            queued: vec![],
            typed: 0,
            baseline_reply: None,
            last_error: None,
        };
        put_spawn(&state, &spawn).unwrap();
        // What the poll pass and on_reply read back.
        assert_eq!(spawns(&state).unwrap(), vec![spawn.clone()]);
        assert_eq!(spawn_for_thread(&state, "1.1").unwrap(), Some(spawn));
        // Nothing reached the database sandboxes can write.
        assert!(shared.kv_keys_with_prefix("rc:").unwrap().is_empty());
        delete_spawn(&state, "1.1").unwrap();
        assert!(spawns(&state).unwrap().is_empty());
    }

    #[test]
    fn the_state_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (d, _state) = state();
        let mode = std::fs::metadata(d.path().join("rc"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn heartbeat_round_trips() {
        let (_d, db) = db();
        assert!(last_beat(&db).unwrap().is_none());
        let now = Utc::now();
        beat(&db, now).unwrap();
        assert_eq!(
            last_beat(&db).unwrap().unwrap().timestamp(),
            now.timestamp()
        );
    }
}
