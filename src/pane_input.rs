//! Typing into an agent's zellij pane from outside its session, shared by
//! quota-watch (the continue message) and `nibble rc` (Slack replies).
//!
//! Text goes through `zellij action write-chars`, so it reaches the live
//! TUI. Multi-line text is sent as a bracketed paste (Claude Code and pi
//! both support it) so embedded newlines don't submit early; a single line
//! goes as plain keystrokes so it also answers a permission menu.

use anyhow::{bail, Context, Result};
use std::process::Command;

use crate::models::Task;
use crate::status::{list_zellij_panes, PaneListError};

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

/// Run `zellij --session <session> <args>`, returning trimmed stdout.
pub(crate) fn zellij(session: &str, args: &[&str]) -> Result<String> {
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
}
