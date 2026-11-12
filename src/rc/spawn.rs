//! Sessions started from Slack: open a zellij tab running the agent, wait
//! for the agent to register in that pane and report its session, then
//! type the first message and link the thread.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::path::Path;

use super::daemon::{card_for, endable_location, log, session_card, Daemon};
use super::modal::NewSession;
use super::pane;
use super::slack::{self, CardState, SessionStatus};
use super::store::{self, Link, Location, PendingSpawn, SpawnAgent, Watch};
use crate::models::{AgentType, Task, TaskStatus};
use crate::mux::sh_quote;
use crate::pane_input;

/// Grace period after an agent reports its session before the first
/// message is typed, so its TUI is reading input.
const READY_GRACE_SECS: i64 = 2;

impl Daemon {
    pub(super) fn start_session(&mut self, new: &NewSession) -> Result<()> {
        let channel = new.origin.channel.clone();
        if let Some(ts) = &new.origin.thread_ts {
            if thread_busy(
                store::spawn_for_thread(&self.state, ts)?.is_some(),
                store::link_by_thread(&self.state, &channel, ts)?.as_ref(),
            ) {
                anyhow::bail!("this thread already has a session; start another from a new message or `/nibble`");
            }
        }
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
        let wrappers = crate::mux::wrappers_dir()?;
        if new.location == Location::Host {
            // Without it the tab would be a bare shell and the thread would
            // wait out the start timeout.
            crate::mux::require_wrapper(&wrappers.join(wrapper_name(new.agent)))?;
        }
        let session = self.cfg.zellij_session.clone();
        crate::mux::ensure_background_session(&self.db, &session)?;
        let script = launch_script(
            new.location,
            new.agent,
            &new.cwd,
            &std::env::current_exe().context("cannot locate nibble binary")?,
            &wrappers,
            &std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
        );
        let tab_name = tab_name_for(&new.cwd);
        let pane_id = pane::open_tab(&session, &tab_name, &cwd, &script)?;
        store::put_spawn(
            &self.state,
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
                typed: 0,
                baseline_reply: None,
                last_error: None,
            },
        )
    }

    /// Advance one pending spawn: find its agent, wait for its TUI, hand it
    /// the first message and link the thread.
    pub(super) fn advance_spawn(
        &mut self,
        mut spawn: PendingSpawn,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let timeout =
            chrono::Duration::seconds(i64::try_from(self.cfg.start_timeout_secs).unwrap_or(240));
        if now - spawn.started_at > timeout {
            store::delete_spawn(&self.state, &spawn.thread_ts)?;
            self.note(
                &spawn.channel,
                &spawn.thread_ts,
                &format!(
                    ":warning: The session did not start within {}s{}. Look at tab `{}` in zellij `{}`.",
                    self.cfg.start_timeout_secs,
                    spawn
                        .last_error
                        .as_deref()
                        .map(|e| format!(" (last error: {})", slack::escape(e)))
                        .unwrap_or_default(),
                    spawn.tab_name,
                    spawn.zellij_session
                ),
            );
            return Ok(());
        }
        if spawn.task_id.is_none() {
            let tasks = self.db.list_tasks()?;
            spawn.task_id = find_spawned_task(&tasks, &spawn).map(|t| t.task_id.clone());
            if spawn.task_id.is_none() {
                if !pane_input::pane_alive(&spawn.zellij_session, spawn.pane_id).unwrap_or(true) {
                    store::delete_spawn(&self.state, &spawn.thread_ts)?;
                    self.note(
                        &spawn.channel,
                        &spawn.thread_ts,
                        ":warning: The agent's pane closed before it started.",
                    );
                    return Ok(());
                }
                return store::put_spawn(&self.state, &spawn);
            }
        }
        let task_id = spawn.task_id.clone().unwrap_or_default();
        let Some(task) = self.db.get_task_by_id(&task_id)? else {
            spawn.task_id = None;
            return store::put_spawn(&self.state, &spawn);
        };
        if spawn.ready_at.is_none() {
            if !session_known(&task) {
                return store::put_spawn(&self.state, &spawn);
            }
            spawn.ready_at = Some(now);
            return store::put_spawn(&self.state, &spawn);
        }
        if spawn
            .ready_at
            .is_some_and(|r| (now - r).num_seconds() < READY_GRACE_SECS)
        {
            return Ok(());
        }
        // The agent may have quit during the grace period: then the pane is
        // the user's shell and the message would run as a command.
        if !pane::agent_present(&task, &spawn.zellij_session, spawn.pane_id)? {
            store::delete_spawn(&self.state, &spawn.thread_ts)?;
            self.note(
                &spawn.channel,
                &spawn.thread_ts,
                ":warning: The agent quit before its first message, so nothing was typed.",
            );
            return Ok(());
        }
        // What counts as "already posted" is fixed before typing, so a
        // replay after the agent answered still relays that answer.
        if spawn.baseline_reply.is_none() {
            spawn.baseline_reply = Some(self.current_turn(&task).newest_id());
            store::put_spawn(&self.state, &spawn)?;
        }
        // Type the first message and anything queued, recording progress so
        // a retry (after a transient zellij or database error) resumes where
        // it stopped instead of typing a message twice.
        let messages: Vec<String> = std::iter::once(spawn.prompt.clone())
            .chain(spawn.queued.iter().cloned())
            .collect();
        while spawn.typed < messages.len() {
            pane_input::send_text(&spawn.zellij_session, spawn.pane_id, &messages[spawn.typed])?;
            spawn.typed += 1;
            store::put_spawn(&self.state, &spawn)?;
        }
        // A replay that already linked the thread reuses that link.
        let existing = reusable_link(store::link(&self.state, &task.task_id)?, &spawn.thread_ts);
        let mut link = match existing {
            Some(link) => link,
            None => {
                let card = card_for(&task, &spawn.zellij_session);
                let mut link = Link::new(
                    &task.task_id,
                    &spawn.channel,
                    &spawn.thread_ts,
                    card,
                    Watch::Working,
                );
                link.last_reply_id = spawn.baseline_reply.clone().flatten();
                link.from_slack = true;
                link.permalink = self.bot.permalink(&spawn.channel, &spawn.thread_ts).ok();
                store::put_link(&self.state, &link)?;
                link
            }
        };
        store::delete_spawn(&self.state, &spawn.thread_ts)?;
        if link.card_ts.is_some() {
            return Ok(());
        }
        // From here on the session is linked; the card is best effort. The
        // thread root is your message or the prompt, so the card is a reply:
        // remember it, Close rewrites it.
        let session_card = session_card(&link, Some(&spawn.prompt));
        match self.bot.post(
            &link.channel,
            Some(&link.thread_ts),
            &slack::session_card_text(&session_card),
            &slack::session_card_blocks(
                &session_card,
                CardState::Open {
                    endable: endable_location(&link),
                },
            ),
        ) {
            Ok(card_ts) => {
                link.card_ts = Some(card_ts);
                store::put_link(&self.state, &link)?;
            }
            Err(e) => log(&format!("could not post the session card: {e:#}")),
        }
        self.session_status(&link.channel, &link.thread_ts, SessionStatus::Processing);
        self.session_title(
            &link.channel,
            &link.thread_ts,
            &slack::preview(&spawn.prompt),
        );
        Ok(())
    }
}

/// The task's link when a replayed start already created it for this thread.
fn reusable_link(existing: Option<Link>, thread_ts: &str) -> Option<Link> {
    existing.filter(|l| !l.closed && l.thread_ts == thread_ts)
}

/// A thread holds one session: a start is refused while one is starting or
/// open there (pending starts are keyed by thread).
fn thread_busy(pending: bool, link: Option<&Link>) -> bool {
    pending || link.is_some_and(|l| !l.closed)
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
            pane_input::target(t)
                .is_some_and(|(s, p)| s == spawn.zellij_session && p == spawn.pane_id)
        })
        .max_by_key(|t| {
            let window = t
                .context
                .as_ref()
                .is_some_and(|c| c.extra.contains_key("parent_task_id"));
            (window, t.created_at)
        })
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
            let args = match agent {
                SpawnAgent::Claude => " --fresh",
                SpawnAgent::Pi => "",
            };
            format!(
                "{}{args}",
                sh_quote(&wrappers.join(wrapper_name(agent)).to_string_lossy())
            )
        }
        Location::Sandbox => {
            let flag = match agent {
                SpawnAgent::Claude => "",
                SpawnAgent::Pi => " --pi",
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

/// The nibble wrapper a host session of `agent` starts through.
fn wrapper_name(agent: SpawnAgent) -> &'static str {
    match agent {
        SpawnAgent::Claude => "claude-wrapper",
        SpawnAgent::Pi => "pi-wrapper",
    }
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
    use super::super::test_support::task;
    use super::*;
    use serde_json::json;

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
            typed: 0,
            baseline_reply: None,
            last_error: None,
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
    fn a_replayed_start_reuses_its_link() {
        let mut l = crate::rc::test_support::link("t", "1.1");
        l.card_ts = Some("1.2".into());
        let reused = reusable_link(Some(l.clone()), "1.1").unwrap();
        assert_eq!(reused.card_ts.as_deref(), Some("1.2"), "no second card");
        assert!(reusable_link(Some(l.clone()), "9.9").is_none());
        l.closed = true;
        assert!(reusable_link(Some(l), "1.1").is_none());
        assert!(reusable_link(None, "1.1").is_none());
    }

    #[test]
    fn one_session_per_thread() {
        assert!(thread_busy(true, None));
        assert!(!thread_busy(false, None));
        let mut l = crate::rc::test_support::link("t", "1.1");
        assert!(thread_busy(false, Some(&l)));
        l.closed = true;
        assert!(
            !thread_busy(false, Some(&l)),
            "a closed thread can host a new session"
        );
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
            launch_script(Location::Host, SpawnAgent::Pi, cwd, bin, wr, "/bin/zsh"),
            "'/w/pi-wrapper'; exec '/bin/zsh'"
        );
        assert_eq!(
            launch_script(Location::Sandbox, SpawnAgent::Pi, cwd, bin, wr, "/bin/sh"),
            "'/bin/nib ble' sandbox attach '/r/it'\\''s' --btw --pi; exec '/bin/sh'"
        );
        assert_eq!(tab_name_for(cwd), "it's (slack)");
    }
}
