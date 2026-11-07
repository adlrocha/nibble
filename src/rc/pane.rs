//! Driving an agent's zellij pane from outside its session.
//!
//! Messages are typed into the live TUI with `zellij action write-chars`,
//! the same channel quota-watch uses, so the session in the pane is the one
//! Slack talks to. Multi-line text is sent as a bracketed paste (Claude Code
//! and pi both support it) so embedded newlines don't submit early; a single
//! line goes as plain keystrokes so it also answers a permission menu.

use anyhow::{bail, Context, Result};
use std::process::Command;

use crate::models::Task;
use crate::status::{list_zellij_panes, PaneListError, ZellijPane};

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// The characters to type for `text`: normalised newlines, bracketed when
/// the text spans lines. `None` for blank text.
pub(crate) fn encode_input(text: &str) -> Option<String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    // Strip the paste markers themselves: they would end the paste early.
    let text = text.replace(PASTE_START, "").replace(PASTE_END, "");
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(if text.contains('\n') {
        format!("{PASTE_START}{text}{PASTE_END}")
    } else {
        text.to_string()
    })
}

fn zellij(session: &str, args: &[&str]) -> Result<String> {
    let out = Command::new("zellij")
        .arg("--session")
        .arg(session)
        .args(args)
        .output()
        .context("zellij is not installed or not on PATH")?;
    if !out.status.success() {
        bail!(
            "zellij {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The zellij session and pane hosting a task's agent.
pub(crate) fn target(task: &Task) -> Option<(String, u32)> {
    let session = crate::status::session_of(task)?.to_string();
    let pane = crate::status::pane_id_of(task)?;
    Some((session, pane))
}

/// Whether terminal pane `pane` still exists in `session`.
pub(crate) fn pane_alive(session: &str, pane: u32) -> Result<bool> {
    match list_zellij_panes(Some(session)) {
        Ok(panes) => Ok(panes.iter().any(|p| p.id == pane && !p.is_plugin)),
        Err(PaneListError::Gone(_)) => Ok(false),
        Err(PaneListError::Failed(e)) => bail!("cannot list zellij panes: {e}"),
    }
}

/// Type `text` into the pane and press Enter.
pub(crate) fn send_text(session: &str, pane: u32, text: &str) -> Result<()> {
    let Some(chars) = encode_input(text) else {
        return Ok(());
    };
    let pane = pane.to_string();
    // `--` so text starting with `-` (a bullet list) is not read as a flag.
    zellij(
        session,
        &["action", "write-chars", "--pane-id", &pane, "--", &chars],
    )?;
    zellij(session, &["action", "write", "--pane-id", &pane, "13"])?;
    Ok(())
}

/// Press Escape in the pane: interrupts the agent's current turn.
pub(crate) fn send_escape(session: &str, pane: u32) -> Result<()> {
    let pane = pane.to_string();
    zellij(session, &["action", "write", "--pane-id", &pane, "27"]).map(drop)
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

    #[test]
    fn single_lines_are_typed_plainly() {
        assert_eq!(
            encode_input("  run the tests \n").as_deref(),
            Some("run the tests")
        );
        assert_eq!(encode_input("1").as_deref(), Some("1"));
    }

    #[test]
    fn multi_line_text_is_a_bracketed_paste() {
        assert_eq!(
            encode_input("a\r\nb").as_deref(),
            Some("\x1b[200~a\nb\x1b[201~")
        );
        // Embedded paste markers cannot end the paste early.
        assert_eq!(
            encode_input("x\x1b[201~\ny").as_deref(),
            Some("\x1b[200~x\ny\x1b[201~")
        );
    }

    #[test]
    fn blank_text_sends_nothing() {
        assert_eq!(encode_input(" \n\t"), None);
    }

    fn zpane(id: u32, tab: u32, plugin: bool, command: Option<&str>) -> ZellijPane {
        serde_json::from_value(serde_json::json!({
            "id": id, "is_plugin": plugin, "tab_id": tab, "title": "",
            "terminal_command": command
        }))
        .unwrap()
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
