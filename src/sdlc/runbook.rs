//! `nibble sdlc runbook`: how the flow works, every setting, and what the
//! current repository should set next.
//!
//! The guide is `docs/runbook.md`, compiled in, so the terminal and GitHub show
//! the same text. The repository part reads the default branch only, like
//! nibble-review and `nibble land`.

use anyhow::Result;
use std::io::Write;

use super::{
    load_repo, parse_config, read_default, validate_repo_name, LandMode, LandProfile, OpenRepo,
    Shell, CONFIG_PATH,
};

/// The guide printed before the repository's settings.
const RUNBOOK: &str = include_str!("../../docs/runbook.md");
/// Where the judge's context for a repository lives.
const RISK_PATH: &str = ".nibble/risk.md";
/// nibble-review's defaults, shown for settings a repository leaves unset.
const DEFAULT_MODE: LandMode = LandMode::Shadow;
const DEFAULT_PROFILE: LandProfile = LandProfile::Critical;
const DEFAULT_REVIEWER: &str = "github-actions[bot]";
/// Said when the policy is complete: the setup steps this command can't see.
const UNCHECKED: &str = "The policy is complete. Not checked here: the `OPENCODE_API_KEY` secret (`nibble sdlc check` does that), an `AGENTS.md` that forbids merging, a branch ruleset, and permission for your agent to run `nibble land`.\n";

impl LandMode {
    fn as_str(self) -> &'static str {
        match self {
            LandMode::Off => "off",
            LandMode::Shadow => "shadow",
            LandMode::Enforce => "enforce",
        }
    }
}

impl LandProfile {
    fn as_str(self) -> &'static str {
        match self {
            LandProfile::SideProject => "side-project",
            LandProfile::Standard => "standard",
            LandProfile::Critical => "critical",
        }
    }
}

/// Prints the guide, then the settings of `repo`, or of the repository the
/// `current` shell runs in. Outside a repository the guide alone is a success;
/// any other failure to read the settings, a broken policy included, is one.
pub(super) fn run(
    repo: Option<&str>,
    open: OpenRepo<'_>,
    current: &dyn Shell,
    out: &mut dyn Write,
) -> Result<i32> {
    out.write_all(RUNBOOK.as_bytes())?;
    writeln!(out)?;
    let report = match repo {
        Some(name) => validate_repo_name(name)
            .and_then(open)
            .and_then(|workspace| report(workspace.shell.as_ref())),
        None if current
            .run("git", &["rev-parse", "--show-toplevel"], None)
            .is_err() =>
        {
            writeln!(
                out,
                "Run it inside a repository, or pass owner/name, to see its settings."
            )?;
            return Ok(0);
        }
        None => report(current),
    };
    match report {
        Ok(text) => {
            write!(out, "{text}")?;
            Ok(0)
        }
        Err(error) => {
            writeln!(out, "✗ {error:#}")?;
            Ok(1)
        }
    }
}

/// Reads the repository's policy and risk map from its default branch.
pub(super) fn report(shell: &dyn Shell) -> Result<String> {
    let repo = load_repo(shell)?;
    let config = read_default(shell, &repo, CONFIG_PATH)?;
    let risk = read_default(shell, &repo, RISK_PATH)?;
    render(&repo.default_branch, config.as_deref(), risk.is_some())
}

/// Lists each setting, marking the unset ones with their default, and the steps
/// still open, in the order the runbook gives them.
fn render(branch: &str, config: Option<&str>, has_risk_map: bool) -> Result<String> {
    let mut text = format!("## This repository\n\nRead from `{branch}`.\n\n");
    let Some(config) = config else {
        text.push_str(&format!(
            "No `{CONFIG_PATH}`, so the flow isn't installed.\n\nNext: run `nibble sdlc install`.\n"
        ));
        return Ok(text);
    };
    let land = parse_config(config)?.land;
    let mode = land.mode.unwrap_or(DEFAULT_MODE);
    let unset = " (not set)";
    let rows = [
        (
            "mode",
            format!(
                "{}{}",
                mode.as_str(),
                if land.mode.is_none() { unset } else { "" }
            ),
        ),
        (
            "profile",
            land.profile.map_or_else(
                || format!("{}{unset}", DEFAULT_PROFILE.as_str()),
                |profile| profile.as_str().to_string(),
            ),
        ),
        (
            "required_checks",
            list_or(&land.required_checks, "none, so nothing merges unattended"),
        ),
        ("authors", list_or(&land.authors, "any person")),
        (
            "always_human",
            list_or(&land.always_human, "only the profile's paths"),
        ),
        (
            "ask_when",
            list_or(&land.ask_when.keys().cloned().collect::<Vec<_>>(), "none"),
        ),
        ("main_is_staging", land.main_is_staging.to_string()),
        (
            "max_lines",
            land.max_lines
                .map_or_else(|| "no limit".to_string(), |lines| lines.to_string()),
        ),
        (
            "reviewer",
            land.reviewer
                .clone()
                .unwrap_or_else(|| format!("{DEFAULT_REVIEWER}{unset}")),
        ),
        (
            "risk map",
            if has_risk_map {
                format!("`{RISK_PATH}`")
            } else {
                "missing".to_string()
            },
        ),
    ];
    let width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    for (name, value) in &rows {
        text.push_str(&format!("    {name:<width$}  {value}\n"));
    }

    let mut next = Vec::new();
    if land.required_checks.is_empty() {
        next.push("Set `required_checks` to your CI check names (`gh pr checks <number>` lists them). Until then nothing merges unattended.".to_string());
    }
    if land.profile.is_none() {
        next.push(format!(
            "Choose a `profile`. Unset means `{}`, the strictest.",
            DEFAULT_PROFILE.as_str()
        ));
    }
    if land.authors.is_empty() {
        next.push(
            "Set `authors` to your login if other people open pull requests here.".to_string(),
        );
    }
    if !has_risk_map {
        next.push(format!(
            "Write `{RISK_PATH}`, so the judge knows what a merge sets off."
        ));
    }
    match mode {
        LandMode::Off => {
            next.push("Land is off. Set `mode = \"shadow\"` to watch its decisions.".to_string());
        }
        LandMode::Shadow => next.push(
            "Run `nibble land backtest` on a few recent pull requests, then set `mode = \"enforce\"` if you agree with it.".to_string(),
        ),
        LandMode::Enforce => {}
    }

    text.push('\n');
    if next.is_empty() {
        text.push_str(UNCHECKED);
    } else {
        text.push_str("Next:\n");
        for (index, step) in next.iter().enumerate() {
            text.push_str(&format!("{}. {step}\n", index + 1));
        }
    }
    Ok(text)
}

/// Joins `values`, or says `empty` when there are none.
fn list_or(values: &[String], empty: &str) -> String {
    if values.is_empty() {
        empty.to_string()
    } else {
        values.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell outside any repository: every command fails.
    struct Outside;

    impl Shell for Outside {
        fn run(&self, program: &str, _: &[&str], _: Option<&std::path::Path>) -> Result<String> {
            anyhow::bail!("{program} failed")
        }
    }

    #[test]
    fn outside_a_repository_the_guide_alone_is_a_success() {
        let open = |_: &str| -> Result<super::super::Workspace> { panic!("opened") };
        let mut out = Vec::new();
        assert_eq!(run(None, &open, &Outside, &mut out).unwrap(), 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text
            .trim_end()
            .ends_with("pass owner/name, to see its settings."));
    }

    #[test]
    fn old_and_extra_settings_nibble_review_accepts_still_parse() {
        let config = "[land]\nmax_review_rounds = 5\nalways_human = [\"src/**/*.sql\"]\n\n[merge]\nold = true\n\n[other]\nx = 1\n";
        let text = render("main", Some(config), false).unwrap();
        assert!(text.contains("always_human     src/**/*.sql"));
    }

    #[test]
    fn a_repository_without_the_flow_is_told_to_install_it() {
        let text = render("main", None, false).unwrap();
        assert!(text.contains("Read from `main`"));
        assert!(text.contains("Next: run `nibble sdlc install`."));
        assert!(!text.contains("required_checks"));
    }

    #[test]
    fn an_empty_policy_shows_every_default_and_every_step() {
        let text = render("main", Some("[land]\n"), false).unwrap();
        assert!(text.contains("mode             shadow (not set)"));
        assert!(text.contains("profile          critical (not set)"));
        assert!(text.contains("required_checks  none, so nothing merges unattended"));
        assert!(text.contains("authors          any person"));
        assert!(text.contains("always_human     only the profile's paths"));
        assert!(text.contains("main_is_staging  false"));
        assert!(text.contains("max_lines        no limit"));
        assert!(text.contains("reviewer         github-actions[bot] (not set)"));
        assert!(text.contains("risk map         missing"));
        for step in [
            "1. Set `required_checks`",
            "2. Choose a `profile`",
            "3. Set `authors`",
            "4. Write `.nibble/risk.md`",
            "5. Run `nibble land backtest`",
        ] {
            assert!(text.contains(step), "missing {step:?} in\n{text}");
        }
    }

    #[test]
    fn a_finished_policy_in_enforce_has_nothing_left() {
        let config = r#"[land]
mode = "enforce"
profile = "side-project"
required_checks = ["test", "review"]
authors = ["adlrocha"]
always_human = ["src/land"]
main_is_staging = true
max_lines = 400
reviewer = "nibble-review[bot]"

[land.ask_when]
newsletter = "Changes who gets an email"
"#;
        let text = render("trunk", Some(config), true).unwrap();
        assert!(text.contains("mode             enforce\n"));
        assert!(text.contains("profile          side-project\n"));
        assert!(text.contains("required_checks  test, review"));
        assert!(text.contains("authors          adlrocha"));
        assert!(text.contains("always_human     src/land"));
        assert!(text.contains("ask_when         newsletter"));
        assert!(text.contains("main_is_staging  true"));
        assert!(text.contains("max_lines        400"));
        assert!(text.contains("reviewer         nibble-review[bot]\n"));
        assert!(text.contains("risk map         `.nibble/risk.md`"));
        assert!(text.contains("The policy is complete."));
        assert!(!text.contains("Next:"));
    }

    #[test]
    fn mode_off_says_how_to_turn_it_back_on() {
        let config = "[land]\nmode = \"off\"\nprofile = \"standard\"\nrequired_checks = [\"test\"]\nauthors = [\"me\"]\n";
        let text = render("main", Some(config), true).unwrap();
        assert!(text.contains("1. Land is off."));
        assert!(!text.contains("backtest"));
    }

    #[test]
    fn an_invalid_policy_is_an_error() {
        for config in [
            "[land]\nmode = \"merge\"\n",
            "[land]\nprofile = \"relaxed\"\n",
            "[land]\nauthors = \"me\"\n",
            "[land]\nrequired_check = [\"test\"]\n",
            "not toml",
        ] {
            let error = render("main", Some(config), false).unwrap_err();
            assert!(format!("{error:#}").contains("invalid .nibble/sdlc.toml"));
        }
    }

    #[test]
    fn the_guide_is_printed_before_the_repository() {
        let open = |_: &str| -> Result<super::super::Workspace> { anyhow::bail!("no such repo") };
        let mut out = Vec::new();
        let code = run(Some("owner/name"), &open, &Outside, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("# nibble sdlc runbook"));
        assert!(text.contains("## The levers"));
        assert!(text.trim_end().ends_with("✗ no such repo"));
        assert_eq!(code, 1);
    }

    #[test]
    fn a_bad_repository_name_fails_before_opening_it() {
        let open = |_: &str| -> Result<super::super::Workspace> { panic!("opened") };
        let mut out = Vec::new();
        assert_eq!(
            run(Some("not a repo"), &open, &Outside, &mut out).unwrap(),
            1
        );
    }

    #[test]
    fn the_guide_documents_every_setting_the_runbook_shows() {
        for setting in [
            "`mode`",
            "`profile`",
            "`required_checks`",
            "`authors`",
            "`always_human`",
            "`[land.ask_when]`",
            "`main_is_staging`",
            "`max_lines`",
            "`reviewer`",
            "`.nibble/risk.md`",
        ] {
            assert!(RUNBOOK.contains(setting), "runbook misses {setting}");
        }
    }
}
