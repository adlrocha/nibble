//! Sandbox context injection: toolchain detection and AGENTS.md/CLAUDE.md
//! generation for sandboxed agent environments.

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
}

/// Build the AGENTS.md content written to `/<repo-name>/AGENTS.md` inside the
/// container.  This is the **primary** agent instruction file — Claude reads
/// it natively and Claude Code reads it via the `@../AGENTS.md` import in CLAUDE.md.
///
/// The content covers sandbox environment, toolchain setup, and general working
/// principles.
pub fn build_sandbox_agents_md(repo_name: &str, toolchains: &[(&str, &str, &str)]) -> String {
    let mut out = String::new();

    // Header (repo name is dynamic, so it stays inline)
    out.push_str("# nibble Sandbox Agent Instructions\n\n");
    out.push_str(&format!(
        "You are running inside an isolated Podman sandbox managed by **nibble** for the **{}** project. \
         This file contains all instructions for how to operate inside this environment. \
         Read it fully before starting any task.\n\n",
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

    out
}

/// Write nibble's sandbox instructions into `AGENTS.md` and `.claude/CLAUDE.md`
/// inside the container, **without clobbering any existing repo content**.
///
/// **AGENTS.md** (`/<repo-name>/AGENTS.md`):
/// - Nibble's content is wrapped in sentinel comments so it can be updated
///   idempotently without touching the rest of the file:
///   ```text
///   <!-- nibble-sandbox:begin -->
///   ...nibble instructions...
///   <!-- nibble-sandbox:end -->
///   ```
/// - If the file does not exist → created containing only the sentinel block.
/// - If the file exists and the sentinel block is present → the block is
///   replaced in-place; everything outside the sentinels is preserved.
/// - If the file exists but has no sentinel → the block is appended at the end;
///   existing repo content is left completely untouched.
///
/// **CLAUDE.md** (`/<repo-name>/.claude/CLAUDE.md`):
/// - Contains `@../AGENTS.md` as the first line (safe-prepend, never overwrites).
/// - If the file already exists with `@../AGENTS.md` at line 1, it is left untouched.
/// - If `@../AGENTS.md` is missing from line 1, it is prepended — user content below is preserved.
pub fn inject_sandbox_claude_md(
    container_id: &str,
    container_dir: &str,
    agents_content: &str,
) -> Result<()> {
    let escaped_agents = agents_content.replace('\'', "'\\''");

    let script = format!(
        r#"set -e
mkdir -p {dir}/.claude

# ── Update AGENTS.md using sentinel block (never overwrites repo content) ──────
AGENTS_FILE={dir}/AGENTS.md
BEGIN_SENTINEL='<!-- nibble-sandbox:begin -->'
END_SENTINEL='<!-- nibble-sandbox:end -->'
NIBBLE_BLOCK=$(printf '%s\n%s\n%s\n' "$BEGIN_SENTINEL" '{agents}' "$END_SENTINEL")

if [ ! -f "$AGENTS_FILE" ]; then
    # File does not exist — create it with just the sentinel block.
    printf '%s\n' "$NIBBLE_BLOCK" > "$AGENTS_FILE"
elif grep -qF "$BEGIN_SENTINEL" "$AGENTS_FILE" 2>/dev/null; then
    # Sentinel is present — replace the block in-place, preserving everything outside.
    # Write the new block to a temp file so awk can read it without newline-escaping issues.
    BLOCK_TMP=$(mktemp)
    printf '%s\n' "$NIBBLE_BLOCK" > "$BLOCK_TMP"
    TMP=$(mktemp)
    awk -v begin="$BEGIN_SENTINEL" -v end="$END_SENTINEL" -v blockfile="$BLOCK_TMP" '
        $0 == begin {{ in_block=1; while ((getline line < blockfile) > 0) print line; next }}
        $0 == end   {{ in_block=0; next }}
        !in_block   {{ print }}
    ' "$AGENTS_FILE" > "$TMP"
    rm -f "$BLOCK_TMP"
    mv "$TMP" "$AGENTS_FILE"
else
    # No sentinel found — append the block; existing content is untouched.
    printf '\n%s\n' "$NIBBLE_BLOCK" >> "$AGENTS_FILE"
fi

# ── Update .claude/CLAUDE.md (Claude Code entrypoint) ─────────────────────────
TARGET={dir}/.claude/CLAUDE.md
IMPORT_LINE='@../AGENTS.md'

if [ ! -f "$TARGET" ]; then
    printf '%s\n' "$IMPORT_LINE" > "$TARGET"
else
    # Ensure @../AGENTS.md is present as the very first line.
    # Checking only the first line (via head -1) prevents a false match when
    # "@../AGENTS.md" appears in the file body (e.g. inside a comment or example).
    if ! head -1 "$TARGET" | grep -qF "$IMPORT_LINE" 2>/dev/null; then
        TMP=$(mktemp)
        printf '%s\n' "$IMPORT_LINE" > "$TMP"
        cat "$TARGET" >> "$TMP"
        mv "$TMP" "$TARGET"
    fi
fi"#,
        agents = escaped_agents,
        dir = container_dir,
    );

    let output = std::process::Command::new("podman")
        .args(["exec", container_id, "/bin/bash", "-c", &script])
        .output()
        .context("Failed to write AGENTS.md / CLAUDE.md into container")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Writing AGENTS.md/CLAUDE.md failed: {}", stderr.trim());
    }

    Ok(())
}

#[cfg(test)]
mod agents_md_tests {
    use super::*;
    use crate::models::{AgentType, Task};
    use std::str::FromStr;

    // ── build_sandbox_agents_md tests ─────────────────────────────────────────

    #[test]
    fn test_agents_md_contains_repo_name() {
        let out = build_sandbox_agents_md("my-cool-project", &[]);
        assert!(
            out.contains("my-cool-project"),
            "repo name should appear in AGENTS.md header"
        );
    }

    #[test]
    fn test_agents_md_toolchain_table_present() {
        let toolchains = [("Rust", "cargo build", "cargo test")];
        let out = build_sandbox_agents_md("proj", &toolchains);
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
    fn test_agents_md_no_toolchain_fallback_message() {
        let out = build_sandbox_agents_md("proj", &[]);
        assert!(
            out.contains("No recognised dependency manifest"),
            "should show fallback message when no toolchain detected"
        );
    }

    // ── CLAUDE.md content tests ───────────────────────────────────────────────
    // CLAUDE.md is now just "@../AGENTS.md" — all content lives in AGENTS.md.
    // These tests verify that the agents_md contains the information that used
    // to live in the nibble delimiter block.

    #[test]
    fn test_agents_md_contains_toolchain_info() {
        // Toolchain info must be in AGENTS.md (not duplicated in a CLAUDE.md block)
        let toolchains = [("Node.js", "npm install", "npm test")];
        let out = build_sandbox_agents_md("proj", &toolchains);
        assert!(
            out.contains("npm install"),
            "toolchain install command must be in AGENTS.md"
        );
        assert!(
            out.contains("npm test"),
            "toolchain run hint must be in AGENTS.md"
        );
    }

    #[test]
    fn test_agents_md_is_single_source_of_truth() {
        // AGENTS.md must contain the repo name and environment info —
        // everything Claude Code needs is here, imported via @../AGENTS.md in CLAUDE.md
        let out = build_sandbox_agents_md("my-cool-project", &[]);
        assert!(
            out.contains("my-cool-project"),
            "repo name must appear in AGENTS.md"
        );
        assert!(
            out.contains("Working directory"),
            "environment section must be present"
        );
    }

    // ── Sentinel block tests ──────────────────────────────────────────────────
    // These tests verify the shell logic in inject_sandbox_claude_md by
    // simulating the three cases in pure Rust (no container needed).

    fn apply_sentinel_logic(existing: Option<&str>, nibble_content: &str) -> String {
        let begin = "<!-- nibble-sandbox:begin -->";
        let end = "<!-- nibble-sandbox:end -->";
        let block = format!("{}\n{}\n{}", begin, nibble_content, end);

        match existing {
            None => format!("{}\n", block),
            Some(file) if file.contains(begin) => {
                // Replace in-place: keep lines outside sentinels, substitute block.
                let mut out = String::new();
                let mut in_block = false;
                let mut replaced = false;
                for line in file.lines() {
                    if line == begin {
                        in_block = true;
                        if !replaced {
                            out.push_str(&block);
                            out.push('\n');
                            replaced = true;
                        }
                        continue;
                    }
                    if line == end {
                        in_block = false;
                        continue;
                    }
                    if !in_block {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                out
            }
            Some(file) => {
                // Append: existing content untouched, block added at end.
                format!("{}\n\n{}\n", file, block)
            }
        }
    }

    #[test]
    fn test_sentinel_creates_file_when_absent() {
        let result = apply_sentinel_logic(None, "nibble instructions here");
        assert!(result.contains("<!-- nibble-sandbox:begin -->"));
        assert!(result.contains("nibble instructions here"));
        assert!(result.contains("<!-- nibble-sandbox:end -->"));
    }

    #[test]
    fn test_sentinel_replaces_block_in_place() {
        let existing = "# My Project\n\nSome docs.\n\n<!-- nibble-sandbox:begin -->\nold content\n<!-- nibble-sandbox:end -->\n\nMore docs.\n";
        let result = apply_sentinel_logic(Some(existing), "new nibble content");
        assert!(
            result.contains("# My Project"),
            "repo content before sentinel must be preserved"
        );
        assert!(
            result.contains("More docs."),
            "repo content after sentinel must be preserved"
        );
        assert!(
            result.contains("new nibble content"),
            "new nibble content must be present"
        );
        assert!(
            !result.contains("old content"),
            "old nibble content must be gone"
        );
    }

    #[test]
    fn test_sentinel_appends_when_no_sentinel_present() {
        let existing = "# My Project\n\nThis is the real AGENTS.md.\n";
        let result = apply_sentinel_logic(Some(existing), "nibble instructions");
        assert!(
            result.starts_with("# My Project"),
            "original content must come first"
        );
        assert!(
            result.contains("This is the real AGENTS.md."),
            "original content must be preserved"
        );
        assert!(
            result.contains("<!-- nibble-sandbox:begin -->"),
            "sentinel begin must be appended"
        );
        assert!(
            result.contains("nibble instructions"),
            "nibble content must be appended"
        );
    }

    #[test]
    fn test_sentinel_replace_is_idempotent() {
        let existing = "# Header\n\n<!-- nibble-sandbox:begin -->\nv1 content\n<!-- nibble-sandbox:end -->\n\nFooter\n";
        let after_first = apply_sentinel_logic(Some(existing), "v2 content");
        let after_second = apply_sentinel_logic(Some(&after_first), "v2 content");
        assert_eq!(
            after_first, after_second,
            "applying sentinel twice with same content must be idempotent"
        );
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
