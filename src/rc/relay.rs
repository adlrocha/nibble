//! The outbound relay: what a linked session's state means for its Slack
//! thread. A pure state machine ([`next_step`]) decides; [`Daemon::relay`]
//! carries the effects out.

use anyhow::Result;

use super::daemon::{log, Daemon};
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
        let prompt = prompt_key(&task);
        // A running turn is re-read only when its transcript grew; a change
        // of state (above all, the turn going idle) always reads it.
        let len = crate::session::files::session_file_for(&task)
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len());
        let fresh = observed != link.watch
            || len.is_none()
            || self.transcript_lens.get(&task.task_id) != len.as_ref();
        let step = next_step(&link, observed, prompt.as_deref(), &|| {
            if fresh {
                self.current_turn(&task)
            } else {
                transcript::Turn::default()
            }
        });
        if let Some(len) = len {
            self.transcript_lens.insert(task.task_id.clone(), len);
        }
        // Record the new state (the reply included) before acting on it: a
        // post that fails or times out after Slack delivered it must not be
        // retried on the next poll and appear twice.
        if step.changed(&link) {
            link.watch = step.watch;
            link.idle_checks = step.idle_checks;
            if let Some(id) = &step.reply_id {
                link.last_reply_id = Some(id.clone());
            }
            link.blocked_reason = step.blocked_reason.clone();
            // Re-read before writing: a reply may have closed the link meanwhile.
            if store::link(&self.state, &link.task_id)?.is_some_and(|l| !l.closed) {
                store::put_link(&self.state, &link)?;
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
                Effect::Progress(text) => {
                    if let Err(e) = self.post_progress(&link, &text.text) {
                        log(&format!("could not post progress: {e:#}"));
                    }
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

/// Identifies the permission prompt a blocked task waits on. The reason
/// alone repeats (Claude reports `Permission required: Bash` for every Bash
/// prompt), but every `blocked` report stamps `completed_at`, so two
/// prompts never share a key.
pub(super) fn prompt_key(task: &Task) -> Option<String> {
    let reason = task.attention_reason.as_deref()?;
    let at = task
        .completed_at
        .map(|t| t.timestamp_millis())
        .unwrap_or_default();
    Some(format!("{reason} @{at}"))
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
    /// A text the turn went on from.
    Progress(transcript::Reply),
    Reply(transcript::Reply),
    /// The turn ended without any text to relay.
    NoReply,
    Close,
}

#[derive(Debug, PartialEq)]
struct Step {
    watch: Watch,
    idle_checks: u32,
    /// The newest text posted in this step.
    reply_id: Option<String>,
    /// The permission prompt the thread shows after this step.
    blocked_reason: Option<String>,
    effects: Vec<Effect>,
}

impl Step {
    fn changed(&self, link: &Link) -> bool {
        link.watch != self.watch
            || link.idle_checks != self.idle_checks
            || self.reply_id.is_some()
            || link.blocked_reason != self.blocked_reason
    }
}

/// What to relay for a link whose task is now `observed` (`reason`: the
/// task's attention reason when blocked). While a turn runs, each text it
/// moves on from is posted as progress; once it is idle, the rest and its
/// reply. The transcript is only read when a turn is running or may have
/// just finished.
fn next_step(
    link: &Link,
    observed: Watch,
    reason: Option<&str>,
    turn: &dyn Fn() -> transcript::Turn,
) -> Step {
    let prev = link.watch;
    let shown = (observed == Watch::Blocked).then(|| reason.unwrap_or_default().to_string());
    if observed == Watch::Exited {
        return Step {
            watch: observed,
            idle_checks: 0,
            reply_id: None,
            blocked_reason: None,
            effects: vec![Effect::Close],
        };
    }
    if observed == Watch::Idle && prev == Watch::Idle {
        return Step {
            watch: observed,
            idle_checks: 0,
            reply_id: None,
            blocked_reason: None,
            effects: vec![],
        };
    }
    let new = turn().since(link.last_reply_id.as_deref());
    let mut reply_id = new.progress.last().map(|r| r.id.clone());
    let mut effects: Vec<Effect> = new.progress.into_iter().map(Effect::Progress).collect();
    let step = |watch, idle_checks, reply_id, blocked_reason, effects| Step {
        watch,
        idle_checks,
        reply_id,
        blocked_reason,
        effects,
    };
    match observed {
        Watch::Working if prev != Watch::Working => {
            effects.push(Effect::Status(status_of(Watch::Working)));
            step(observed, 0, reply_id, shown, effects)
        }
        // Still the prompt the thread shows: nothing new. A different
        // prompt in the same blocked spell is posted.
        Watch::Blocked if !(prev == Watch::Blocked && link.blocked_reason == shown) => {
            effects.extend([Effect::Blocked, Effect::Status(status_of(Watch::Blocked))]);
            step(observed, 0, reply_id, shown, effects)
        }
        Watch::Working | Watch::Blocked => step(observed, 0, reply_id, shown, effects),
        _ => match new.reply {
            Some(r) => {
                reply_id = Some(r.id.clone());
                effects.extend([Effect::Reply(r), Effect::Status(status_of(Watch::Idle))]);
                step(Watch::Idle, 0, reply_id, None, effects)
            }
            None if link.idle_checks + 1 >= IDLE_GIVE_UP => {
                effects.extend([Effect::NoReply, Effect::Status(status_of(Watch::Idle))]);
                step(Watch::Idle, 0, reply_id, None, effects)
            }
            // Not yet: keep the previous state and count the miss.
            None => step(
                prev,
                link.idle_checks + 1,
                reply_id,
                link.blocked_reason.clone(),
                effects,
            ),
        },
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

    /// A turn with these progress texts and, if not empty, this reply.
    fn done(progress: &[&str], last: &str) -> transcript::Turn {
        transcript::Turn {
            progress: progress.iter().filter_map(|id| reply(id)).collect(),
            reply: Some(last).filter(|id| !id.is_empty()).and_then(reply),
        }
    }

    #[test]
    fn a_running_turn_streams_each_text_it_moves_on_from_once() {
        // The turn posts "a" while working; the reply candidate "b" waits.
        let l = link(Watch::Working, Some("old"), 0);
        let s = next_step(&l, Watch::Working, None, &|| done(&["a"], "b"));
        assert_eq!(s.effects, vec![Effect::Progress(reply("a").unwrap())]);
        assert_eq!(
            (s.watch, s.reply_id.as_deref()),
            (Watch::Working, Some("a"))
        );
        assert!(s.changed(&l));
        // Read again with nothing new: nothing posted, nothing changed.
        let l = link(Watch::Working, Some("a"), 0);
        let s = next_step(&l, Watch::Working, None, &|| done(&["a"], "b"));
        assert!(s.effects.is_empty());
        assert!(!s.changed(&l));
        // A permission prompt: the text before it, then the prompt.
        let s = next_step(&l, Watch::Blocked, Some("Bash"), &|| done(&["a", "c"], ""));
        assert_eq!(
            s.effects,
            vec![
                Effect::Progress(reply("c").unwrap()),
                Effect::Blocked,
                Effect::Status(SessionStatus::Suspended)
            ]
        );
        // The turn ends: only what was not posted yet, then the reply.
        let l = link(Watch::Working, Some("c"), 0);
        let s = next_step(&l, Watch::Idle, None, &|| done(&["a", "c", "d"], "e"));
        assert_eq!(
            s.effects,
            vec![
                Effect::Progress(reply("d").unwrap()),
                Effect::Reply(reply("e").unwrap()),
                Effect::Status(SessionStatus::Active)
            ]
        );
        assert_eq!(s.reply_id.as_deref(), Some("e"));
    }

    #[test]
    fn an_idle_report_before_the_final_message_waits_for_it() {
        // The race: idle is reported, the final message isn't on disk yet.
        let l = link(Watch::Working, Some("old"), 0);
        let s = next_step(&l, Watch::Idle, None, &|| done(&["a"], ""));
        assert_eq!(s.effects, vec![Effect::Progress(reply("a").unwrap())]);
        assert_eq!((s.watch, s.idle_checks), (Watch::Working, 1));
        assert_eq!(s.reply_id.as_deref(), Some("a"));
        // Next poll: it landed and is posted as the reply.
        let l = link(Watch::Working, Some("a"), 1);
        let s = next_step(&l, Watch::Idle, None, &|| done(&["a"], "z"));
        assert_eq!(
            s.effects,
            vec![
                Effect::Reply(reply("z").unwrap()),
                Effect::Status(SessionStatus::Active)
            ]
        );
    }

    #[test]
    fn finished_turn_posts_the_new_reply_once() {
        let l = link(Watch::Working, Some("old"), 0);
        let s = next_step(&l, Watch::Idle, None, &|| done(&[], "new"));
        assert_eq!(s.watch, Watch::Idle);
        assert_eq!(s.reply_id.as_deref(), Some("new"));
        assert_eq!(s.effects[0], Effect::Reply(reply("new").unwrap()));
        assert_eq!(s.effects[1], Effect::Status(SessionStatus::Active));

        // Already idle: nothing, and the transcript is not even read.
        let l = link(Watch::Idle, Some("new"), 0);
        let s = next_step(&l, Watch::Idle, None, &|| panic!("must not read"));
        assert!(s.effects.is_empty());
        assert!(!s.changed(&l));
    }

    #[test]
    fn idle_without_a_new_reply_waits_then_gives_up() {
        let l = link(Watch::Working, Some("same"), 0);
        let s = next_step(&l, Watch::Idle, None, &|| done(&[], "same"));
        assert_eq!((s.watch, s.idle_checks), (Watch::Working, 1));
        assert!(s.effects.is_empty());
        let l = link(Watch::Working, Some("same"), IDLE_GIVE_UP - 1);
        let s = next_step(&l, Watch::Idle, None, &transcript::Turn::default);
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
        let s = next_step(
            &link(Watch::Idle, None, 0),
            Watch::Working,
            None,
            &transcript::Turn::default,
        );
        assert_eq!(s.effects, vec![Effect::Status(SessionStatus::Processing)]);
        let s = next_step(
            &link(Watch::Working, None, 3),
            Watch::Working,
            None,
            &transcript::Turn::default,
        );
        assert!(s.effects.is_empty());
        assert_eq!(s.idle_checks, 0);

        let s = next_step(
            &link(Watch::Working, None, 0),
            Watch::Blocked,
            None,
            &transcript::Turn::default,
        );
        assert_eq!(
            s.effects,
            vec![Effect::Blocked, Effect::Status(SessionStatus::Suspended)]
        );
        let mut shown = link(Watch::Blocked, None, 0);
        shown.blocked_reason = Some("Needs input".into());
        let s = next_step(
            &shown,
            Watch::Blocked,
            Some("Needs input"),
            &transcript::Turn::default,
        );
        assert!(s.effects.is_empty());
        // Approved, then finished: the reply is posted.
        let s = next_step(&link(Watch::Blocked, None, 0), Watch::Idle, None, &|| {
            done(&[], "r")
        });
        assert!(matches!(s.effects[0], Effect::Reply(_)));

        let s = next_step(
            &link(Watch::Working, None, 0),
            Watch::Exited,
            None,
            &transcript::Turn::default,
        );
        assert_eq!(s.effects, vec![Effect::Close]);
    }

    #[test]
    fn answering_a_permission_prompt_does_not_repost_it() {
        let watch = after_reply(Watch::Blocked);
        // The agent has not reported yet: still blocked, nothing to post.
        let mut l = link(watch, None, 0);
        l.blocked_reason = Some("Permission required: Bash".into());
        let s = next_step(
            &l,
            Watch::Blocked,
            Some("Permission required: Bash"),
            &transcript::Turn::default,
        );
        assert!(s.effects.is_empty());
        // Ordinary replies expect a turn.
        assert_eq!(after_reply(Watch::Idle), Watch::Working);
    }

    #[test]
    fn a_new_permission_prompt_while_blocked_is_posted() {
        let mut l = link(Watch::Blocked, None, 0);
        l.blocked_reason = Some("Permission required: Bash".into());
        let same = next_step(
            &l,
            Watch::Blocked,
            Some("Permission required: Bash"),
            &transcript::Turn::default,
        );
        assert!(same.effects.is_empty());
        let next = next_step(
            &l,
            Watch::Blocked,
            Some("Permission required: Edit"),
            &transcript::Turn::default,
        );
        assert_eq!(next.effects[0], Effect::Blocked);
        assert_eq!(
            next.blocked_reason.as_deref(),
            Some("Permission required: Edit")
        );
        // Leaving the blocked state forgets the prompt.
        let working = next_step(&l, Watch::Working, None, &transcript::Turn::default);
        assert_eq!(working.blocked_reason, None);
        assert!(working.changed(&l));
    }

    #[test]
    fn repeated_prompts_for_one_tool_get_distinct_keys() {
        let mut t = task("t", 1, "main", Utc::now(), false);
        assert_eq!(prompt_key(&t), None);
        t.attention_reason = Some("Permission required: Bash".into());
        t.completed_at = Some(Utc::now());
        let first = prompt_key(&t);
        t.completed_at = Some(Utc::now() + chrono::Duration::seconds(3));
        let second = prompt_key(&t);
        assert_ne!(first, second);
        // The second prompt is posted although the reason text is the same.
        let mut l = link(Watch::Blocked, None, 0);
        l.blocked_reason = first;
        let s = next_step(
            &l,
            Watch::Blocked,
            second.as_deref(),
            &transcript::Turn::default,
        );
        assert_eq!(s.effects[0], Effect::Blocked);
    }

    #[test]
    fn status_mapping_is_shared() {
        assert_eq!(status_of(Watch::Working), SessionStatus::Processing);
        assert_eq!(status_of(Watch::Blocked), SessionStatus::Suspended);
        assert_eq!(status_of(Watch::Idle), SessionStatus::Active);
    }
}
