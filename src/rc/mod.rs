//! `nibble rc`: remote-control agent sessions from Slack.
//!
//! - `/nibble-rc` inside a Claude Code, pi or omp session (host or sandbox)
//!   sends that session to a new thread in your DM with the nibble Slack
//!   app. Replies in the thread are typed into the session's zellij pane;
//!   each finished turn's reply comes back to the thread. The pane stays
//!   live, so you can carry on at the terminal at any time.
//! - `/nibble` in Slack (or any new DM to the app) starts a new session in a
//!   new tab of the `rc.zellij_session` zellij session, on the host or in a
//!   sandbox, in a repo you pick; its thread works the same way.
//! - Close a session's thread with its Close button or `/nibble-rc stop`;
//!   the zellij tab is never touched.
//!
//! The Slack connection lives in one host daemon (`nibble rc daemon`,
//! `nibble-rc.service`). Agents talk to it through nibble's database, so
//! the Slack tokens are never readable from inside a sandbox. Setup and
//! the Slack app manifest are in `docs/rc.md`.

mod daemon;
mod modal;
mod pane;
mod relay;
mod secrets;
mod slack;
mod socket;
mod spawn;
mod store;
mod transcript;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use crate::config::RcConfig;
use crate::db::Database;
use store::{Request, RequestAction};

/// The Slack app manifest, for `nibble rc manifest`.
const MANIFEST: &str = include_str!("../../docs/rc-slack-manifest.yml");
/// How long `/nibble-rc` waits for the daemon's answer.
const REQUEST_WAIT: Duration = Duration::from_secs(20);
/// A heartbeat older than this means the daemon is not running.
const BEAT_STALE_SECS: i64 = 120;

/// Run the daemon (`nibble rc daemon`).
pub(crate) fn cmd_daemon(cfg: RcConfig, db_path: std::path::PathBuf) -> Result<()> {
    daemon::run(cfg, db_path)
}

/// Print the Slack app manifest.
pub(crate) fn cmd_manifest() {
    print!("{MANIFEST}");
}

/// The task a command targets: the explicit id, else the agent's own
/// (`AGENT_TASK_ID`, exported by the wrappers and sandbox attach).
fn resolve_task(task: Option<&str>) -> Result<String> {
    task.map(str::to_string)
        .or_else(|| std::env::var("AGENT_TASK_ID").ok())
        .filter(|t| !t.is_empty())
        .context("no task given and AGENT_TASK_ID is not set (run this from an agent, or pass a task id)")
}

/// Ask the daemon to start or stop relaying `task_id`, and wait for its
/// answer.
fn request(db: &Database, task_id: &str, action: RequestAction) -> Result<String> {
    let fresh =
        store::last_beat(db)?.is_some_and(|b| (Utc::now() - b).num_seconds() <= BEAT_STALE_SECS);
    if !fresh {
        bail!("the nibble rc daemon is not running (systemctl --user start nibble-rc; check `nibble rc doctor`)");
    }
    store::put_request(
        db,
        task_id,
        &Request {
            action,
            at: Utc::now(),
        },
    )?;
    let deadline = Instant::now() + REQUEST_WAIT;
    while Instant::now() < deadline {
        if let Some(resp) = store::take_response(db, task_id)? {
            return if resp.ok {
                Ok(resp.message)
            } else {
                bail!("{}", resp.message)
            };
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    store::withdraw_request(db, task_id)?;
    bail!(
        "the nibble rc daemon did not answer within {}s",
        REQUEST_WAIT.as_secs()
    )
}

/// `nibble rc start|stop [TASK]`.
pub(crate) fn cmd_request(db: &Database, task: Option<&str>, stop: bool) -> Result<()> {
    let task_id = resolve_task(task)?;
    let action = if stop {
        RequestAction::Stop
    } else {
        RequestAction::Start
    };
    println!("{}", request(db, &task_id, action)?);
    Ok(())
}

/// `nibble rc list`.
pub(crate) fn cmd_list(db: &Database) -> Result<()> {
    let mut links = store::links(db)?;
    links.sort_by_key(|l| (l.closed, std::cmp::Reverse(l.created_at)));
    if links.is_empty() {
        println!("No sessions have been sent to Slack.");
    }
    for l in links {
        println!(
            "{} {:<10} {} · {} · {}{}",
            if l.closed { "closed" } else { "open  " },
            l.task_id.chars().take(10).collect::<String>(),
            l.card.title,
            l.card.agent,
            l.card.location,
            l.permalink.map(|p| format!("  {p}")).unwrap_or_default()
        );
    }
    for s in store::spawns(db)? {
        println!(
            "starting {} in {} (tab {})",
            s.agent.as_str(),
            s.cwd,
            s.tab_name
        );
    }
    Ok(())
}

/// Which `/nibble-rc` invocation a prompt is.
#[derive(Debug, PartialEq)]
enum RcCommand {
    Start,
    Stop,
}

const USAGE: &str = "usage: /nibble-rc [stop]";

/// Parse a prompt as `/nibble-rc [start|stop]`. `None` when the prompt is
/// something else; `Err` for a malformed invocation.
fn parse_command(prompt: &str) -> Option<std::result::Result<RcCommand, String>> {
    let mut words = prompt.split_whitespace();
    if words.next()? != "/nibble-rc" {
        return None;
    }
    let arg = words.next();
    if words.next().is_some() {
        return Some(Err(USAGE.to_string()));
    }
    Some(match arg {
        None | Some("start") | Some("on") => Ok(RcCommand::Start),
        Some("stop") | Some("off") => Ok(RcCommand::Stop),
        Some(_) => Err(USAGE.to_string()),
    })
}

/// The hook's answer for Claude: block the prompt (it never reaches the
/// model) and show `reason` to the user.
fn block(reason: &str) -> Value {
    json!({ "decision": "block", "reason": reason })
}

/// What the Claude `UserPromptSubmit` hook prints for `input` (the hook's
/// JSON). `None` lets the prompt through untouched. `env_task` is the
/// wrapper's `AGENT_TASK_ID`; without one the task is the derived
/// `claude-<session_id>` the other hooks use.
fn claude_hook_response(
    input: &Value,
    env_task: Option<String>,
    run: &dyn Fn(&str, RequestAction) -> Result<String>,
) -> Option<Value> {
    let prompt = input["prompt"].as_str().unwrap_or_default();
    let cmd = parse_command(prompt)?;
    let task_id = env_task
        .filter(|t| !t.is_empty())
        .or_else(|| input["session_id"].as_str().map(|s| format!("claude-{s}")));
    Some(match (cmd, task_id) {
        (Err(usage), _) => block(&usage),
        (Ok(_), None) => block("nibble-rc: no task id for this session"),
        (Ok(cmd), Some(task)) => {
            let action = match cmd {
                RcCommand::Start => RequestAction::Start,
                RcCommand::Stop => RequestAction::Stop,
            };
            match run(&task, action) {
                Ok(msg) => block(&format!("nibble-rc: {msg}")),
                Err(e) => block(&format!("nibble-rc failed: {e:#}")),
            }
        }
    })
}

/// `nibble rc claude-hook`: the `UserPromptSubmit` hook that turns
/// `/nibble-rc` into a request without spending a model turn.
pub(crate) fn cmd_claude_hook(db: &Database) -> Result<()> {
    let mut raw = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw)
        .context("failed to read hook input")?;
    let Ok(input) = serde_json::from_str::<Value>(&raw) else {
        return Ok(()); // not ours to judge: let the prompt through
    };
    let env_task = std::env::var("AGENT_TASK_ID").ok();
    if let Some(out) =
        claude_hook_response(&input, env_task, &|task, action| request(db, task, action))
    {
        println!("{out}");
    }
    Ok(())
}

// ── setup / doctor ────────────────────────────────────────────────────────────

/// Read one line from the terminal, optionally without echo (tokens).
fn prompt_line(label: &str, secret: bool) -> Result<String> {
    print!("{label}: ");
    std::io::stdout().flush()?;
    let _guard = if secret { EchoOff::new() } else { None };
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("failed to read input")?;
    if secret {
        println!();
    }
    Ok(line.trim().to_string())
}

/// The terminal state to restore if `nibble rc setup` is interrupted while
/// echo is off (a signal ends the process without running `Drop`).
static SAVED_TERMIOS: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();

extern "C" fn restore_and_reraise(sig: libc::c_int) {
    // SAFETY: tcsetattr, signal and raise are async-signal-safe; the saved
    // termios was written before this handler was installed.
    unsafe {
        if let Some(t) = SAVED_TERMIOS.get() {
            libc::tcsetattr(0, libc::TCSANOW, t);
        }
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Disables terminal echo while alive (no-op when stdin is not a tty).
/// Ctrl-C or a TERM signal restores echo before the process dies.
struct EchoOff(libc::termios);

impl EchoOff {
    fn new() -> Option<Self> {
        // SAFETY: tcgetattr/tcsetattr on fd 0 with valid termios pointers;
        // the saved state is restored on drop or by the signal handler.
        unsafe {
            if libc::isatty(0) != 1 {
                return None;
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let orig = t;
            let _ = SAVED_TERMIOS.set(orig);
            let handler = restore_and_reraise as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
            t.c_lflag &= !libc::ECHO;
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return None;
            }
            Some(EchoOff(orig))
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: restores the termios saved in `new`, then the default
        // signal dispositions.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.0);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
        }
    }
}

/// `nibble rc setup`: store the Slack credentials and check them.
pub(crate) fn cmd_setup() -> Result<()> {
    println!(
        "Create the Slack app first: `nibble rc manifest` prints its manifest (see docs/rc.md)."
    );
    let secrets = secrets::Secrets {
        bot_token: prompt_line("Bot token (xoxb-…, OAuth & Permissions)", true)?,
        app_token: prompt_line(
            "App-level token (xapp-…, Basic Information, scope connections:write)",
            true,
        )?,
        user_id: prompt_line("Your Slack member ID (profile → ⋮ → Copy member ID)", false)?,
    };
    let path = secrets::save(&secrets)?;
    println!("Saved to {} (mode 600).", path.display());
    let (team, user) = slack::Slack::new(&secrets.bot_token).auth_test()?;
    println!("Bot token works: {user} in {team}.");
    println!("Next: systemctl --user enable nibble-rc && systemctl --user restart nibble-rc, then `nibble rc doctor`.");
    Ok(())
}

fn check(label: &str, result: Result<String>) -> bool {
    match result {
        Ok(detail) => {
            println!(
                "  ✓ {label}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            );
            true
        }
        Err(e) => {
            println!("  ✗ {label}: {e:#}");
            false
        }
    }
}

/// `nibble rc doctor`: check every piece remote control relies on.
pub(crate) fn cmd_doctor(db: &Database, cfg: &RcConfig) -> Result<()> {
    println!("nibble rc doctor");
    let secrets = secrets::load();
    let mut ok = check(
        "credentials",
        secrets
            .as_ref()
            .map(|_| secrets::secrets_path().display().to_string())
            .map_err(|e| anyhow::anyhow!("{e:#}")),
    );
    if let Ok(s) = &secrets {
        let bot = slack::Slack::new(&s.bot_token);
        ok &= check(
            "bot token",
            bot.auth_test()
                .map(|(team, user)| format!("{user} in {team}")),
        );
        ok &= check("your DM with the app", bot.open_dm(&s.user_id));
        let app = slack::Slack::new(&s.app_token);
        ok &= check(
            "Socket Mode",
            app.open_connection().and_then(|url| {
                let mut conn = socket::connect(&url)?;
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline {
                    if let socket::Read::Text(t) = conn.read()? {
                        if socket::parse_envelope(&t)? == socket::Envelope::Hello {
                            return Ok("hello received".to_string());
                        }
                    }
                }
                bail!("no hello from Slack within 10s")
            }),
        );
    }
    ok &= check(
        "zellij",
        std::process::Command::new("zellij")
            .arg("--version")
            .output()
            .map_err(anyhow::Error::from)
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()),
    );
    println!(
        "  · new sessions open in zellij session '{}'",
        cfg.zellij_session
    );
    ok &= check(
        "daemon",
        store::last_beat(db).and_then(|b| match b {
            Some(b) if (Utc::now() - b).num_seconds() <= BEAT_STALE_SECS => {
                Ok(format!("alive ({}s ago)", (Utc::now() - b).num_seconds()))
            }
            Some(b) => bail!("last seen {b}; systemctl --user status nibble-rc"),
            None => bail!("never ran; systemctl --user enable --now nibble-rc"),
        }),
    );
    if !ok {
        bail!("some checks failed");
    }
    Ok(())
}

/// Fixtures shared by the rc modules' tests.
#[cfg(test)]
mod test_support {
    use chrono::{DateTime, Utc};
    use serde_json::json;
    use std::collections::HashMap;

    use super::store::{CardInfo, Link};
    use crate::models::{AgentType, Task, TaskContext};

    /// An open link for task `task` on thread `ts` in DM `D1`.
    pub(crate) fn link(task: &str, ts: &str) -> Link {
        let card = CardInfo {
            title: "t".into(),
            agent: "claude".into(),
            location: "host".into(),
            cwd: None,
            zellij: "main".into(),
        };
        let mut l = Link::new(task, "D1", ts, card, super::store::Watch::Idle);
        l.watch = None;
        l
    }

    /// A task registered in zellij pane `pane` of `session`; `window`
    /// marks it as a sandbox window (it has a parent sandbox task).
    pub(crate) fn task(
        id: &str,
        pane: u32,
        session: &str,
        created: DateTime<Utc>,
        window: bool,
    ) -> Task {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nibble_rc_invocations() {
        assert_eq!(parse_command("/nibble-rc"), Some(Ok(RcCommand::Start)));
        assert_eq!(
            parse_command("  /nibble-rc start "),
            Some(Ok(RcCommand::Start))
        );
        assert_eq!(parse_command("/nibble-rc stop"), Some(Ok(RcCommand::Stop)));
        assert_eq!(parse_command("/nibble-rc off"), Some(Ok(RcCommand::Stop)));
        assert!(matches!(
            parse_command("/nibble-rc frobnicate"),
            Some(Err(_))
        ));
        assert!(matches!(parse_command("/nibble-rc stop now"), Some(Err(_))));
        assert_eq!(parse_command("/nibble-rcx"), None);
        assert_eq!(parse_command("please run /nibble-rc"), None);
        assert_eq!(parse_command(""), None);
    }

    #[test]
    fn hook_passes_other_prompts_through() {
        let out = claude_hook_response(&json!({"prompt": "fix the bug"}), None, &|_, _| {
            panic!("no request")
        });
        assert!(out.is_none());
    }

    #[test]
    fn hook_blocks_and_reports_the_daemon_answer() {
        // Task id from the session id when no wrapper exported one.
        let out = claude_hook_response(
            &json!({"prompt": "/nibble-rc", "session_id": "abc"}),
            None,
            &|task, action| {
                assert_eq!((task, action), ("claude-abc", RequestAction::Start));
                Ok("Sent to Slack".into())
            },
        )
        .unwrap();
        assert_eq!(out["decision"], "block");
        assert_eq!(out["reason"], "nibble-rc: Sent to Slack");

        // The wrapper's task id wins.
        let out = claude_hook_response(
            &json!({"prompt": "/nibble-rc stop", "session_id": "abc"}),
            Some("wrapped".into()),
            &|task, action| {
                assert_eq!((task, action), ("wrapped", RequestAction::Stop));
                bail!("daemon down")
            },
        )
        .unwrap();
        assert_eq!(out["reason"], "nibble-rc failed: daemon down");

        let out = claude_hook_response(
            &json!({"prompt": "/nibble-rc bogus"}),
            None,
            &|_, _| panic!(),
        )
        .unwrap();
        assert_eq!(out["reason"], USAGE);
        let out =
            claude_hook_response(&json!({"prompt": "/nibble-rc"}), None, &|_, _| panic!()).unwrap();
        assert!(out["reason"].as_str().unwrap().contains("no task id"));
    }

    #[test]
    fn requests_fail_fast_without_a_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("t.db")).unwrap();
        let err = request(&db, "t", RequestAction::Start)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not running"), "{err}");
        assert!(store::requests(&db).unwrap().is_empty());
    }

    #[test]
    fn requests_return_the_daemon_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = Database::open(&path).unwrap();
        store::beat(&db, Utc::now()).unwrap();
        // A stand-in daemon answering from another connection.
        let responder = std::thread::spawn(move || {
            let db = Database::open(&path).unwrap();
            for _ in 0..80 {
                if let Some((task, _)) = store::requests(&db).unwrap().into_iter().next() {
                    store::answer(
                        &db,
                        &task,
                        &store::Response {
                            ok: true,
                            message: "linked".into(),
                        },
                    )
                    .unwrap();
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        assert_eq!(request(&db, "t", RequestAction::Start).unwrap(), "linked");
        responder.join().unwrap();
    }
}
