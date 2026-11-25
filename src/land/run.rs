//! Runs external programs with a timeout, so tests can stand in for all of them.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Enough for any GitHub call.
pub(super) const SHORT: Duration = Duration::from_secs(120);

const POLL: Duration = Duration::from_millis(50);

/// How long output may stay open after a program exits, held by its children.
const LINGER: Duration = Duration::from_secs(10);

/// The variables a program sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Env {
    /// nibble's own, plus these.
    Inherit(BTreeMap<String, String>),
    /// Exactly these.
    Only(BTreeMap<String, String>),
}

/// One program to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub env: Env,
    pub dir: Option<PathBuf>,
    pub timeout: Duration,
}

impl Invocation {
    /// A short call with nibble's own environment.
    pub(super) fn new(program: &str, args: &[&str]) -> Invocation {
        Invocation {
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            stdin: None,
            env: Env::Inherit(BTreeMap::new()),
            dir: None,
            timeout: SHORT,
        }
    }
}

/// Runs programs, returning stdout and failing on a non-zero exit or a timeout.
pub(super) trait Runner {
    fn run(&self, invocation: &Invocation) -> Result<String>;
}

/// Runs programs for real.
pub(super) struct RealRunner;

impl Runner for RealRunner {
    fn run(&self, invocation: &Invocation) -> Result<String> {
        let name = &invocation.program;
        let mut command = Command::new(name);
        command
            .args(&invocation.args)
            .stdin(if invocation.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own group, so a timeout stops its children too.
            .process_group(0);
        match &invocation.env {
            Env::Inherit(extra) => command.envs(extra),
            Env::Only(all) => command.env_clear().envs(all),
        };
        if let Some(dir) = &invocation.dir {
            command.current_dir(dir);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start {name}"))?;

        // Every pipe gets a thread: a child that fills one while we wait on another would hang.
        let input = invocation.stdin.clone();
        let stdin = child.stdin.take();
        let writer = thread::spawn(move || {
            if let (Some(mut pipe), Some(text)) = (stdin, input) {
                // A child that stops reading early reports its own error.
                let _ = pipe.write_all(text.as_bytes());
            }
        });
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let status =
            wait(&mut child, invocation.timeout).with_context(|| format!("{name} was stopped"))?;
        let collect = |output: &mpsc::Receiver<Vec<u8>>| match output.recv_timeout(LINGER) {
            Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Err(_) => {
                stop_group(&child);
                bail!("{name} exited but left something running that holds its output open")
            }
        };
        let (stdout, stderr) = (collect(&stdout)?, collect(&stderr)?);
        drop(writer);
        if !status {
            let shown = invocation.args.first().map_or("", String::as_str);
            // Agents report failures on either stream.
            let detail = [stderr.trim(), stdout.trim()]
                .into_iter()
                .find(|text| !text.is_empty())
                .unwrap_or("no output");
            let tail: String = detail.chars().rev().take(2000).collect();
            bail!(
                "{name} {shown} failed: {}",
                tail.chars().rev().collect::<String>()
            );
        }
        Ok(stdout)
    }
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (sender, output) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            // A broken pipe ends the output; the exit status says whether that matters.
            let _ = pipe.read_to_end(&mut bytes);
        }
        // Nobody is listening once the program was stopped.
        let _ = sender.send(bytes);
    });
    output
}

/// Kills everything the program started.
fn stop_group(child: &Child) {
    if let Ok(group) = libc::pid_t::try_from(child.id()) {
        // SAFETY: a plain system call on the group this program was started in.
        // One that is already gone is fine.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
}

/// Waits for the child, killing it at the deadline. Returns whether it succeeded.
fn wait(child: &mut Child, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        if Instant::now() >= deadline {
            stop_group(child);
            child.wait()?;
            bail!("it took longer than {}s", timeout.as_secs());
        }
        thread::sleep(POLL);
    }
}
