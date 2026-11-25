//! Land's GitHub calls, through the `gh` the caller is signed in to.

use super::bundle::{Bundle, Header, Mode, VERSION};
use super::history;
use super::run::{Env, Invocation, Runner};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const ACTION_REPO: &str = "adlrocha/nibble-review";
const WORKFLOW_PATH: &str = ".github/workflows/nibble-review.yml";

/// The label that marks a pull request as waiting for its owner.
pub(super) const LABEL: &str = "needs-human";

/// Fetching and building the rules takes a while the first time.
const BUILD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Runs `gh` and the rules of the reviewer a repository pins.
pub(super) struct GitHub<'a> {
    pub runner: &'a dyn Runner,
    /// Where built reviewers are kept, one directory per commit.
    pub cache: PathBuf,
    /// nibble's environment, so the tools that need no credentials get none.
    pub env: &'a BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeMethod {
    viewer_default_merge_method: String,
}

#[derive(Debug, Deserialize)]
struct State {
    state: String,
}

/// Most merged pull requests a digest lists.
pub(super) const DIGEST_LIMIT: usize = 200;

/// One merged pull request, for a release digest.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Merged {
    pub number: u64,
    pub head_ref_oid: String,
    pub title: String,
    pub url: String,
    pub comments: Vec<MergedComment>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MergedComment {
    pub body: String,
    pub author_association: String,
}

/// One open pull request waiting for its owner.
#[derive(Debug, Deserialize)]
pub(super) struct Waiting {
    pub title: String,
    pub url: String,
}

impl GitHub<'_> {
    fn gh(&self, args: &[&str]) -> Result<String> {
        self.runner.run(&Invocation::new("gh", args))
    }

    /// The repository named, or the one in the current directory.
    pub(super) fn repository(&self, named: Option<String>) -> Result<String> {
        let name = match named {
            Some(name) => name,
            None => self
                .gh(&[
                    "repo",
                    "view",
                    "--json",
                    "nameWithOwner",
                    "--jq",
                    ".nameWithOwner",
                ])?
                .trim()
                .to_string(),
        };
        let part = |part: &str| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        };
        match name.split_once('/') {
            Some((owner, repo)) if part(owner) && part(repo) => Ok(name),
            _ => bail!("expected a repository as owner/name, got `{name}`"),
        }
    }

    /// Reads the decision and evidence for one pull request, or `None` when land is off.
    ///
    /// `policy` replaces the repository's own, for a backtest.
    pub(super) fn bundle(
        &self,
        repo: &str,
        number: u64,
        policy: Option<&Path>,
    ) -> Result<Option<Bundle>> {
        let script = self.rules(repo)?;
        let token = self.gh(&["auth", "token"])?;
        let mut args = vec![script.display().to_string(), "--json".to_string()];
        if let Some(path) = policy {
            args.extend(["--config".to_string(), path.display().to_string()]);
        }
        let printed = self.runner.run(&Invocation {
            program: "node".to_string(),
            args,
            stdin: None,
            env: Env::Only(self.tool_env(&[
                ("GH_TOKEN", token.trim()),
                ("GITHUB_REPOSITORY", repo),
                ("PR_NUMBER", &number.to_string()),
            ])),
            dir: None,
            timeout: super::run::SHORT,
        })?;
        let header: Header = serde_json::from_str(&printed).with_context(|| {
            format!(
                "{repo} pins a nibble-review too old for `nibble land`; run `nibble sdlc install`"
            )
        })?;
        if header.version != VERSION {
            bail!(
                "{repo}'s nibble-review reports bundle version {}, and this nibble reads {VERSION}",
                header.version
            );
        }
        if header.mode == Mode::Off {
            return Ok(None);
        }
        let bundle =
            serde_json::from_str(&printed).context("nibble-review's land bundle is malformed")?;
        Ok(Some(bundle))
    }

    /// The rules script of the reviewer the default branch pins, built once per commit.
    fn rules(&self, repo: &str) -> Result<PathBuf> {
        let sha = self.pinned(repo)?;
        let built = self.cache.join(&sha);
        let script = built.join("dist/cli/land.js");
        if script.is_file() {
            return Ok(script);
        }
        fs::create_dir_all(&self.cache)
            .with_context(|| format!("failed to create {}", self.cache.display()))?;
        // Built aside and moved in place, so an interrupted build is never used.
        let draft = tempfile::tempdir_in(&self.cache).context("failed to create a build folder")?;
        let url = format!("https://github.com/{ACTION_REPO}");
        let env = Env::Only(self.tool_env(&[("HUSKY", "0"), ("GIT_TERMINAL_PROMPT", "0")]));
        let steps: [(&str, &[&str]); 5] = [
            ("git", &["init", "--quiet"]),
            ("git", &["fetch", "--quiet", "--depth", "1", &url, &sha]),
            ("git", &["checkout", "--quiet", "FETCH_HEAD"]),
            (
                "npm",
                &["ci", "--ignore-scripts", "--no-audit", "--no-fund"],
            ),
            ("npm", &["run", "build"]),
        ];
        for (program, args) in steps {
            self.runner
                .run(&Invocation {
                    env: env.clone(),
                    dir: Some(draft.path().to_path_buf()),
                    timeout: BUILD_TIMEOUT,
                    ..Invocation::new(program, args)
                })
                .context("failed to build the land rules")?;
        }
        if !draft.path().join("dist/cli/land.js").is_file() {
            bail!("{repo} pins a nibble-review without land; run `nibble sdlc install`");
        }
        match fs::rename(draft.path(), &built) {
            Ok(()) => {}
            // Another run finished the same build first.
            Err(_) if script.is_file() => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to store {}", built.display()))
            }
        }
        Ok(script)
    }

    /// The commit of nibble-review that the default branch's workflow runs.
    fn pinned(&self, repo: &str) -> Result<String> {
        let path = format!("repos/{repo}/contents/{WORKFLOW_PATH}");
        let workflow = self
            .gh(&["api", "-H", "Accept: application/vnd.github.raw", &path])
            .with_context(|| {
                format!("{repo} does not run nibble-review; run `nibble sdlc install`")
            })?;
        let prefix = format!("uses: {ACTION_REPO}@");
        let reference = workflow
            .lines()
            .find_map(|line| {
                line.trim_start()
                    .trim_start_matches("- ")
                    .strip_prefix(prefix.as_str())
            })
            .and_then(|rest| rest.split_whitespace().next())
            .with_context(|| format!("{repo}'s workflow does not use {ACTION_REPO}"))?;
        if !reference
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
        {
            bail!("{repo} pins nibble-review to an unexpected ref: {reference}");
        }
        let is_sha = |text: &str| {
            text.len() == 40
                && text
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        };
        if is_sha(reference) {
            return Ok(reference.to_string());
        }
        let sha = self.gh(&[
            "api",
            &format!("repos/{ACTION_REPO}/commits/{reference}"),
            "--jq",
            ".sha",
        ])?;
        let sha = sha.trim();
        if !is_sha(sha) {
            bail!("GitHub returned no commit for nibble-review@{reference}");
        }
        Ok(sha.to_string())
    }

    /// Just enough to run git, node and npm, without nibble's credentials.
    fn tool_env(&self, extra: &[(&str, &str)]) -> BTreeMap<String, String> {
        const KEPT: &[&str] = &[
            "PATH",
            "HOME",
            "LANG",
            "TMPDIR",
            "SSL_CERT_FILE",
            "NODE_EXTRA_CA_CERTS",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "NO_PROXY",
        ];
        self.env
            .iter()
            .filter(|(name, _)| KEPT.contains(&name.as_str()))
            .map(|(name, value)| (name.clone(), value.clone()))
            .chain(
                extra
                    .iter()
                    .map(|(name, value)| ((*name).to_string(), (*value).to_string())),
            )
            .collect()
    }

    /// Recent pull requests, for what they teach about this one.
    pub(super) fn history(&self, repo: &str) -> Result<history::Response> {
        let (owner, name) = repo.split_once('/').context("expected owner/name")?;
        let printed = self.gh(&[
            "api",
            "graphql",
            "-f",
            &format!("query={}", history::QUERY),
            "-f",
            &format!("owner={owner}"),
            "-f",
            &format!("name={name}"),
            "-F",
            &format!("count={}", history::LOOKBACK),
        ])?;
        serde_json::from_str(&printed).context("failed to read the pull request history")
    }

    pub(super) fn comment(&self, repo: &str, number: u64, body: &str) -> Result<()> {
        self.runner.run(&Invocation {
            stdin: Some(body.to_string()),
            ..Invocation::new(
                "gh",
                &[
                    "pr",
                    "comment",
                    &number.to_string(),
                    "--repo",
                    repo,
                    "--body-file",
                    "-",
                ],
            )
        })?;
        Ok(())
    }

    /// Adds the label, which GitHub creates when the repository lacks it.
    pub(super) fn add_label(&self, repo: &str, number: u64) -> Result<()> {
        self.gh(&[
            "api",
            &format!("repos/{repo}/issues/{number}/labels"),
            "-f",
            &format!("labels[]={LABEL}"),
        ])?;
        Ok(())
    }

    /// Removes the label. One that isn't there is already removed.
    pub(super) fn remove_label(&self, repo: &str, number: u64) -> Result<()> {
        let path = format!("repos/{repo}/issues/{number}/labels/{LABEL}");
        match self.gh(&["api", "--method", "DELETE", &path]) {
            Err(error) if !format!("{error:#}").contains("404") => Err(error),
            _ => Ok(()),
        }
    }

    /// Merges exactly `sha` with the repository's default method, and confirms it happened.
    pub(super) fn merge(&self, repo: &str, number: u64, sha: &str) -> Result<()> {
        let printed = self.gh(&["repo", "view", repo, "--json", "viewerDefaultMergeMethod"])?;
        let method: MergeMethod =
            serde_json::from_str(&printed).context("failed to read the merge method")?;
        let flag = match method.viewer_default_merge_method.as_str() {
            "MERGE" => "--merge",
            "SQUASH" => "--squash",
            "REBASE" => "--rebase",
            other => bail!("unknown merge method {other}"),
        };
        let attempt = self.gh(&[
            "pr",
            "merge",
            &number.to_string(),
            "--repo",
            repo,
            flag,
            "--match-head-commit",
            sha,
        ]);
        // gh can fail after GitHub merged, so only GitHub says what happened.
        match (self.state(repo, number), attempt) {
            (Ok(state), _) if state == "MERGED" => Ok(()),
            (Ok(_), Err(error)) => Err(error),
            (Ok(state), Ok(_)) => bail!(
                "GitHub accepted the merge but the pull request is {state}, so it may merge later; tell the user"
            ),
            (Err(error), _) => Err(error).context(
                "could not confirm whether the pull request merged; check it before anything else",
            ),
        }
    }

    /// Whether the pull request is OPEN, CLOSED or MERGED.
    pub(super) fn state(&self, repo: &str, number: u64) -> Result<String> {
        let number = number.to_string();
        let printed = self.gh(&["pr", "view", &number, "--repo", repo, "--json", "state"])?;
        let state: State = serde_json::from_str(&printed).context("failed to read the state")?;
        Ok(state.state)
    }

    /// The caller's open pull requests that wait for a person, in every repository.
    pub(super) fn waiting(&self) -> Result<Vec<Waiting>> {
        let printed = self.gh(&[
            "search",
            "prs",
            "--author",
            "@me",
            "--state",
            "open",
            "--label",
            LABEL,
            "--limit",
            "100",
            "--json",
            "title,url",
        ])?;
        serde_json::from_str(&printed).context("failed to read the search")
    }

    /// The publication date of the latest release, if there is one.
    pub(super) fn last_release(&self, repo: &str) -> Result<Option<String>> {
        let printed = self.gh(&[
            "release",
            "list",
            "--repo",
            repo,
            "--exclude-drafts",
            "--limit",
            "1",
            "--json",
            "publishedAt",
            "--jq",
            ".[].publishedAt",
        ])?;
        let date = printed.trim();
        Ok((!date.is_empty()).then(|| date.to_string()))
    }

    /// Pull requests merged after `since`.
    pub(super) fn merged_since(&self, repo: &str, since: &str) -> Result<Vec<Merged>> {
        let printed = self.gh(&[
            "pr",
            "list",
            "--repo",
            repo,
            "--state",
            "merged",
            "--search",
            &format!("merged:>{since}"),
            "--limit",
            &DIGEST_LIMIT.to_string(),
            "--json",
            "number,title,url,headRefOid,comments",
        ])?;
        serde_json::from_str(&printed).context("failed to read the merged pull requests")
    }
}
