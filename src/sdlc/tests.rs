use super::*;
use std::cell::RefCell;
use std::io::Cursor;

/// Answers commands by the first matching substring and records every call.
struct FakeShell {
    answers: Vec<(&'static str, Result<String, String>)>,
    calls: RefCell<Vec<String>>,
}

impl FakeShell {
    fn new(answers: &[(&'static str, &str)]) -> Self {
        Self {
            answers: answers
                .iter()
                .map(|(key, value)| (*key, Ok((*value).to_string())))
                .collect(),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn failing(mut self, key: &'static str) -> Self {
        self.answers.insert(0, (key, Err(format!("{key} failed"))));
        self
    }

    fn called(&self, wanted: &str) -> bool {
        self.calls.borrow().iter().any(|call| call.contains(wanted))
    }
}

impl Shell for FakeShell {
    fn run(&self, program: &str, args: &[&str], _dir: Option<&Path>) -> Result<String> {
        let call = format!("{program} {}", args.join(" "));
        self.calls.borrow_mut().push(call.clone());
        match self.answers.iter().find(|(key, _)| call.contains(key)) {
            Some((_, Ok(value))) => Ok(value.trim_end().to_string()),
            Some((_, Err(message))) => Err(anyhow!("{message}")),
            None => Ok(String::new()),
        }
    }
}

fn repo() -> Repo {
    Repo {
        default_branch: "main".to_string(),
    }
}

fn pin() -> Pin {
    Pin {
        sha: "a".repeat(40),
        label: "v1.1.0".to_string(),
    }
}

const WORKFLOW: &str = "permissions:
  contents: read
  pull-requests: write

jobs:
  review:
    steps:
      - uses: adlrocha/nibble-review@old # v1.0.5
        with:
          api-key: ${{ secrets.KEY }}
          allow-skip: \"false\"
";

#[test]
fn set_pin_keeps_the_step_and_replaces_only_the_ref() {
    let updated = set_pin(WORKFLOW, &pin()).unwrap();
    assert!(updated.contains(&format!(
        "      - uses: adlrocha/nibble-review@{} # v1.1.0\n",
        "a".repeat(40)
    )));
    assert!(updated.contains("allow-skip: \"false\""));
}

#[test]
fn edits_fail_clearly_without_the_reviewer_step() {
    let other = "permissions:\n  contents: read\njobs: {}\n";
    assert!(set_pin(other, &pin()).is_err());
    assert!(ensure_land_input(other).is_err());
    assert!(ensure_statuses_write(other).is_err());
}

#[test]
fn the_fix_sub_action_is_not_the_reviewer() {
    let fixer = "steps:\n  - uses: adlrocha/nibble-review/fix@v1\n    with:\n      a: b\n";
    assert!(set_pin(fixer, &pin()).is_err());
}

#[test]
fn land_is_added_once_inside_the_reviewer_step() {
    let updated = ensure_land_input(WORKFLOW).unwrap();
    assert!(updated.contains("        with:\n          land: \"true\"\n          api-key"));
    assert_eq!(ensure_land_input(&updated).unwrap(), updated);
}

#[test]
fn a_commented_land_line_does_not_count() {
    let commented = WORKFLOW.replace("allow-skip: \"false\"", "# land: \"true\"");
    assert!(ensure_land_input(&commented)
        .unwrap()
        .contains("          land: \"true\"\n"));
}

#[test]
fn other_spellings_are_rewritten_instead_of_duplicated() {
    let spelled = WORKFLOW.replace("allow-skip: \"false\"", "land: true");
    let updated = ensure_land_input(&spelled).unwrap();
    assert_eq!(updated.matches("land:").count(), 1);
    assert!(updated.contains("          land: \"true\"\n"));

    let read = WORKFLOW.replace("pull-requests: write", "statuses: read");
    let updated = ensure_statuses_write(&read).unwrap();
    assert_eq!(updated.matches("statuses:").count(), 1);
    assert!(updated.contains("  statuses: write\n"));
}

#[test]
fn four_space_workflows_keep_their_indentation() {
    let wide = "jobs:
    review:
        permissions:
            contents: read
        steps:
            - uses: adlrocha/nibble-review@v1
";
    let updated = ensure_statuses_write(wide).unwrap();
    assert!(updated.contains("            contents: read\n            statuses: write\n"));
}

#[test]
fn a_blank_line_inside_permissions_does_not_duplicate_the_key() {
    let spaced = "permissions:
  contents: read

  statuses: write
jobs:
  review:
    steps:
      - uses: adlrocha/nibble-review@v1
";
    assert_eq!(ensure_statuses_write(spaced).unwrap(), spaced);
}

#[test]
fn removing_land_matches_other_spellings() {
    let text = "      land: true\n      statuses: write # needed\n      statuses: read\n";
    assert_eq!(remove_land_lines(text), "      statuses: read\n");
}

#[test]
fn files_are_found_from_a_subdirectory() {
    let shell = tree(&[]);
    read_default(&shell, &repo(), "AGENTS.md").unwrap();
    assert!(shell.called("ls-tree --full-tree"));
}

#[test]
fn a_step_without_inputs_is_reported() {
    let bare = "steps:\n  - uses: adlrocha/nibble-review@v1\n  - run: echo hi\n";
    let error = ensure_land_input(bare).unwrap_err().to_string();
    assert!(error.contains("no `with:` block"));
}

#[test]
fn statuses_is_added_once_to_the_nearest_permissions_block() {
    let updated = ensure_statuses_write(WORKFLOW).unwrap();
    assert!(updated.contains("  pull-requests: write\n  statuses: write\n\njobs"));
    assert_eq!(ensure_statuses_write(&updated).unwrap(), updated);

    let job_level = "permissions:
  contents: read
jobs:
  review:
    permissions:
      contents: read
    steps:
      - uses: adlrocha/nibble-review@v1
";
    let updated = ensure_statuses_write(job_level).unwrap();
    assert!(updated.contains("      contents: read\n      statuses: write\n    steps"));
    assert!(updated.starts_with("permissions:\n  contents: read\njobs"));
}

#[test]
fn statuses_goes_to_the_reviewer_job_not_an_earlier_one() {
    let two_jobs = "permissions:
  contents: read
jobs:
  lint:
    permissions:
      contents: read
    steps:
      - run: echo hi
  review:
    steps:
      - uses: adlrocha/nibble-review@v1
";
    let updated = ensure_statuses_write(two_jobs).unwrap();
    assert!(updated.starts_with("permissions:\n  contents: read\n  statuses: write\njobs"));
    assert!(updated.contains("    permissions:\n      contents: read\n    steps"));
}

#[test]
fn a_workflow_without_a_permissions_block_is_reported() {
    let none = "jobs:\n  review:\n    steps:\n      - uses: adlrocha/nibble-review@v1\n";
    assert!(ensure_statuses_write(none)
        .unwrap_err()
        .to_string()
        .contains("no `permissions:` block"));
}

#[test]
fn removing_land_reverses_the_workflow_edits() {
    let installed = ensure_statuses_write(&ensure_land_input(WORKFLOW).unwrap()).unwrap();
    assert_eq!(remove_land_lines(&installed), WORKFLOW);
}

#[test]
fn the_block_is_appended_replaced_and_removed_without_touching_the_rest() {
    let original = "# Title\n\nKeep me.\n";
    let installed = upsert_block(original, "first");
    assert!(installed.starts_with("# Title\n\nKeep me.\n\n<!-- nibble-sdlc:begin -->\nfirst\n"));
    assert_eq!(upsert_block(&installed, "first"), installed);

    let updated = upsert_block(&installed, "second");
    assert!(updated.contains("second") && !updated.contains("first"));
    assert_eq!(remove_block(&updated), original);
    assert_eq!(remove_block(original), original);
    assert!(upsert_block("", "only").starts_with(BLOCK_BEGIN));
}

/// A repository whose default branch holds only the named files.
fn tree(files: &[(&'static str, &'static str)]) -> FakeShell {
    let mut answers: Vec<(&'static str, &str)> = Vec::new();
    for (path, text) in files {
        answers.push((
            Box::leak(format!("origin/main:{path}").into_boxed_str()),
            text,
        ));
        answers.push((Box::leak(format!("-- {path}").into_boxed_str()), path));
    }
    FakeShell::new(&answers)
}

#[test]
fn install_writes_all_three_files_into_an_empty_repo() {
    let changes = install_changes(&tree(&[]), &repo(), &pin(), true).unwrap();
    let paths: Vec<&str> = changes.iter().map(|change| change.path.as_str()).collect();
    assert_eq!(paths, [WORKFLOW_PATH, CONFIG_PATH, "AGENTS.md"]);
    let workflow = changes[0].content.as_deref().unwrap();
    assert!(workflow.contains(&format!("nibble-review@{} # v1.1.0", "a".repeat(40))));
    assert!(!workflow.contains("__PIN__"));
    let config = changes[1].content.as_deref().unwrap();
    assert!(config.contains("main_is_staging = true"));
    assert!(parse_config(config).unwrap().land.main_is_staging);
}

#[test]
fn install_keeps_customisations_and_targets_only_agents_md() {
    let shell = tree(&[
        (WORKFLOW_PATH, WORKFLOW),
        (CONFIG_PATH, "[land]\nmax_lines = 50\n"),
        ("CLAUDE.md", "@../AGENTS.md\n"),
    ]);
    let changes = install_changes(&shell, &repo(), &pin(), false).unwrap();
    let paths: Vec<&str> = changes.iter().map(|change| change.path.as_str()).collect();
    assert_eq!(paths, [WORKFLOW_PATH, "AGENTS.md"]);
    assert!(changes[0]
        .content
        .as_deref()
        .unwrap()
        .contains("allow-skip: \"false\""));
}

#[test]
fn rerunning_install_changes_nothing() {
    let first = install_changes(&tree(&[]), &repo(), &pin(), false).unwrap();
    let text = |index: usize| -> &'static str {
        Box::leak(first[index].content.clone().unwrap().into_boxed_str())
    };
    let shell = tree(&[
        (WORKFLOW_PATH, text(0)),
        (CONFIG_PATH, text(1)),
        ("AGENTS.md", text(2)),
    ]);
    assert_eq!(install_changes(&shell, &repo(), &pin(), false).unwrap(), []);
}

#[test]
fn uninstall_keeps_the_reviewer_unless_asked() {
    let installed: &'static str = Box::leak(
        ensure_statuses_write(&ensure_land_input(WORKFLOW).unwrap())
            .unwrap()
            .into_boxed_str(),
    );
    let agents: &'static str = Box::leak(upsert_block("# Own\n", "block").into_boxed_str());
    let shell = tree(&[
        (WORKFLOW_PATH, installed),
        (CONFIG_PATH, "[land]\n"),
        ("AGENTS.md", agents),
    ]);

    let changes = uninstall_changes(&shell, &repo(), false).unwrap();
    assert_eq!(changes[0].content.as_deref(), Some(WORKFLOW));
    assert_eq!(changes[1].path, CONFIG_PATH);
    assert_eq!(changes[1].content, None);
    assert_eq!(changes[2].content.as_deref(), Some("# Own\n"));

    let removed = uninstall_changes(&shell, &repo(), true).unwrap();
    assert_eq!(removed[0].content, None);
    assert_eq!(uninstall_changes(&tree(&[]), &repo(), false).unwrap(), []);
}

#[test]
fn the_pin_defaults_to_the_latest_release_and_accepts_a_ref() {
    let shell = FakeShell::new(&[("releases/latest", "v1.1.0"), ("commits/", "abc")]);
    assert_eq!(
        resolve_pin(&shell, None).unwrap(),
        Pin {
            sha: "abc".to_string(),
            label: "v1.1.0".to_string()
        }
    );
    let shell = FakeShell::new(&[("commits/feat/x", "def")]);
    assert_eq!(resolve_pin(&shell, Some("feat/x")).unwrap().sha, "def");
    assert!(!shell.called("releases/latest"));
}

fn change() -> Vec<Change> {
    vec![
        Change {
            path: "a.txt".to_string(),
            content: Some("hello\n".to_string()),
        },
        Change {
            path: "gone.txt".to_string(),
            content: None,
        },
    ]
}

const PUBLISH: Publish = Publish {
    branch: "chore/x",
    title: "title",
    body: "body",
};

fn apply_with(
    shell: &FakeShell,
    changes: &[Change],
    dry: bool,
    yes: bool,
    answer: &str,
) -> (Result<()>, String) {
    let mut out = Vec::new();
    let result = apply(
        shell,
        &repo(),
        changes,
        &PUBLISH,
        dry,
        yes,
        &mut Cursor::new(answer.to_string()),
        &mut out,
    );
    (result, String::from_utf8(out).unwrap())
}

#[test]
fn apply_does_nothing_without_changes_or_on_a_dry_run() {
    let shell = FakeShell::new(&[]);
    let (result, output) = apply_with(&shell, &[], false, true, "");
    result.unwrap();
    assert!(output.contains("Nothing to change."));

    let (result, output) = apply_with(&shell, &change(), true, false, "");
    result.unwrap();
    assert!(output.contains("write  a.txt") && output.contains("delete gone.txt"));
    assert!(shell.calls.borrow().is_empty());
}

#[test]
fn apply_needs_a_yes_and_refuses_an_existing_remote_branch() {
    let shell = FakeShell::new(&[]);
    let (result, output) = apply_with(&shell, &change(), false, false, "n\n");
    result.unwrap();
    assert!(output.contains("Cancelled."));
    assert!(shell.calls.borrow().is_empty());

    let shell = FakeShell::new(&[("pr list", "https://example/pull/1")]);
    let (result, _) = apply_with(&shell, &change(), false, true, "");
    assert!(result.unwrap_err().to_string().contains("already open"));
    assert!(!shell.called("worktree add"));
}

#[test]
fn the_branch_of_a_merged_pr_is_cleared_so_reinstalling_works() {
    let shell = FakeShell::new(&[("ls-remote", "abc refs/heads/chore/x")]);
    release_branch(&shell, "chore/x").unwrap();
    assert!(shell.called("push -q origin --delete chore/x"));

    let fresh = FakeShell::new(&[]);
    release_branch(&fresh, "chore/x").unwrap();
    assert!(!fresh.called("--delete"));
}

#[test]
fn a_killed_run_leaves_nothing_that_blocks_a_retry() {
    let listing = "worktree /repo\nHEAD a\nbranch refs/heads/main\n\nworktree /tmp/x\nHEAD b\nbranch refs/heads/chore/x\n\nworktree /home/me/review\nHEAD c\nbranch refs/heads/chore/x";
    let shell = FakeShell::new(&[("worktree list", listing)]);
    clear_local_leftovers(&shell, "chore/x", Path::new("/tmp"));
    assert!(shell.called("worktree prune"));
    assert!(shell.called("worktree remove --force /tmp/x"));
    assert!(!shell.called("worktree remove --force /repo"));
    assert!(!shell.called("/home/me/review"));
    assert!(shell.called("branch -q -d chore/x"));
    assert!(!shell.called("branch -q -D"));

    let failing = FakeShell::new(&[])
        .failing("worktree list")
        .failing("branch -q -d");
    clear_local_leftovers(&failing, "chore/x", Path::new("/tmp"));
}

#[test]
fn publishing_writes_files_and_opens_a_pr_without_hooks() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("gone.txt"), "x").unwrap();
    let changes = vec![
        Change {
            path: "nested/a.txt".to_string(),
            content: Some("hello\n".to_string()),
        },
        change().remove(1),
    ];
    let shell = FakeShell::new(&[("pr create", "https://example/pull/1")]);
    let url = publish_from(&shell, directory.path(), &changes, &PUBLISH, &repo()).unwrap();
    assert_eq!(url, "https://example/pull/1");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("nested/a.txt")).unwrap(),
        "hello\n"
    );
    assert!(!directory.path().join("gone.txt").exists());
    assert!(shell.called("--no-verify"));
    assert!(shell.called("--base main --head chore/x"));
}

#[test]
fn a_failed_publish_still_cleans_up_locally() {
    let shell = FakeShell::new(&[]).failing("git add");
    let (result, _) = apply_with(&shell, &[], false, true, "");
    result.unwrap();
    let mut changes = change();
    changes.remove(1);
    let (result, _) = apply_with(&shell, &changes, false, true, "");
    assert!(result.is_err());
    assert!(shell.called("worktree remove"));
    assert!(shell.called("branch -q -D chore/x"));
}

#[test]
fn an_unlistable_secret_warns_and_never_fails() {
    let shell = FakeShell::new(&[]).failing("secret list");
    let mut out = Vec::new();
    warn_missing_secret(&shell, &mut out).unwrap();
    assert!(String::from_utf8(out)
        .unwrap()
        .contains("Could not check the secret"));
    let checks = check_repo(&shell, &repo()).unwrap();
    assert!(checks
        .iter()
        .any(|(ok, line)| *ok && line.contains("Could not check")));
}

#[test]
fn a_missing_secret_is_named() {
    let mut out = Vec::new();
    warn_missing_secret(&FakeShell::new(&[("secret list", "OTHER")]), &mut out).unwrap();
    assert!(String::from_utf8(out)
        .unwrap()
        .contains("gh secret set OPENCODE_API_KEY"));
    let mut out = Vec::new();
    warn_missing_secret(&FakeShell::new(&[("secret list", SECRET_NAME)]), &mut out).unwrap();
    assert!(out.is_empty());
}

#[test]
fn check_reports_each_piece() {
    let bare = FakeShell::new(&[("secret list", "OTHER")]).failing("releases/latest");
    let empty = check_repo(&bare, &repo()).unwrap();
    assert!(empty.iter().all(|(ok, _)| !ok));

    let installed: &'static str = Box::leak(
        ensure_statuses_write(&ensure_land_input(WORKFLOW).unwrap())
            .unwrap()
            .into_boxed_str(),
    );
    let agents: &'static str = Box::leak(upsert_block("", AGENTS_BLOCK).into_boxed_str());
    let mut shell = tree(&[
        (WORKFLOW_PATH, installed),
        (CONFIG_PATH, "[land]\nmain_is_staging = true\n"),
        ("AGENTS.md", agents),
    ]);
    shell
        .answers
        .push(("secret list", Ok(SECRET_NAME.to_string())));
    shell
        .answers
        .push(("releases/latest", Err("no release".to_string())));
    let checks = check_repo(&shell, &repo()).unwrap();
    assert!(checks.iter().all(|(ok, _)| *ok), "{checks:?}");
    assert!(checks.last().unwrap().1.contains("staging step: yes"));

    // An old pin and an old block are the drift install would fix.
    let mut stale = tree(&[
        (WORKFLOW_PATH, installed),
        (
            "AGENTS.md",
            "<!-- nibble-sdlc:begin -->\nold\n<!-- nibble-sdlc:end -->\n",
        ),
    ]);
    stale
        .answers
        .push(("releases/latest", Ok("v9".to_string())));
    stale.answers.push(("commits/v9", Ok("f".repeat(40))));
    let checks = check_repo(&stale, &repo()).unwrap();
    assert!(checks
        .iter()
        .any(|(ok, line)| *ok && line.contains("not pinned to the latest release (v9)")));
    assert!(checks
        .iter()
        .any(|(ok, line)| !ok && line.contains("current delivery flow block")));

    // A permission in the wrong block is drift, even though the line exists.
    let misplaced = "permissions:
  statuses: write
jobs:
  review:
    permissions:
      contents: read
    steps:
      - uses: adlrocha/nibble-review@v1
        with:
          land: \"true\"
";
    let shell = tree(&[(WORKFLOW_PATH, misplaced)]);
    let checks = check_repo(&shell, &repo()).unwrap();
    assert!(checks
        .iter()
        .any(|(ok, line)| !ok && line.contains("statuses")));

    let invalid = tree(&[(CONFIG_PATH, "[merge]\nmethod = \"yolo\"\n")]);
    let checks = check_repo(&invalid, &repo()).unwrap();
    assert!(checks.last().unwrap().1.contains("unknown merge method"));
}

#[test]
fn the_merge_method_comes_from_the_default_branch() {
    assert_eq!(merge_method(&tree(&[]), &repo()).unwrap(), "squash");
    let shell = tree(&[(CONFIG_PATH, "[merge]\nmethod = \"rebase\"\n")]);
    assert_eq!(merge_method(&shell, &repo()).unwrap(), "rebase");
    assert!(merge_method(&tree(&[(CONFIG_PATH, "not toml =")]), &repo()).is_err());
}

#[test]
fn status_lists_land_results() {
    let prs = r#"[
      {"number": 1, "title": "One", "headRefOid": "aaa"},
      {"number": 2, "title": "Two", "headRefOid": "bbb"}
    ]"#;
    let shell = FakeShell::new(&[
        ("pr list", prs),
        ("commits/aaa/status", "shadow: would merge"),
    ]);
    assert_eq!(
        render_status(&shell).unwrap(),
        "#1 One: shadow: would merge\n#2 Two: no land result yet\n"
    );
    let partial = FakeShell::new(&[("pr list", prs)]).failing("commits/bbb/status");
    assert!(render_status(&partial)
        .unwrap()
        .contains("#2 Two: no land result yet"));
    let empty = render_status(&FakeShell::new(&[("pr list", "[]")])).unwrap();
    assert_eq!(empty, "No open pull requests.\n");
    assert!(render_status(&FakeShell::new(&[("pr list", "oops")])).is_err());
}

const VIEW: &str = r#"{"headRefOid": "abc", "title": "T"}"#;
const CLEARED: &str = "shadow: would merge";
const ASKS: &str = "shadow: would ask you (size)";
const QUOTES: &str = "shadow: would ask you (test file removed: tests/would merge.test.ts)";
const UNKNOWN: &str = "";

fn merge_with(
    land: &str,
    yes: bool,
    interactive: bool,
    auto: bool,
    answer: &str,
) -> (FakeShell, Result<()>, String) {
    let shell = FakeShell::new(&[("pr view", VIEW), ("commits/abc/status", land)]);
    let mut out = Vec::new();
    let request = MergeRequest {
        pr: 7,
        auto,
        yes,
        interactive,
        method: "squash".to_string(),
    };
    let result = merge(
        &shell,
        &request,
        &mut Cursor::new(answer.to_string()),
        &mut out,
    );
    (shell, result, String::from_utf8(out).unwrap())
}

#[test]
fn merge_pins_the_head_and_uses_the_method() {
    let (shell, result, output) = merge_with(CLEARED, true, false, false, "");
    result.unwrap();
    assert!(shell.called("pr merge 7 --squash --match-head-commit abc"));
    assert!(output.contains("Land: shadow: would merge") && output.contains("Merged."));

    let (shell, result, output) = merge_with(CLEARED, true, false, true, "");
    result.unwrap();
    assert!(shell.called("--match-head-commit abc --auto"));
    assert!(output.contains("Auto-merge enabled."));
}

#[test]
fn only_a_person_at_a_terminal_can_override_land() {
    for view in [ASKS, UNKNOWN, QUOTES] {
        let (shell, result, _) = merge_with(view, true, false, false, "");
        assert!(result.unwrap_err().to_string().contains("only a person"));
        assert!(!shell.called("pr merge"));
    }
    let (shell, result, _) = merge_with(ASKS, true, true, false, "y\n");
    result.unwrap();
    assert!(shell.called("pr merge"));
    let (shell, result, output) = merge_with(ASKS, true, true, false, "n\n");
    result.unwrap();
    assert!(output.contains("Cancelled."));
    assert!(!shell.called("pr merge"));
}

#[test]
fn a_cleared_merge_without_a_terminal_still_needs_yes() {
    let (shell, result, _) = merge_with(CLEARED, false, false, false, "");
    assert!(result.unwrap_err().to_string().contains("pass --yes"));
    assert!(!shell.called("pr merge"));
    let (shell, result, _) = merge_with(CLEARED, false, true, false, "yes\n");
    result.unwrap();
    assert!(shell.called("pr merge"));
}

#[test]
fn merge_fails_on_bad_gh_output() {
    let bad = FakeShell::new(&[("pr view", "oops")]);
    let request = MergeRequest {
        pr: 7,
        auto: false,
        yes: true,
        interactive: true,
        method: "squash".to_string(),
    };
    let mut out = Vec::new();
    assert!(merge(&bad, &request, &mut Cursor::new(String::new()), &mut out).is_err());
    let headless = FakeShell::new(&[("pr view", "{}")]);
    let error = merge(
        &headless,
        &request,
        &mut Cursor::new(String::new()),
        &mut out,
    )
    .unwrap_err();
    assert!(error.to_string().contains("no head commit"));
}

#[test]
fn load_repo_reads_the_default_branch_and_fetches_it() {
    let shell = FakeShell::new(&[("rev-parse", "/repo"), ("repo view", "trunk")]);
    let repo = load_repo(&shell).unwrap();
    assert_eq!(repo.default_branch, "trunk");
    assert!(shell.called("fetch -q origin trunk"));
    let outside = FakeShell::new(&[]).failing("rev-parse");
    assert!(load_repo(&outside).is_err());
}
