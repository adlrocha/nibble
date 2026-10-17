//! `nibble vert`: open vert on the current repo, the Alt-v zellij pane.
//!
//! The keybind splits the focused pane and runs this in the new one, so the
//! working directory is the repo the user was looking at. When vert is not
//! installed, cannot start, or exits with an error, the pane stays open
//! until Enter so the message can be read before `close_on_exit` closes it.

use anyhow::{Context, Result};
use std::io::{BufRead, Write};
use std::process::{Command, ExitStatus};

/// Installer one-liner shown when vert is missing.
const VERT_INSTALL_CMD: &str =
    "curl -fsSL https://raw.githubusercontent.com/adlrocha/vert/main/install.sh | bash";

/// Run vert in the current directory and return the process exit code.
pub(crate) fn cmd_vert() -> Result<i32> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    run_vert(&mut stdin.lock(), &mut stdout.lock(), || {
        Command::new("vert").status()
    })
}

/// Launch vert through `launch` and report what happened on `out`, waiting
/// on `input` for Enter whenever there is a message to read.
fn run_vert(
    input: &mut impl BufRead,
    out: &mut impl Write,
    launch: impl FnOnce() -> std::io::Result<ExitStatus>,
) -> Result<i32> {
    match launch() {
        Ok(status) if status.success() => Ok(0),
        Ok(status) => {
            write!(
                out,
                "\nvert exited with an error (is this a git repo?). Press Enter to close."
            )?;
            wait_for_enter(input, out)?;
            Ok(status.code().unwrap_or(1))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            write!(
                out,
                "vert is not installed. Install it with:\n\n  {VERT_INSTALL_CMD}\n\nPress Enter to close."
            )?;
            wait_for_enter(input, out)?;
            Ok(127)
        }
        Err(e) => {
            write!(out, "could not start vert: {e}\n\nPress Enter to close.")?;
            wait_for_enter(input, out)?;
            Ok(126)
        }
    }
}

fn wait_for_enter(input: &mut impl BufRead, out: &mut impl Write) -> Result<()> {
    out.flush()?;
    let mut line = String::new();
    input
        .read_line(&mut line)
        .context("could not read from stdin")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn run(launch: impl FnOnce() -> std::io::Result<ExitStatus>) -> (i32, String) {
        let mut input: &[u8] = b"\n";
        let mut out = Vec::new();
        let code = run_vert(&mut input, &mut out, launch).unwrap();
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn clean_exit_prints_nothing() {
        let (code, out) = run(|| Ok(ExitStatus::from_raw(0)));
        assert_eq!(code, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn failed_exit_keeps_pane_open_and_forwards_code() {
        // Raw wait status 2 << 8 is exit code 2.
        let (code, out) = run(|| Ok(ExitStatus::from_raw(2 << 8)));
        assert_eq!(code, 2);
        assert!(out.contains("vert exited with an error"));
        assert!(out.contains("Press Enter to close."));
    }

    #[test]
    fn killed_by_signal_reports_code_one() {
        // Raw wait status 9 is "terminated by SIGKILL", which has no code.
        let (code, _) = run(|| Ok(ExitStatus::from_raw(9)));
        assert_eq!(code, 1);
    }

    #[test]
    fn missing_vert_shows_install_command() {
        let (code, out) = run(|| Err(std::io::Error::from(std::io::ErrorKind::NotFound)));
        assert_eq!(code, 127);
        assert!(out.contains("vert is not installed"));
        assert!(out.contains(VERT_INSTALL_CMD));
        assert!(out.contains("Press Enter to close."));
    }

    #[test]
    fn other_launch_errors_keep_pane_open() {
        let (code, out) = run(|| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)));
        assert_eq!(code, 126);
        assert!(out.contains("could not start vert"));
        assert!(out.contains("Press Enter to close."));
    }

    #[test]
    fn unreadable_stdin_is_an_error() {
        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("closed"))
            }
        }
        let mut input = std::io::BufReader::new(Broken);
        let mut out = Vec::new();
        let err = run_vert(&mut input, &mut out, || {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        })
        .unwrap_err();
        assert!(err.to_string().contains("could not read from stdin"));
    }
}
