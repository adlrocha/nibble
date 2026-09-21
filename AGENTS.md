<!-- nibble-sandbox:begin -->
# nibble Sandbox Agent Instructions

You are running inside an isolated Podman sandbox managed by **nibble** for the **nibble** project. This file contains all instructions for how to operate inside this environment. Read it fully before starting any task.

## Environment

- **Working directory**: `/nibble (the project repo, mounted read-write)
- **Full sudo access**: install any system package with `apt-get install`
- **Ports forwarded** to the host: services on `localhost:3000`, `:8080`, etc. are reachable from outside
- **Internet access** is available
- **Git** is configured with the host user's identity and SSH keys
- `claude` is available if you need a nested agent session

## Toolchain Setup

Project dependencies are installed automatically at sandbox spawn via `.nibble/setup.sh` if that script exists. By the time you receive a task, dependencies should already be built and ready.

- If `.nibble/setup.sh` **exists**: it was already run at spawn — do not re-run it unless something is broken. If you need a new system dependency or build step, update the script and run it manually once, then commit the change.
- If `.nibble/setup.sh` **does not exist**: dependencies won't be pre-installed. Check for manifest files below and install them yourself. Create (or ask to create) `.nibble/setup.sh` so future spawns are automatic.

The following dependency manifests were detected:

| Manifest | Install command | Run/test |
|----------|----------------|----------|
| Rust | `cargo build  # rustup + cargo pre-installed by .nibble/setup.sh; binary at ~/.cargo/bin/cargo` | `cargo run / cargo test` |

If a command fails due to missing system tools, install them with `sudo apt-get install <package>`.

## General Working Principles

- Make small, focused changes and run tests after each one
- The container persists between sessions — installed packages and build artifacts are retained
- When you finish a task, summarise what you did clearly so the notification sent to the user is informative
- Ask before making changes outside the project's stated scope

## Skills

Skills are stored on the **host** at `~/.claude/skills/` and bind-mounted into every sandbox at `/home/node/.claude/skills/`. This means:

- Skill updates made inside a sandbox are immediately visible on the host and in all other sandboxes — they share the same directory.
- The host `install.sh` re-installs skills from the nibble repo to `~/.claude/skills/` whenever you update them in source. Run it after editing skills in the repo.

<!-- nibble:global:begin -->

## Coding Behavior Guidelines

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

### 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:

- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

### 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

### 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:

- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:

- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

### 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:

- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:

```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.

<!-- nibble:global:end -->

<!-- nibble-sandbox:end -->
