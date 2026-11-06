//! `nibble sdlc`: install and run the delivery flow in a repository.
//!
//! The flow is committed to the repository through a pull request rather than
//! injected into a sandbox, so it is reviewable and easy to back out:
//! `install` and `uninstall` both open a PR and never touch the checkout you
//! are in. Everything is read from the default branch on `origin`.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::Command;

use crate::cli::SdlcAction;

const ACTION_REPO: &str = "adlrocha/nibble-review";
const WORKFLOW_PATH: &str = ".github/workflows/nibble-review.yml";
const CONFIG_PATH: &str = ".nibble/sdlc.toml";
const AGENTS_PATH: &str = "AGENTS.md";
const INSTALL_BRANCH: &str = "chore/nibble-sdlc";
const UNINSTALL_BRANCH: &str = "chore/nibble-sdlc-remove";
const LAND_CONTEXT: &str = "nibble-review/land";
/// Land's description when it clears a commit, set by `statusDescription` in
/// nibble-review. Matched exactly: other descriptions quote file names.
const LAND_CLEARED: &str = "shadow: would merge";
const SECRET_NAME: &str = "OPENCODE_API_KEY";
const SECRET_UNKNOWN: &str = "Could not check the secret: listing secrets needs admin access.";
const BLOCK_BEGIN: &str = "<!-- nibble-sdlc:begin -->";
const BLOCK_END: &str = "<!-- nibble-sdlc:end -->";
const LAND_INPUT: &str = "land: \"true\"";
const STATUSES_PERMISSION: &str = "statuses: write";
const WORKFLOW_TEMPLATE: &str = include_str!("nibble-review.yml");
const AGENTS_BLOCK: &str = include_str!("agents-block.md");

/// Runs external programs, so tests can stand in for `git` and `gh`.
trait Shell {
    /// Runs `program` and returns its stdout without trailing whitespace, failing on a non-zero exit.
    fn run(&self, program: &str, args: &[&str], dir: Option<&Path>) -> Result<String>;
}

struct RealShell;

impl Shell for RealShell {
    fn run(&self, program: &str, args: &[&str], dir: Option<&Path>) -> Result<String> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        let output = command
            .output()
            .with_context(|| format!("failed to start {program}"))?;
        if !output.status.success() {
            bail!(
                "{program} {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string())
    }
}

/// The GitHub identity of the repository in the current directory.
struct Repo {
    default_branch: String,
}

/// The commit the workflow pins the action to, with a readable label.
#[derive(Debug, PartialEq)]
struct Pin {
    sha: String,
    label: String,
}

/// One file the flow creates, rewrites, or deletes.
#[derive(Debug, PartialEq)]
struct Change {
    path: String,
    /// New contents, or `None` to delete the file.
    content: Option<String>,
}

#[derive(Deserialize, Default)]
struct SdlcFile {
    #[serde(default)]
    land: LandSettings,
    #[serde(default)]
    merge: MergeSettings,
}

#[derive(Deserialize, Default)]
struct LandSettings {
    #[serde(default)]
    main_is_staging: bool,
}

#[derive(Deserialize)]
struct MergeSettings {
    #[serde(default = "default_merge_method")]
    method: String,
}

impl Default for MergeSettings {
    fn default() -> Self {
        Self {
            method: default_merge_method(),
        }
    }
}

fn default_merge_method() -> String {
    "squash".to_string()
}

/// Entry point for `nibble sdlc`; returns the process exit code.
pub(crate) fn cmd_sdlc(action: SdlcAction) -> Result<i32> {
    let shell = RealShell;
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    match action {
        SdlcAction::Install {
            dry_run,
            yes,
            action_ref,
            main_is_staging,
        } => {
            let repo = load_repo(&shell)?;
            let pin = resolve_pin(&shell, action_ref.as_deref())?;
            let changes = install_changes(&shell, &repo, &pin, main_is_staging)?;
            let publish = Publish {
                branch: INSTALL_BRANCH,
                title: "ci: install the nibble sdlc flow",
                body: "Installs nibble-review with `land` in shadow mode: each PR gets a `## Risk` comment and a `nibble-review/land` status. Nothing merges or blocks. Remove it with `nibble sdlc uninstall`.",
            };
            apply(
                &shell, &repo, &changes, &publish, dry_run, yes, &mut input, &mut out,
            )?;
            warn_missing_secret(&shell, &mut out)?;
            Ok(0)
        }
        SdlcAction::Uninstall {
            dry_run,
            yes,
            remove_reviewer,
        } => {
            let repo = load_repo(&shell)?;
            let changes = uninstall_changes(&shell, &repo, remove_reviewer)?;
            let publish = Publish {
                branch: UNINSTALL_BRANCH,
                title: "ci: remove the nibble sdlc flow",
                body: "Removes the flow installed by `nibble sdlc install`.",
            };
            apply(
                &shell, &repo, &changes, &publish, dry_run, yes, &mut input, &mut out,
            )?;
            Ok(0)
        }
        SdlcAction::Check => {
            let repo = load_repo(&shell)?;
            let checks = check_repo(&shell, &repo)?;
            for (ok, line) in &checks {
                writeln!(out, "{} {line}", if *ok { "✓" } else { "✗" })?;
            }
            Ok(i32::from(checks.iter().any(|(ok, _)| !ok)))
        }
        SdlcAction::Status => {
            write!(out, "{}", render_status(&shell)?)?;
            Ok(0)
        }
        SdlcAction::Merge { pr, auto, yes } => {
            let repo = load_repo(&shell)?;
            let method = merge_method(&shell, &repo)?;
            merge(
                &shell,
                &MergeRequest {
                    pr,
                    auto,
                    yes,
                    interactive,
                    method,
                },
                &mut input,
                &mut out,
            )?;
            Ok(0)
        }
    }
}

fn load_repo(shell: &dyn Shell) -> Result<Repo> {
    shell
        .run("git", &["rev-parse", "--show-toplevel"], None)
        .context("not inside a git repository")?;
    let default_branch = shell
        .run(
            "gh",
            &[
                "repo",
                "view",
                "--json",
                "defaultBranchRef",
                "--jq",
                ".defaultBranchRef.name",
            ],
            None,
        )
        .context("could not read the repository from GitHub")?;
    shell.run("git", &["fetch", "-q", "origin", &default_branch], None)?;
    Ok(Repo { default_branch })
}

/// Pins to `action_ref` when given, otherwise to the latest release.
fn resolve_pin(shell: &dyn Shell, action_ref: Option<&str>) -> Result<Pin> {
    let label = match action_ref {
        Some(reference) => reference.to_string(),
        None => shell.run(
            "gh",
            &[
                "api",
                &format!("repos/{ACTION_REPO}/releases/latest"),
                "--jq",
                ".tag_name",
            ],
            None,
        )?,
    };
    let sha = shell.run(
        "gh",
        &[
            "api",
            &format!("repos/{ACTION_REPO}/commits/{label}"),
            "--jq",
            ".sha",
        ],
        None,
    )?;
    Ok(Pin { sha, label })
}

/// Reads a file from the default branch on origin, or `None` when it is absent.
fn read_default(shell: &dyn Shell, repo: &Repo, path: &str) -> Result<Option<String>> {
    let tree = format!("origin/{}", repo.default_branch);
    let listed = shell.run("git", &["ls-tree", "--name-only", &tree, "--", path], None)?;
    if listed.is_empty() {
        return Ok(None);
    }
    let object = format!("{tree}:{path}");
    Ok(Some(format!(
        "{}\n",
        shell.run("git", &["show", &object], None)?
    )))
}

fn install_changes(
    shell: &dyn Shell,
    repo: &Repo,
    pin: &Pin,
    main_is_staging: bool,
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    let workflow = read_default(shell, repo, WORKFLOW_PATH)?;
    let updated = match &workflow {
        Some(text) => ensure_statuses_write(&ensure_land_input(&set_pin(text, pin)?)?)?,
        None => WORKFLOW_TEMPLATE.replace("__PIN__", &pin_text(pin)),
    };
    push_if_changed(&mut changes, WORKFLOW_PATH, workflow.as_deref(), updated);

    // An existing config is the repository's own choice, so it is never rewritten.
    if read_default(shell, repo, CONFIG_PATH)?.is_none() {
        changes.push(Change {
            path: CONFIG_PATH.to_string(),
            content: Some(render_config(main_is_staging)),
        });
    }

    let text = read_default(shell, repo, AGENTS_PATH)?;
    let updated = upsert_block(text.as_deref().unwrap_or(""), AGENTS_BLOCK);
    push_if_changed(&mut changes, AGENTS_PATH, text.as_deref(), updated);
    Ok(changes)
}

fn uninstall_changes(shell: &dyn Shell, repo: &Repo, remove_reviewer: bool) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    if let Some(text) = read_default(shell, repo, WORKFLOW_PATH)? {
        if remove_reviewer {
            changes.push(Change {
                path: WORKFLOW_PATH.to_string(),
                content: None,
            });
        } else {
            push_if_changed(
                &mut changes,
                WORKFLOW_PATH,
                Some(&text),
                remove_land_lines(&text),
            );
        }
    }
    if read_default(shell, repo, CONFIG_PATH)?.is_some() {
        changes.push(Change {
            path: CONFIG_PATH.to_string(),
            content: None,
        });
    }
    if let Some(text) = read_default(shell, repo, AGENTS_PATH)? {
        let updated = remove_block(&text);
        push_if_changed(&mut changes, AGENTS_PATH, Some(&text), updated);
    }
    Ok(changes)
}

fn push_if_changed(changes: &mut Vec<Change>, path: &str, before: Option<&str>, after: String) {
    if before != Some(after.as_str()) {
        changes.push(Change {
            path: path.to_string(),
            content: Some(after),
        });
    }
}

fn render_config(main_is_staging: bool) -> String {
    format!(
        "# Read by nibble-review from the default branch, and by `nibble sdlc`.\n\
         [land]\n\
         mode = \"shadow\"  # or \"off\"; shadow only reports and never merges or blocks\n\
         # true when main deploys to dev and a release deploys to production\n\
         main_is_staging = {main_is_staging}\n\
         # Optional, with nibble-review's defaults when left out:\n\
         # max_lines = 400\n\
         # max_review_rounds = 5\n\
         # always_human = [\"src/auth/**\"]  # extra paths that always need a person\n\
         \n\
         [merge]\n\
         method = \"squash\"  # squash, merge, or rebase; used by `nibble sdlc merge`\n"
    )
}

fn pin_text(pin: &Pin) -> String {
    format!("{} # {}", pin.sha, pin.label)
}

/// Finds the step that uses the reviewer action (not its `/fix` sub-action).
fn find_uses(lines: &[&str]) -> Option<usize> {
    let prefix = format!("uses: {ACTION_REPO}@");
    lines.iter().position(|line| {
        line.trim_start()
            .trim_start_matches("- ")
            .starts_with(&prefix)
    })
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Rejoins lines, keeping a single trailing newline.
fn join_lines(lines: &[String]) -> String {
    format!("{}\n", lines.join("\n").trim_end())
}

fn set_pin(text: &str, pin: &Pin) -> Result<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let borrowed: Vec<&str> = text.lines().collect();
    let index = find_uses(&borrowed)
        .ok_or_else(|| anyhow!("{WORKFLOW_PATH} has no step using {ACTION_REPO}"))?;
    let (head, _) = lines[index]
        .split_once('@')
        .ok_or_else(|| anyhow!("unexpected uses line in {WORKFLOW_PATH}"))?;
    lines[index] = format!("{head}@{}", pin_text(pin));
    Ok(join_lines(&lines))
}

/// Adds `land: "true"` as the first input of the reviewer step.
fn ensure_land_input(text: &str) -> Result<String> {
    let borrowed: Vec<&str> = text.lines().collect();
    let uses = find_uses(&borrowed)
        .ok_or_else(|| anyhow!("{WORKFLOW_PATH} has no step using {ACTION_REPO}"))?;
    let line = borrowed[uses];
    let key_indent = indent_of(line)
        + if line.trim_start().starts_with("- ") {
            2
        } else {
            0
        };
    let with = borrowed[uses + 1..]
        .iter()
        .take_while(|candidate| candidate.trim().is_empty() || indent_of(candidate) >= key_indent)
        .position(|candidate| candidate.trim() == "with:" && indent_of(candidate) == key_indent)
        .map(|offset| uses + 1 + offset)
        .ok_or_else(|| anyhow!("the nibble-review step in {WORKFLOW_PATH} has no `with:` block"))?;
    let block_len = borrowed[with + 1..]
        .iter()
        .take_while(|candidate| candidate.trim().is_empty() || indent_of(candidate) > key_indent)
        .count();
    let block = with + 1..with + 1 + block_len;
    let line = format!("{}{LAND_INPUT}", " ".repeat(key_indent + 2));
    Ok(set_key(text, block, "land:", with + 1, &line))
}

/// Rewrites `key` if it is in `block`, or inserts `line` at `position`. A
/// second entry would be a duplicate YAML key, which GitHub rejects.
fn set_key(
    text: &str,
    block: std::ops::Range<usize>,
    key: &str,
    position: usize,
    line: &str,
) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    match block
        .into_iter()
        .find(|index| lines[*index].trim_start().starts_with(key))
    {
        Some(index) => lines[index] = line.to_string(),
        None => lines.insert(position, line.to_string()),
    }
    join_lines(&lines)
}

/// Finds the `permissions:` line governing the step at `uses`: its job's own
/// block, which replaces the top-level one, or else the top-level block.
fn governing_permissions(lines: &[&str], uses: usize) -> Option<usize> {
    let jobs = lines.iter().position(|line| line.trim_end() == "jobs:")?;
    let is_key = |line: &&str| !line.trim().is_empty() && !line.trim_start().starts_with('#');
    let job_indent = lines[jobs + 1..]
        .iter()
        .find(|line| is_key(line))
        .map(|line| indent_of(line))?;
    let start = lines[..uses]
        .iter()
        .rposition(|line| is_key(line) && indent_of(line) == job_indent)?;
    let end = lines[start + 1..]
        .iter()
        .position(|line| is_key(line) && indent_of(line) <= job_indent)
        .map_or(lines.len(), |offset| start + 1 + offset);
    let in_job = lines[start..end]
        .iter()
        .position(|line| line.trim_end() == format!("{}permissions:", " ".repeat(job_indent + 2)))
        .map(|offset| start + offset);
    in_job.or_else(|| {
        lines[..jobs]
            .iter()
            .position(|line| line.trim_end() == "permissions:")
    })
}

/// Grants `statuses: write` in the permissions block that governs the reviewer step.
fn ensure_statuses_write(text: &str) -> Result<String> {
    let borrowed: Vec<&str> = text.lines().collect();
    let uses = find_uses(&borrowed)
        .ok_or_else(|| anyhow!("{WORKFLOW_PATH} has no step using {ACTION_REPO}"))?;
    let block = governing_permissions(&borrowed, uses).ok_or_else(|| {
        anyhow!("{WORKFLOW_PATH} has no `permissions:` block; add one with `{STATUSES_PERMISSION}`")
    })?;
    let child_indent = indent_of(borrowed[block]) + 2;
    let count = borrowed[block + 1..]
        .iter()
        .take_while(|line| indent_of(line) >= child_indent && !line.trim().is_empty())
        .count();
    let range = block + 1..block + 1 + count;
    let line = format!("{}{STATUSES_PERMISSION}", " ".repeat(child_indent));
    Ok(set_key(text, range.clone(), "statuses:", range.end, &line))
}

fn remove_land_lines(text: &str) -> String {
    let kept: Vec<String> = text
        .lines()
        .filter(|line| line.trim() != LAND_INPUT && line.trim() != STATUSES_PERMISSION)
        .map(str::to_string)
        .collect();
    join_lines(&kept)
}

/// Replaces the marked block, or appends one, leaving everything else untouched.
fn upsert_block(text: &str, block: &str) -> String {
    let wrapped = format!("{BLOCK_BEGIN}\n{}\n{BLOCK_END}", block.trim());
    match block_range(text) {
        Some((start, end)) => format!("{}{wrapped}{}", &text[..start], &text[end..]),
        None if text.trim().is_empty() => format!("{wrapped}\n"),
        None => format!("{}\n\n{wrapped}\n", text.trim_end()),
    }
}

fn remove_block(text: &str) -> String {
    match block_range(text) {
        Some((start, end)) => {
            let joined = format!("{}{}", text[..start].trim_end(), &text[end..]);
            format!("{}\n", joined.trim_end())
        }
        None => text.to_string(),
    }
}

fn block_range(text: &str) -> Option<(usize, usize)> {
    let start = text.find(BLOCK_BEGIN)?;
    let end = text[start..].find(BLOCK_END)? + start + BLOCK_END.len();
    Some((start, end))
}

/// What a set of changes is published as.
struct Publish {
    branch: &'static str,
    title: &'static str,
    body: &'static str,
}

/// Shows the plan, asks, and publishes it as a PR from a throwaway worktree.
#[allow(clippy::too_many_arguments)]
fn apply(
    shell: &dyn Shell,
    repo: &Repo,
    changes: &[Change],
    publish: &Publish,
    dry_run: bool,
    yes: bool,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<()> {
    if changes.is_empty() {
        writeln!(out, "Nothing to change.")?;
        return Ok(());
    }
    writeln!(out, "Would open a PR on {} changing:", publish.branch)?;
    for change in changes {
        let verb = if change.content.is_some() {
            "write "
        } else {
            "delete"
        };
        writeln!(out, "  {verb} {}", change.path)?;
    }
    if dry_run {
        return Ok(());
    }
    if !yes && !confirm("Open this PR?", input, out)? {
        writeln!(out, "Cancelled.")?;
        return Ok(());
    }
    release_branch(shell, publish.branch)?;
    clear_local_leftovers(shell, publish.branch);

    let directory = tempfile::tempdir().context("failed to create a temporary worktree path")?;
    let path = directory.path().to_string_lossy().to_string();
    let base = format!("origin/{}", repo.default_branch);
    shell.run(
        "git",
        &["worktree", "add", "-q", "-b", publish.branch, &path, &base],
        None,
    )?;
    let result = publish_from(shell, directory.path(), changes, publish, repo);
    // Clean up locally on success and failure alike, so a retry can start over.
    let removed = shell.run("git", &["worktree", "remove", "--force", &path], None);
    let deleted = shell.run("git", &["branch", "-q", "-D", publish.branch], None);
    let url = result?;
    removed?;
    deleted?;
    writeln!(out, "{url}")?;
    Ok(())
}

/// Clears the way for a new PR. The branch of a merged PR is deleted, because
/// GitHub keeps it and a later install must still work; an open PR stops the run.
fn release_branch(shell: &dyn Shell, branch: &str) -> Result<()> {
    let open = shell.run(
        "gh",
        &[
            "pr", "list", "--state", "open", "--head", branch, "--json", "url", "--jq", ".[0].url",
        ],
        None,
    )?;
    if !open.is_empty() {
        bail!("{open} is already open from {branch}; merge or close it first");
    }
    let remote = shell.run("git", &["ls-remote", "--heads", "origin", branch], None)?;
    if !remote.is_empty() {
        shell.run("git", &["push", "-q", "origin", "--delete", branch], None)?;
    }
    Ok(())
}

/// Removes what a killed run left behind, so a retry starts clean. Best effort:
/// there is normally nothing here to remove.
fn clear_local_leftovers(shell: &dyn Shell, branch: &str) {
    let wanted = format!("branch refs/heads/{branch}");
    let listing = shell
        .run("git", &["worktree", "list", "--porcelain"], None)
        .unwrap_or_default();
    for entry in listing.split("\n\n") {
        if entry.lines().any(|line| line == wanted) {
            if let Some(path) = entry
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("worktree "))
            {
                let _ = shell.run("git", &["worktree", "remove", "--force", path], None);
            }
        }
    }
    let _ = shell.run("git", &["branch", "-q", "-D", branch], None);
}

fn publish_from(
    shell: &dyn Shell,
    worktree: &Path,
    changes: &[Change],
    publish: &Publish,
    repo: &Repo,
) -> Result<String> {
    for change in changes {
        let target = worktree.join(&change.path);
        match &change.content {
            Some(content) => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                std::fs::write(&target, content)
                    .with_context(|| format!("failed to write {}", target.display()))?;
            }
            None => std::fs::remove_file(&target)
                .with_context(|| format!("failed to delete {}", target.display()))?,
        }
    }
    shell.run("git", &["add", "-A"], Some(worktree))?;
    // Repository hooks need dependencies a fresh worktree doesn't have, and
    // this commit holds only generated config; CI still checks the PR.
    shell.run(
        "git",
        &["commit", "-q", "--no-verify", "-m", publish.title],
        Some(worktree),
    )?;
    shell.run(
        "git",
        &["push", "-q", "-u", "origin", publish.branch],
        Some(worktree),
    )?;
    shell.run(
        "gh",
        &[
            "pr",
            "create",
            "--base",
            &repo.default_branch,
            "--head",
            publish.branch,
            "--title",
            publish.title,
            "--body",
            publish.body,
        ],
        Some(worktree),
    )
}

fn confirm(question: &str, input: &mut dyn BufRead, out: &mut dyn Write) -> Result<bool> {
    write!(out, "{question} [y/N] ")?;
    out.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn warn_missing_secret(shell: &dyn Shell, out: &mut dyn Write) -> Result<()> {
    match has_secret(shell) {
        Some(true) => {}
        Some(false) => writeln!(
            out,
            "Missing secret: run `gh secret set {SECRET_NAME}` so reviews can run."
        )?,
        None => writeln!(out, "{SECRET_UNKNOWN}")?,
    }
    Ok(())
}

/// Reports whether the secret exists, or `None` when this account can't list secrets.
fn has_secret(shell: &dyn Shell) -> Option<bool> {
    let names = shell
        .run(
            "gh",
            &["secret", "list", "--json", "name", "--jq", ".[].name"],
            None,
        )
        .ok()?;
    Some(names.lines().any(|name| name == SECRET_NAME))
}

/// Compares the default branch with what the flow expects.
fn check_repo(shell: &dyn Shell, repo: &Repo) -> Result<Vec<(bool, String)>> {
    let workflow = read_default(shell, repo, WORKFLOW_PATH)?;
    let text = workflow.as_deref().unwrap_or("");
    // Same logic as install, so check reports exactly what install would change.
    let unchanged = |edit: fn(&str) -> Result<String>| edit(text).is_ok_and(|after| after == text);
    let config = read_default(shell, repo, CONFIG_PATH)?;
    let parsed = config.as_deref().map(parse_config).transpose();
    let instructions = read_default(shell, repo, AGENTS_PATH)?;
    let secret = match has_secret(shell) {
        Some(true) => (true, format!("secret {SECRET_NAME} is set")),
        Some(false) => (false, format!("secret {SECRET_NAME} is missing")),
        None => (true, SECRET_UNKNOWN.to_string()),
    };

    let mut checks = vec![
        (workflow.is_some(), format!("{WORKFLOW_PATH} exists")),
        (
            unchanged(ensure_land_input),
            "the reviewer step sets land".to_string(),
        ),
        (
            unchanged(ensure_statuses_write),
            "the workflow grants statuses: write".to_string(),
        ),
        (config.is_some(), format!("{CONFIG_PATH} exists")),
        (
            instructions
                .as_deref()
                .is_some_and(|text| upsert_block(text, AGENTS_BLOCK) == text),
            format!("{AGENTS_PATH} has the current delivery flow block"),
        ),
        secret,
    ];
    // A failed lookup says nothing about the repository, so it isn't drift.
    if let Ok(latest) = resolve_pin(shell, None) {
        // An older pin can be deliberate (`--ref`), so it is reported, not drift.
        let current = set_pin(text, &latest).is_ok_and(|after| after == text);
        let note = if current { "" } else { "not " };
        checks.push((
            true,
            format!(
                "the action is {note}pinned to the latest release ({})",
                latest.label
            ),
        ));
    }
    match parsed {
        Ok(Some(file)) => checks.push((
            true,
            format!(
                "{CONFIG_PATH} parses; nibble-review checks [land] (main is a safe staging step: {})",
                if file.land.main_is_staging {
                    "yes"
                } else {
                    "no"
                }
            ),
        )),
        Ok(None) => {}
        Err(error) => checks.push((false, format!("{error:#}"))),
    }
    Ok(checks)
}

fn parse_config(text: &str) -> Result<SdlcFile> {
    let file: SdlcFile = toml::from_str(text).with_context(|| format!("invalid {CONFIG_PATH}"))?;
    if !["squash", "merge", "rebase"].contains(&file.merge.method.as_str()) {
        bail!(
            "invalid {CONFIG_PATH}: unknown merge method {}",
            file.merge.method
        );
    }
    Ok(file)
}

/// Reads the merge method from the default branch, like every other setting.
fn merge_method(shell: &dyn Shell, repo: &Repo) -> Result<String> {
    match read_default(shell, repo, CONFIG_PATH)? {
        Some(text) => Ok(parse_config(&text)?.merge.method),
        None => Ok(default_merge_method()),
    }
}

/// Finds land's report for a commit in a PR's status rollup.
fn land_description(rollup: &serde_json::Value) -> Option<String> {
    rollup.as_array()?.iter().find_map(|item| {
        (item["context"].as_str() == Some(LAND_CONTEXT))
            .then(|| item["description"].as_str().unwrap_or("").to_string())
    })
}

fn render_status(shell: &dyn Shell) -> Result<String> {
    let raw = shell.run(
        "gh",
        &[
            "pr",
            "list",
            "--state",
            "open",
            "--json",
            "number,title,statusCheckRollup",
        ],
        None,
    )?;
    let prs: serde_json::Value = serde_json::from_str(&raw).context("failed to parse gh output")?;
    let lines: Vec<String> = prs
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|pr| {
            format!(
                "#{} {}: {}\n",
                pr["number"],
                pr["title"].as_str().unwrap_or(""),
                land_description(&pr["statusCheckRollup"])
                    .unwrap_or_else(|| "no land result yet".to_string())
            )
        })
        .collect();
    Ok(if lines.is_empty() {
        "No open pull requests.\n".to_string()
    } else {
        lines.concat()
    })
}

struct MergeRequest {
    pr: u64,
    auto: bool,
    yes: bool,
    interactive: bool,
    method: String,
}

/// Merges the PR's current head, and only a person can override land.
fn merge(
    shell: &dyn Shell,
    request: &MergeRequest,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<()> {
    let number = request.pr.to_string();
    let raw = shell.run(
        "gh",
        &[
            "pr",
            "view",
            &number,
            "--json",
            "headRefOid,title,statusCheckRollup",
        ],
        None,
    )?;
    let pr: serde_json::Value = serde_json::from_str(&raw).context("failed to parse gh output")?;
    let head = pr["headRefOid"]
        .as_str()
        .ok_or_else(|| anyhow!("PR #{number} has no head commit"))?;
    let land = land_description(&pr["statusCheckRollup"]);
    let clear = land.as_deref() == Some(LAND_CLEARED);
    writeln!(
        out,
        "#{number} {}\nLand: {}",
        pr["title"].as_str().unwrap_or(""),
        land.as_deref().unwrap_or("no result for this commit")
    )?;

    if !clear && !request.interactive {
        bail!("land did not clear PR #{number}, and only a person at a terminal can override it");
    }
    if !(clear && request.yes) {
        if !request.interactive {
            bail!("pass --yes to merge PR #{number} without a terminal");
        }
        if !confirm("Merge this commit?", input, out)? {
            writeln!(out, "Cancelled.")?;
            return Ok(());
        }
    }

    let method = format!("--{}", request.method);
    let mut args = vec!["pr", "merge", &number, &method, "--match-head-commit", head];
    if request.auto {
        args.push("--auto");
    }
    shell.run("gh", &args, None)?;
    writeln!(
        out,
        "{}",
        if request.auto {
            "Auto-merge enabled."
        } else {
            "Merged."
        }
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
