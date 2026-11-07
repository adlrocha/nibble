//! Opening a zellij tab for a session started from Slack. Typing into an
//! agent's pane is [`crate::pane_input`]'s job.

use anyhow::{bail, Context, Result};

use crate::models::Task;
use crate::pane_input::zellij;
use crate::status::{list_zellij_panes, pane_at_shell, PaneListError, ZellijPane};

/// Whether the task's agent still runs in its pane: the agent process (when
/// nibble recorded its pid) is alive, and the pane exists, has not exited
/// and is not back at a shell prompt (sessions started from Slack fall back
/// to the user's shell). Typing into a pane whose agent is gone would run
/// the text as a shell command.
pub(crate) fn agent_present(task: &Task, session: &str, pane: u32) -> Result<bool> {
    if task.pid.is_some_and(|pid| !crate::status::pid_alive(pid)) {
        return Ok(false);
    }
    match list_zellij_panes(Some(session)) {
        Ok(panes) => Ok(panes.iter().any(|p| p.id == pane && agent_pane(p))),
        Err(PaneListError::Gone(_)) => Ok(false),
        Err(PaneListError::Failed(e)) => bail!("cannot list zellij panes: {e}"),
    }
}

/// A terminal pane that may still hold an agent.
fn agent_pane(p: &ZellijPane) -> bool {
    !p.is_plugin && !p.exited && !pane_at_shell(p)
}

/// Parse `terminal_<n>` (what `zellij run` prints) into a pane id.
fn parse_terminal_pane(out: &str) -> Option<u32> {
    out.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("terminal_"))
        .and_then(|n| n.parse().ok())
}

/// Open a tab named `tab_name` in `session` running `script` (a `sh -c`
/// command line) in `cwd`, then the user's shell once it exits — as if it
/// had been typed into that shell. Returns the new pane's id.
pub(crate) fn open_tab(session: &str, tab_name: &str, cwd: &str, script: &str) -> Result<u32> {
    let tab = zellij(
        session,
        &["action", "new-tab", "--name", tab_name, "--cwd", cwd],
    )?;
    let tab_id = tab
        .lines()
        .last()
        .map(str::trim)
        .filter(|t| t.chars().all(|c| c.is_ascii_digit()) && !t.is_empty())
        .with_context(|| format!("zellij new-tab printed no tab id ({tab:?})"))?
        .to_string();
    let out = zellij(
        session,
        &[
            "run",
            "--tab-id",
            &tab_id,
            "--name",
            tab_name,
            "--cwd",
            cwd,
            "--close-on-exit",
            "--",
            "sh",
            "-c",
            script,
        ],
    )?;
    let pane = parse_terminal_pane(&out)
        .with_context(|| format!("zellij run printed no pane id ({out:?})"))?;
    // With a client attached, new-tab also opened the layout's plain shell
    // pane; the agent pane replaces it. Best effort: a leftover shell is
    // harmless.
    if let (Ok(tab), Ok(panes)) = (tab_id.parse::<u32>(), list_zellij_panes(Some(session))) {
        for id in stray_shells(&panes, tab, pane) {
            let _ = zellij(
                session,
                &[
                    "action",
                    "close-pane",
                    "--pane-id",
                    &format!("terminal_{id}"),
                ],
            );
        }
    }
    Ok(pane)
}

/// Plain shell panes in `tab` other than `keep` (command panes such as the
/// nibble sidebar and plugins are left alone).
fn stray_shells(panes: &[ZellijPane], tab: u32, keep: u32) -> Vec<u32> {
    panes
        .iter()
        .filter(|p| p.tab_id == tab && !p.is_plugin && p.id != keep && p.terminal_command.is_none())
        .map(|p| p.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane_input::{pane_alive, send_text};
    use std::process::Command;

    fn zpane(id: u32, tab: u32, plugin: bool, command: Option<&str>) -> ZellijPane {
        serde_json::from_value(serde_json::json!({
            "id": id, "is_plugin": plugin, "tab_id": tab, "title": "",
            "terminal_command": command
        }))
        .unwrap()
    }

    #[test]
    fn panes_back_at_their_shell_hold_no_agent() {
        let mut p = zpane(6, 4, false, Some("sh -c 'claude'; exec '/bin/zsh'"));
        // zellij echoes the launch command while it runs: an agent.
        p.pane_command = p.terminal_command.clone();
        assert!(agent_pane(&p));
        // The launch script exec'd the user's shell: no agent any more.
        p.pane_command = Some("/bin/zsh".into());
        assert!(!agent_pane(&p));
        // A plain shell pane running claude, then back at its prompt.
        let mut q = zpane(7, 4, false, None);
        q.pane_command = Some("claude".into());
        assert!(agent_pane(&q));
        q.pane_command = Some("zsh".into());
        assert!(!agent_pane(&q));
        q.pane_command = Some("claude".into());
        q.exited = true;
        assert!(!agent_pane(&q));
    }

    #[test]
    fn a_dead_agent_process_is_not_present() {
        let mut t = Task::new(
            "t".into(),
            crate::models::AgentType::ClaudeCode,
            "x".into(),
            None,
            None,
        );
        // A pid that cannot exist: the check fails before zellij is asked.
        t.pid = Some(i32::MAX);
        assert!(!agent_present(&t, "no-such-session", 1).unwrap());
    }

    #[test]
    fn only_plain_shells_in_the_new_tab_are_stray() {
        let panes = vec![
            zpane(1, 4, false, None),                  // layout shell: stray
            zpane(6, 4, false, Some("sh -c claude")),  // the agent (kept)
            zpane(7, 4, false, Some("nibble status")), // sidebar command pane
            zpane(0, 4, true, None),                   // tab bar plugin
            zpane(2, 0, false, None),                  // shell in another tab
        ];
        assert_eq!(stray_shells(&panes, 4, 6), vec![1]);
    }

    #[test]
    fn parses_zellij_run_output() {
        assert_eq!(parse_terminal_pane("terminal_6\n"), Some(6));
        assert_eq!(parse_terminal_pane("plugin_2"), None);
        assert_eq!(parse_terminal_pane(""), None);
    }

    /// Drives a real zellij: `cargo test -- --ignored zellij_round_trip`.
    #[test]
    #[ignore = "needs zellij; starts and kills a background session"]
    fn zellij_round_trip() {
        let session = format!("nibble-rc-test-{}", std::process::id());
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("typed.txt");
        let created = Command::new("zellij")
            .args(["attach", "--create-background", &session])
            .env_remove("ZELLIJ")
            .status()
            .unwrap();
        assert!(created.success());
        let result = std::panic::catch_unwind(|| {
            let script = format!("cat > {}", out.display());
            let pane = open_tab(
                &session,
                "probe (slack)",
                &dir.path().to_string_lossy(),
                &script,
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(800));
            assert!(pane_alive(&session, pane).unwrap());
            send_text(&session, pane, "hello from slack").unwrap();
            send_text(&session, pane, "- a bullet").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(800));
            let typed = std::fs::read_to_string(&out).unwrap_or_default();
            assert!(typed.contains("hello from slack"), "typed: {typed:?}");
            assert!(typed.contains("- a bullet"), "typed: {typed:?}");
            assert!(!pane_alive(&session, 9999).unwrap());
        });
        let _ = Command::new("zellij")
            .args(["kill-session", &session])
            .status();
        let _ = Command::new("zellij")
            .args(["delete-session", &session])
            .status();
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }
}
