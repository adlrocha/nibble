# Blueprint: AI Factory Pipeline Integration

## Summary

Integrates a structured, chip-design-inspired development pipeline ("AI Factory") into
nibble. Every sandbox agent receives pipeline instructions via `AGENTS.md` (the primary
instruction file, read by both OpenCode and Claude Code) and has access to per-stage
skill files installed at `~/.claude/skills/factory-<name>/SKILL.md`. The pipeline
enforces Spec→Implement→TDD→Adversarial→Risk Score→QA Gate for every non-trivial task,
with human intervention only at the QA Gate for Critical/High risk sections.

## Scope

- In scope:
  - 8 skill files (factory-pipeline, factory-spec, factory-implement, factory-tdd,
    factory-adversarial, factory-risk-score, factory-qa-gate, factory-lessons)
  - `AGENTS.md` at repo root (primary instruction file for all agents)
  - `.claude/CLAUDE.md` updated to import `AGENTS.md` via `@AGENTS.md`
  - `src/main.rs`: new `build_sandbox_agents_md()`, updated `build_sandbox_claude_md()`,
    updated `inject_sandbox_claude_md()` to write both files into spawned containers
  - `src/config.rs`: `FactoryConfig { enabled: bool }` with default `true`
  - `src/cli/mod.rs`: `--factory <bool>` flag on `sandbox spawn`
  - `src/notifications/telegram_listener.rs`: passes `factory.enabled` to spawn
  - `install.sh`: installs skills to `~/.claude/skills/`, writes global
    `~/.config/opencode/AGENTS.md` (from `AGENTS.global.md`)
  - `AGENTS.global.md`: host-facing version of AGENTS.md — factory pipeline
    section only, no sandbox environment/toolchain blocks
  - `skills/README.md`: documents the pipeline and directory layout

- Out of scope:
  - Automatic pipeline enforcement (agent must choose to follow instructions)
  - Telegram commands to trigger pipeline stages
  - Storing pipeline run history in the nibble database
  - CI/CD integration

- Dependencies:
  - nibble sandbox spawn infrastructure (already exists)
  - Claude Code skill discovery (`~/.claude/skills/`)
  - OpenCode AGENTS.md support (`~/.config/opencode/AGENTS.md`, project `AGENTS.md`)

## Interfaces

### Public API / Exports

| Name | Input | Output | Errors |
|------|-------|--------|--------|
| `build_sandbox_agents_md(repo_name, toolchains, factory_enabled)` | repo name, detected toolchains, factory flag | `String` — AGENTS.md content | none |
| `build_sandbox_claude_md(repo_name, toolchains, factory_enabled)` | same | `String` — nibble delimiter block for CLAUDE.md | none |
| `inject_sandbox_claude_md(container_id, claude_block, agents_content)` | container id, both content strings | `Result<()>` | podman exec failure, bash script error |
| `FactoryConfig { enabled: bool }` | — | config struct, default `true` | — |
| `SandboxAction::Spawn { factory: Option<bool> }` | CLI flag | overrides config default | — |

### Data Types

**`FactoryConfig`** (`src/config.rs`):
- `enabled: bool` — default `true`
- Deserializes from `[factory]` section in `~/.agent-tasks/config.toml`

### Events / Side Effects

On `sandbox spawn`:
1. `build_sandbox_agents_md()` generates AGENTS.md content (pure, no I/O)
2. `build_sandbox_claude_md()` generates CLAUDE.md nibble block (pure, no I/O)
3. `inject_sandbox_claude_md()` runs a bash script via `podman exec` that:
   - Writes `/workspace/AGENTS.md` (always overwritten)
   - Creates `/workspace/.claude/` if missing
   - Prepends `@AGENTS.md` import line to `/workspace/.claude/CLAUDE.md` if not present
   - Replaces the nibble delimiter block in CLAUDE.md, or appends if no block exists

On `install.sh`:
- Copies `skills/factory-<name>/SKILL.md` to `~/.claude/skills/factory-<name>/SKILL.md`
- Copies `AGENTS.md` to `~/.config/opencode/AGENTS.md`

## State Machine / Flow

```
sandbox spawn called
  │
  ├── factory_enabled = CLI --factory flag OR config.factory.enabled (default true)
  │
  ├── build_sandbox_agents_md() → agents_content
  ├── build_sandbox_claude_md() → claude_block
  │
  └── inject_sandbox_claude_md(container, claude_block, agents_content)
        │
        ├── podman exec: write /workspace/AGENTS.md
        └── podman exec: update /workspace/.claude/CLAUDE.md
              ├── file missing → create with @AGENTS.md + nibble block
              ├── file exists, no @AGENTS.md line → prepend it, then update block
              └── file exists, has nibble block → replace block only
```

## Invariants

1. (INV-1) `AGENTS.md` is always written atomically via `printf '%s\n' ... > file` — no
   partial writes visible to concurrent readers.
2. (INV-2) The `@AGENTS.md` import line appears at most once in CLAUDE.md — the script
   checks with `grep -qF` before prepending.
3. (INV-3) User-written content outside the nibble delimiter block in CLAUDE.md is never
   modified — only the block between `<!-- nibble:begin -->` and `<!-- nibble:end -->` is
   replaced.
4. (INV-4) `factory_enabled = false` disables factory instructions in AGENTS.md but does
   not change the skill files on disk — skills remain available for manual use.
5. (INV-5) Skill files always have YAML frontmatter with `name` and `description` fields.
6. (INV-6) The `--factory` CLI flag always takes precedence over `config.factory.enabled`.

## Error Handling Strategy

- `inject_sandbox_claude_md` propagates `podman exec` failures as `anyhow::Error` —
  the caller prints a warning but does not abort spawn (non-fatal).
- Config load failures fall back to `Config::default()` (factory enabled by default).
- `install.sh` uses `set -e` — any skill copy failure aborts the install.
- Skill YAML frontmatter parse errors are the agent's problem at runtime, not nibble's.

## Acceptance Criteria

1. (AC-1) After `sandbox spawn`, `/workspace/AGENTS.md` exists inside the container and
   contains the factory pipeline instructions when `factory_enabled = true`.
2. (AC-2) After `sandbox spawn`, `/workspace/.claude/CLAUDE.md` starts with `@AGENTS.md`
   and contains the toolchain section in the nibble delimiter block.
3. (AC-3) Running `inject_sandbox_claude_md` twice on a container with existing CLAUDE.md
   does not duplicate the `@AGENTS.md` line or the nibble delimiter block.
4. (AC-4) User content outside the nibble delimiter block is preserved across re-injections.
5. (AC-5) `nibble sandbox spawn --factory false` produces AGENTS.md without the factory
   pipeline section.
6. (AC-6) After `install.sh`, `~/.claude/skills/factory-spec/SKILL.md` exists and has
   valid YAML frontmatter.
7. (AC-7) After `install.sh`, `~/.config/opencode/AGENTS.md` exists and matches the repo
   `AGENTS.md`.
8. (AC-8) `cargo test` passes with zero failures after all changes.

## Constraints

- Bash script inside container must work with POSIX sh + awk (no Python, no jq)
- Skill files must be plain markdown — no agent-specific syntax
- `AGENTS.md` must be cross-agent compatible (OpenCode + Claude Code)
- Factory must be on by default without requiring user configuration

## Open Questions

None — all design decisions were resolved during implementation.
