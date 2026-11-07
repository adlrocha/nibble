//! The outbound relay: what a linked session's state means for its Slack
//! thread. A pure state machine ([`next_step`]) decides; [`Daemon::relay`]
//! carries the effects out.

use anyhow::Result;

use super::daemon::Daemon;
use super::slack::{self, SessionStatus};
use super::store::{self, Link, Watch};
use super::transcript;
use crate::models::{Task, TaskStatus};

/// Polls an idle task may show no new reply before the turn is considered
/// over without one (the transcript can lag the idle report; and a message
/// that never started a turn must not wait forever).
const IDLE_GIVE_UP: u32 = 20;

impl Daemon {
    pub(super) fn relay(&mut self, mut link: Link) -> Result<()> {
        let Some(task) = self.db.get_task_by_id(&link.task_id)? else {
            return self.close_link(&link.task_id, "The session is gone from nibble.");
        };
        // An agent killed without its exit report leaves a Running row;
        // its dead process means the session is over.
        let observed = if task.pid.is_some_and(|pid| !crate::status::pid_alive(pid)) {
            Watch::Exited
        } else {
            observe(&task)
        };
        let step = next_step(&link, observed, &|| self.current_reply(&task));
        // Record the new state (the reply included) before acting on it: a
        // post that fails or times out after Slack delivered it must not be
        // retried on the next poll and appear twice.
        if step.changed(&link) {
            link.watch = step.watch;
            link.idle_checks = step.idle_checks;
            if let Some(id) = &step.reply_id {
                link.last_reply_id = Some(id.clone());
            }
            // Re-read before writing: a reply may have closed the link meanwhile.
            if store::link(&self.db, &link.task_id)?.is_some_and(|l| !l.closed) {
                store::put_link(&self.db, &link)?;
            }
        }
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
                Effect::Reply(reply) => {
                    if let Err(e) = self.post_reply(&link, &reply.text) {
                        self.note(
                            &link.channel,
                            &link.thread_ts,
                            &format!(
                                ":warning: Could not post the reply ({e:#}); it is in the pane."
                            ),
                        );
                    }
                }
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
        Ok(())
    }
}

/// The task's state as the relay sees it.
pub(super) fn observe(task: &Task) -> Watch {
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
        link.watch != self.watch || link.idle_checks != self.idle_checks || self.reply_id.is_some()
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
        Watch::Working if prev == Watch::Working => Step {
            watch: Watch::Working,
            idle_checks: 0,
            reply_id: None,
            effects: vec![],
        },
        Watch::Working => same(vec![Effect::Status(status_of(Watch::Working))]),
        Watch::Blocked if prev == Watch::Blocked => same(vec![]),
        Watch::Blocked => same(vec![
            Effect::Blocked,
            Effect::Status(status_of(Watch::Blocked)),
        ]),
        Watch::Idle if prev == Watch::Idle => same(vec![]),
        Watch::Idle => {
            match reply().filter(|r| link.last_reply_id.as_deref() != Some(r.id.as_str())) {
                Some(r) => Step {
                    watch: Watch::Idle,
                    idle_checks: 0,
                    reply_id: Some(r.id.clone()),
                    effects: vec![Effect::Reply(r), Effect::Status(status_of(Watch::Idle))],
                },
                None if link.idle_checks + 1 >= IDLE_GIVE_UP => same(vec![
                    Effect::NoReply,
                    Effect::Status(status_of(Watch::Idle)),
                ]),
                // Not yet: keep the previous state and count the miss.
                None => Step {
                    watch: prev,
                    idle_checks: link.idle_checks + 1,
                    reply_id: None,
                    effects: vec![],
                },
            }
        }
    }
}

/// The link state right after a thread reply was typed into the pane: a
/// turn is expected (a fast one may finish between two polls), except that
/// an answer to a permission prompt stays `Blocked` until the agent reports
/// again, so the prompt is not posted a second time.
pub(super) fn after_reply(watch: Watch) -> Watch {
    match watch {
        Watch::Blocked => Watch::Blocked,
        _ => Watch::Working,
    }
}

/// The Slack session status that matches a relay state.
pub(super) fn status_of(watch: Watch) -> SessionStatus {
    match watch {
        Watch::Working => SessionStatus::Processing,
        Watch::Blocked => SessionStatus::Suspended,
        Watch::Idle | Watch::Exited => SessionStatus::Active,
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::task;
    use super::*;
    use chrono::Utc;

    fn link(watch: Watch, last: Option<&str>, idle_checks: u32) -> Link {
        let mut l = crate::rc::test_support::link("t", "1.1");
        l.watch = watch;
        l.last_reply_id = last.map(str::to_string);
        l.idle_checks = idle_checks;
        l
    }

    fn reply(id: &str) -> Option<transcript::Reply> {
        Some(transcript::Reply {
            id: id.into(),
            text: format!("text {id}"),
        })
    }

    #[test]
    fn finished_turn_posts_the_new_reply_once() {
        let l = link(Watch::Working, Some("old"), 0);
        let s = next_step(&l, Watch::Idle, &|| reply("new"));
        assert_eq!(s.watch, Watch::Idle);
        assert_eq!(s.reply_id.as_deref(), Some("new"));
        assert_eq!(s.effects[0], Effect::Reply(reply("new").unwrap()));
        assert_eq!(s.effects[1], Effect::Status(SessionStatus::Active));

        // Already idle: nothing, and the transcript is not even read.
        let l = link(Watch::Idle, Some("new"), 0);
        let s = next_step(&l, Watch::Idle, &|| panic!("must not read"));
        assert!(s.effects.is_empty());
        assert!(!s.changed(&l));
    }

    #[test]
    fn idle_without_a_new_reply_waits_then_gives_up() {
        let l = link(Watch::Working, Some("same"), 0);
        let s = next_step(&l, Watch::Idle, &|| reply("same"));
        assert_eq!((s.watch, s.idle_checks), (Watch::Working, 1));
        assert!(s.effects.is_empty());
        let l = link(Watch::Working, Some("same"), IDLE_GIVE_UP - 1);
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
        let s = next_step(&link(Watch::Idle, None, 0), Watch::Working, &|| None);
        assert_eq!(s.effects, vec![Effect::Status(SessionStatus::Processing)]);
        let s = next_step(&link(Watch::Working, None, 3), Watch::Working, &|| None);
        assert!(s.effects.is_empty());
        assert_eq!(s.idle_checks, 0);

        let s = next_step(&link(Watch::Working, None, 0), Watch::Blocked, &|| None);
        assert_eq!(
            s.effects,
            vec![Effect::Blocked, Effect::Status(SessionStatus::Suspended)]
        );
        let s = next_step(&link(Watch::Blocked, None, 0), Watch::Blocked, &|| None);
        assert!(s.effects.is_empty());
        // Approved, then finished: the reply is posted.
        let s = next_step(&link(Watch::Blocked, None, 0), Watch::Idle, &|| reply("r"));
        assert!(matches!(s.effects[0], Effect::Reply(_)));

        let s = next_step(&link(Watch::Working, None, 0), Watch::Exited, &|| None);
        assert_eq!(s.effects, vec![Effect::Close]);
    }

    #[test]
    fn answering_a_permission_prompt_does_not_repost_it() {
        let watch = after_reply(Watch::Blocked);
        // The agent has not reported yet: still blocked, nothing to post.
        let s = next_step(&link(watch, None, 0), Watch::Blocked, &|| None);
        assert!(s.effects.is_empty());
        // Ordinary replies expect a turn.
        assert_eq!(after_reply(Watch::Idle), Watch::Working);
    }

    #[test]
    fn status_mapping_is_shared() {
        assert_eq!(status_of(Watch::Working), SessionStatus::Processing);
        assert_eq!(status_of(Watch::Blocked), SessionStatus::Suspended);
        assert_eq!(status_of(Watch::Idle), SessionStatus::Active);
    }
}
