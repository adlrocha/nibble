//! Sandbox context: toolchain detection and the sandbox notes handed to the
//! agent as an appended system prompt.
//!
//! The notes are written to [`SANDBOX_PROMPT_PATH`], a container-local file, and
//! passed with `--append-system-prompt` when an agent is launched. Nothing is
//! written into the repository (it is bind-mounted from the host) or into the
//! agent home directories (`~/.claude`, `~/.pi`, `~/.omp` are host mounts too).

use anyhow::{Context, Result};

/// Detect the project toolchain from files present in the repo directory.
///
/// Returns a list of (ecosystem, install_command, run_hint) tuples for every
/// recognised manifest found so Claude can install deps and run the project
/// without guessing.
pub fn detect_toolchains(
    repo_path: &std::path::Path,
) -> Vec<(&'static str, &'static str, &'static str)> {
    let checks: &[(&str, &str, &str, &str)] = &[
        // (manifest file, ecosystem label, install cmd, run hint)
        (
            "package.json",
            "Node.js",
            "npm install",
            "npm start / npm test / npm run dev",
        ),
        (
            "yarn.lock",
            "Node.js",
            "yarn install",
            "yarn start / yarn test / yarn dev",
        ),
        (
            "pnpm-lock.yaml",
            "Node.js",
            "pnpm install",
            "pnpm start / pnpm test / pnpm dev",
        ),
        (
            "Cargo.toml",
            "Rust",
            "cargo build  # rustup + cargo pre-installed by .nibble/setup.sh; binary at ~/.cargo/bin/cargo",
            "cargo run / cargo test",
        ),
        (
            "go.mod",
            "Go",
            "go mod download",
            "go run . / go test ./...",
        ),
        (
            "requirements.txt",
            "Python",
            "pip install -r requirements.txt",
            "python main.py / pytest",
        ),
        (
            "pyproject.toml",
            "Python",
            "pip install -e .",
            "python -m pytest / python -m <module>",
        ),
        (
            "Pipfile",
            "Python",
            "pipenv install",
            "pipenv run python ... / pipenv run pytest",
        ),
        (
            "composer.json",
            "PHP",
            "composer install",
            "php artisan serve / php -S localhost:8000",
        ),
        (
            "Gemfile",
            "Ruby",
            "bundle install",
            "bundle exec rails s / bundle exec rspec",
        ),
        (
            "build.gradle",
            "JVM",
            "./gradlew build",
            "./gradlew run / ./gradlew test",
        ),
        (
            "pom.xml",
            "JVM",
            "mvn install -DskipTests",
            "mvn exec:java / mvn test",
        ),
        ("mix.exs", "Elixir", "mix deps.get", "mix run / mix test"),
        ("Makefile", "Make", "make", "make run / make test"),
    ];

    // Deduplicate: if yarn.lock is present package.json will also be — prefer
    // the more specific lock-file entry over the generic one.
    let mut seen_ecosystems = std::collections::HashSet::new();
    let mut results = Vec::new();

    for (manifest, ecosystem, install, run_hint) in checks {
        if repo_path.join(manifest).exists() && seen_ecosystems.insert(*ecosystem) {
            results.push((*ecosystem, *install, *run_hint));
        }
    }

    results
}

/// Static sandbox instruction fragments embedded at compile time.
/// Edit the .md files under `src/sandbox_instructions/` — never edit here.
mod sandbox_instructions {
    pub const BASE: &str = include_str!("../sandbox_instructions/base.md");
    pub const GENERAL_PRINCIPLES: &str =
        include_str!("../sandbox_instructions/general_principles.md");
    pub const SKILLS: &str = include_str!("../sandbox_instructions/skills.md");
}

/// Container-local file holding the sandbox notes. `/etc` is not mounted from
/// the host, so writing it never touches the user's files.
pub const SANDBOX_PROMPT_PATH: &str = "/etc/nibble/sandbox.md";

/// Build the sandbox notes appended to the agent's system prompt: the sandbox
/// environment, the detected toolchain, and general working principles.
///
/// ```ignore
/// let notes = build_sandbox_prompt("nibble", &[("Rust", "cargo build", "cargo test")]);
/// assert!(notes.contains("cargo build"));
/// ```
pub fn build_sandbox_prompt(repo_name: &str, toolchains: &[(&str, &str, &str)]) -> String {
    let mut out = String::new();

    // Header (repo name is dynamic, so it stays inline)
    out.push_str("# nibble sandbox\n\n");
    out.push_str(&format!(
        "You are running inside an isolated Podman sandbox managed by **nibble** for the **{}** project. \
         These notes describe the environment; the repository's own instructions still apply.\n\n",
        repo_name
    ));

    // Static: environment bullets + toolchain setup preamble
    out.push_str(sandbox_instructions::BASE);
    out.push('\n');

    // Dynamic: detected toolchain table (parametric, cannot be static)
    if toolchains.is_empty() {
        out.push_str("No recognised dependency manifest was found in the repo root.\n");
        out.push_str(
            "Inspect the project structure and install any required tools before running or testing.\n",
        );
    } else {
        out.push_str("The following dependency manifests were detected:\n\n");
        out.push_str("| Manifest | Install command | Run/test |\n");
        out.push_str("|----------|----------------|----------|\n");
        for (ecosystem, install_cmd, run_hint) in toolchains {
            out.push_str(&format!(
                "| {} | `{}` | `{}` |\n",
                ecosystem, install_cmd, run_hint
            ));
        }
        out.push('\n');
        out.push_str(
            "If a command fails due to missing system tools, install them with \
             `sudo apt-get install <package>`.\n",
        );
    }

    // Static: general working principles
    out.push('\n');
    out.push_str(sandbox_instructions::GENERAL_PRINCIPLES);
    out.push('\n');
    out.push_str(sandbox_instructions::SKILLS);

    out
}

/// Write the sandbox notes to [`SANDBOX_PROMPT_PATH`] inside the container.
///
/// The content is streamed over stdin as root, so no shell escaping is needed
/// and the file is replaced on every spawn.
pub fn write_sandbox_prompt(container_id: &str, content: &str) -> Result<()> {
    use std::io::Write;

    let dir = std::path::Path::new(SANDBOX_PROMPT_PATH)
        .parent()
        .and_then(|p| p.to_str())
        .context("Sandbox prompt path has no parent directory")?;
    let script =
        format!("mkdir -p {dir} && cat > {SANDBOX_PROMPT_PATH} && chmod 644 {SANDBOX_PROMPT_PATH}");
    let mut child = std::process::Command::new("podman")
        .args([
            "exec",
            "-i",
            "-u",
            "root",
            container_id,
            "/bin/sh",
            "-c",
            &script,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to start podman exec to write the sandbox notes")?;
    child
        .stdin
        .take()
        .context("podman exec has no stdin")?
        .write_all(content.as_bytes())
        .context("Failed to stream the sandbox notes into the container")?;
    let output = child
        .wait_with_output()
        .context("Failed to wait for podman exec writing the sandbox notes")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Writing {SANDBOX_PROMPT_PATH} failed: {}", stderr.trim());
    }
    Ok(())
}

/// Shell statement that loads the sandbox notes into the bash array
/// `nibble_prompt`, as `--append-system-prompt <notes>`, or leaves it empty when
/// the file is missing (a sandbox spawned by an older nibble).
///
/// Expand it in a launch command with [`SANDBOX_PROMPT_ARGS`].
pub fn sandbox_prompt_setup() -> String {
    format!(
        r#"nibble_prompt=(); if [ -r {SANDBOX_PROMPT_PATH} ]; then nibble_prompt=(--append-system-prompt "$(cat {SANDBOX_PROMPT_PATH})"); fi"#
    )
}

/// Expands `nibble_prompt` (loaded by [`sandbox_prompt_setup`]) as agent
/// arguments. Owned here so the array's name lives in one module.
pub const SANDBOX_PROMPT_ARGS: &str = r#""${nibble_prompt[@]}""#;

/// `podman exec` arguments that run the program given after them with the
/// sandbox notes added, for launches without a shell of their own (headless
/// turns from cron, inject and quota-watch).
///
/// The program and its arguments are passed to bash as positional parameters,
/// so nothing is re-quoted:
///
/// ```ignore
/// cmd.args(exec_with_sandbox_prompt()).arg("pi").args(["--session", s, "-p", msg]);
/// ```
pub fn exec_with_sandbox_prompt() -> [String; 3] {
    [
        "/bin/bash".to_string(),
        "-c".to_string(),
        format!(
            r#"{}; exec "$0" {SANDBOX_PROMPT_ARGS} "$@""#,
            sandbox_prompt_setup()
        ),
    ]
}

/// Marker older nibble versions wrote into a repository's `AGENTS.md`.
const LEGACY_SANDBOX_BLOCK: &str = "<!-- nibble-sandbox:begin -->";

/// Whether the repository still carries the sandbox block an older nibble wrote
/// into `AGENTS.md`. Nothing refreshes it any more, so it goes stale.
pub fn has_legacy_sandbox_block(repo: &std::path::Path) -> bool {
    std::fs::read_to_string(repo.join("AGENTS.md"))
        .is_ok_and(|text| text.contains(LEGACY_SANDBOX_BLOCK))
}

/// Shell prefix for a pi-family launch: load the notes, then shadow `bin` with a
/// function that adds them, so every later `bin …` in the command (fresh or
/// resumed) carries the notes without rewriting each invocation.
///
/// It goes after the install step, whose `command -v bin` must still see the
/// real binary rather than this function.
pub fn pi_prompt_wrapper(bin: &str) -> String {
    format!(
        r#"{setup}; {bin}() {{ command {bin} {args} "$@"; }}"#,
        setup = sandbox_prompt_setup(),
        args = SANDBOX_PROMPT_ARGS
    )
}

#[cfg(test)]
mod sandbox_prompt_tests {
    use super::*;
    use crate::models::{AgentType, Task};
    use std::str::FromStr;

    // ── build_sandbox_prompt tests ─────────────────────────────────────────

    #[test]
    fn test_prompt_contains_repo_name() {
        let out = build_sandbox_prompt("my-cool-project", &[]);
        assert!(
            out.contains("my-cool-project"),
            "repo name should appear in the header"
        );
    }

    #[test]
    fn test_prompt_toolchain_table_present() {
        let toolchains = [("Rust", "cargo build", "cargo test")];
        let out = build_sandbox_prompt("proj", &toolchains);
        assert!(
            out.contains("cargo build"),
            "install command should be in toolchain table"
        );
        assert!(
            out.contains("cargo test"),
            "run hint should be in toolchain table"
        );
    }

    #[test]
    fn test_prompt_no_toolchain_fallback_message() {
        let out = build_sandbox_prompt("proj", &[]);
        assert!(
            out.contains("No recognised dependency manifest"),
            "should show fallback message when no toolchain detected"
        );
    }

    #[test]
    fn test_prompt_contains_environment_and_repo() {
        let out = build_sandbox_prompt("my-cool-project", &[]);
        assert!(out.contains("my-cool-project"));
        assert!(out.contains("Working directory"));
        assert!(
            !out.contains("This file"),
            "the notes are a prompt, not a file in the repo"
        );
    }

    #[test]
    fn test_prompt_setup_reads_container_local_file() {
        let setup = sandbox_prompt_setup();
        assert!(setup.contains(SANDBOX_PROMPT_PATH));
        assert!(setup.contains("--append-system-prompt"));
        assert!(
            setup.contains(&format!("[ -r {SANDBOX_PROMPT_PATH} ]")),
            "a sandbox without the file must still launch"
        );
        assert!(SANDBOX_PROMPT_PATH.starts_with("/etc/"));
    }

    #[test]
    fn test_prompt_includes_skills_guidance() {
        let out = build_sandbox_prompt("proj", &[]);
        assert!(out.contains("## Skills"));
        assert!(out.contains("/home/node/.claude/skills/"));
    }

    #[test]
    fn test_legacy_sandbox_block_is_detected() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(!has_legacy_sandbox_block(tmp.path()), "no AGENTS.md");
        std::fs::write(tmp.path().join("AGENTS.md"), "# Own rules\n").unwrap();
        assert!(!has_legacy_sandbox_block(tmp.path()));
        std::fs::write(
            tmp.path().join("AGENTS.md"),
            "# Own\n<!-- nibble-sandbox:begin -->\nold\n<!-- nibble-sandbox:end -->\n",
        )
        .unwrap();
        assert!(has_legacy_sandbox_block(tmp.path()));
    }

    /// The Claude launch expands the same array the setup fills, so the notes
    /// arrive as one argument there too, and nothing when the file is missing.
    #[test]
    fn test_claude_prompt_args_expand_the_setup_array() {
        let tmp = tempfile::TempDir::new().unwrap();
        let notes = tmp.path().join("sandbox.md");
        std::fs::write(&notes, "a 'b'\n$c").unwrap();
        let run = |path: &str| {
            let script = format!(
                "{}; printf '<%s>' x {SANDBOX_PROMPT_ARGS} y",
                sandbox_prompt_setup().replace(SANDBOX_PROMPT_PATH, path)
            );
            let out = std::process::Command::new("bash")
                .args(["-c", &script])
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(
            run(notes.to_str().unwrap()),
            "<x><--append-system-prompt><a 'b'\n$c><y>"
        );
        assert_eq!(run("/nonexistent"), "<x><y>");
    }

    /// Headless launches pass the program and its arguments through bash
    /// untouched, with the notes inserted right after the program.
    #[test]
    fn test_exec_with_sandbox_prompt_keeps_arguments_intact() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let notes = tmp.path().join("sandbox.md");
        std::fs::write(&notes, "notes").unwrap();
        let bin = tmp.path().join("showargs");
        std::fs::write(
            &bin,
            "#!/bin/sh\nfor a in \"$@\"; do printf '<%s>' \"$a\"; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let run = |path: &str| {
            let [shell, flag, script] = exec_with_sandbox_prompt();
            let out = std::process::Command::new(shell)
                .args([flag, script.replace(SANDBOX_PROMPT_PATH, path)])
                .arg(&bin)
                .args(["-p", "it's \"$HOME\"; rm -rf /"])
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(
            run(notes.to_str().unwrap()),
            "<--append-system-prompt><notes><-p><it's \"$HOME\"; rm -rf />"
        );
        assert_eq!(run("/nonexistent"), "<-p><it's \"$HOME\"; rm -rf />");
    }

    #[test]
    fn test_pi_prompt_wrapper_shadows_binary() {
        let wrapper = pi_prompt_wrapper("omp");
        assert!(wrapper.starts_with(&sandbox_prompt_setup()));
        assert!(wrapper.ends_with(r#"omp() { command omp "${nibble_prompt[@]}" "$@"; }"#));
    }

    /// The setup and wrapper must run under bash and pass the notes as a single
    /// argument, even when they contain quotes, `$` and newlines.
    #[test]
    fn test_pi_prompt_wrapper_passes_notes_as_one_argument() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let notes = tmp.path().join("sandbox.md");
        std::fs::write(&notes, "line 'one'\n\"two\" $HOME").unwrap();
        let bin = tmp.path().join("showargs");
        std::fs::write(
            &bin,
            "#!/bin/sh\nfor a in \"$@\"; do printf '<%s>' \"$a\"; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let run = |notes_path: &str| {
            let script = pi_prompt_wrapper("showargs").replace(SANDBOX_PROMPT_PATH, notes_path)
                + "; showargs --session x";
            let out = std::process::Command::new("bash")
                .args(["-c", &script])
                .env("PATH", format!("{}:/usr/bin:/bin", tmp.path().display()))
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };

        assert_eq!(
            run(notes.to_str().unwrap()),
            "<--append-system-prompt><line 'one'\n\"two\" $HOME><--session><x>"
        );
        assert_eq!(run("/nonexistent/sandbox.md"), "<--session><x>");
    }

    // ── Per-agent session ID routing tests ────────────────────────────────────
    // These tests verify the per-agent session-id acceptance criteria and invariants.

    fn make_db_with_task(agent_type: &str) -> (tempfile::TempDir, crate::db::Database, Task) {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let db = crate::db::Database::open(tmp.path().join("test.db")).unwrap();
        let task = Task::new(
            format!("task-{}", agent_type),
            AgentType::from_str(agent_type).unwrap(),
            "Test task".to_string(),
            None,
            None,
        );
        db.insert_task(&task).unwrap();
        (tmp, db, task)
    }

    /// AC-5 / INV-3: report session-id for a claude_code task writes claude_session_id,
    #[test]
    fn test_ac5_report_session_id_routes_to_claude_field_replacement() {
        use crate::models::TaskContext;
        use std::collections::HashMap;

        let (_tmp, db, mut task) = make_db_with_task("claude_code");
        let sid = "550e8400-e29b-41d4-a716-446655440000".to_string();
        let ctx = task.context.get_or_insert_with(|| TaskContext {
            url: None,
            project_path: None,
            session_id: None,
            claude_session_id: None,
            extra: HashMap::new(),
        });
        ctx.claude_session_id = Some(sid.clone());
        db.update_task(&task).unwrap();

        let reloaded = db.get_task_by_id(&task.task_id).unwrap().unwrap();
        let ctx = reloaded.context.as_ref().unwrap();
        assert_eq!(
            ctx.claude_session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }
}
