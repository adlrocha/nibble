# Blueprint: Pi Agent Sandbox Support

## Summary

Add `AgentType::Pi` to nibble so users can spawn and attach to sandboxed
[pi](https://github.com/badlogic/pi-mono) coding-agent sessions.  Pi is an npm
package (`@mariozechner/pi-coding-agent`) that supports 15+ LLM providers
(including GitHub Copilot, Kimi, GLM) via a single interactive TUI.  The
integration follows the same pattern as OpenCode: install at spawn time, mount the
host config directory so auth/sessions/extensions are shared, resume via `-c`
(continue last session), support `--fresh` (wipe the latest session file) and
`--btw` (throw-away independent session), and inject Telegram messages via
`pi -p` (print/non-interactive mode).  Skills live in `~/.claude/skills/` and are
symlinked into `~/.pi/agent/skills/` so the AI Factory pipeline is accessible to
pi without duplicating files.

## Scope

**In scope:**
- New `AgentType::Pi` variant with wire string `"pi"`
- `PiConfig` section in `~/.nibble/config.toml`
- `~/.pi/` host directory mounted into every sandbox at `/home/node/.pi/`
- Skills symlink: `~/.pi/agent/skills` → `~/.claude/skills/` (created on host at spawn)
- Extensions mount: `~/.pi/agent/extensions/` (subdirectory of mounted `~/.pi/`)
- `--pi` flag on `sandbox attach` and `sandbox spawn`
- Install `@mariozechner/pi-coding-agent` at spawn time (non-fatal on failure)
- Resume via `pi -c` (continue last session in workspace)
- `--fresh` support: delete the most recent pi session file for this workspace,
  then start without `-c`
- `--btw` support: omit `AGENT_TASK_ID` so hooks no-op; start fresh pi instance
  without `-c` so it doesn't corrupt the main session
- Telegram inject: `pi -p <message>` piped to stdin (non-interactive print mode)
- AGENTS.md injection: unchanged — pi reads AGENTS.md natively
- Mutual-exclusion guards: `--pi` incompatible with `--hermes`, `--opencode`,
  `--kimi`, `--glm`
- Notification display: emoji + label for Pi tasks
**Out of scope:**
- Pi-specific `--session <path>` tracking (complex; deferred — simple `-c` is sufficient)
- RPC mode integration
- Per-provider selection flag (provider is selected interactively inside pi with `/models`)
- **Telegram injection for Pi** (deferred — Pi has no lifecycle hooks equivalent to
  the Claude Stop hook; see `FUTURE.md` for the full analysis and upgrade path)
- Building a dedicated `nibble-pi:latest` image (uses existing `nibble-sandbox:latest`)
- Hermes-style single-instance enforcement (pi supports multiple sessions per workspace)

**Dependencies:**
- `@mariozechner/pi-coding-agent` npm package (installed at spawn time via `npm`)
- `nibble-sandbox:latest` image (already has Node.js/npm)
- Host `~/.pi/` directory (created by nibble if absent, like `~/.hermes/`)

## Interfaces

### Public API / Exports

| Name | Input | Output | Errors |
|------|-------|--------|--------|
| `AgentType::Pi` | — | wire string `"pi"` | — |
| `PiConfig::default()` | — | `PiConfig { install_on_spawn: true }` | — |
| `cmd_sandbox_spawn(…, pi: bool, …)` | `pi=true` | spawns sandbox, installs pi, injects AGENTS.md | `anyhow::Error` if pi is true with incompatible flags |
| `cmd_sandbox_attach(…, pi: bool, …)` | `pi=true` | runs `pi -c` or `pi` in container | `anyhow::Error` |
| `ensure_pi_skills_symlink()` | host home dir | creates `~/.pi/agent/skills` → `~/.claude/skills` | logged warning on failure, non-fatal |

### Data Types

**`PiConfig`** (new, in `src/config.rs`):
```rust
pub struct PiConfig {
    /// If true, npm install @mariozechner/pi-coding-agent on every spawn.
    /// Default: true
    pub install_on_spawn: bool,
}
```
- `install_on_spawn`: bool, default `true`
- Serialized as `[pi]` section in `~/.nibble/config.toml`

**`AgentType::Pi`** (new variant):
- `as_str()` → `"pi"`
- `from_str("pi")` → `AgentType::Pi`

**`TaskContext`** (unchanged):
- No new session ID field needed for Pi (simple `-c` strategy)
- Session identity is implicit (pi uses workspace path to find last session)

### Events / Side Effects

**At spawn time (pi=true)**:
1. Mount `~/.pi/` → `/home/node/.pi/rw` (create minimal structure if absent)
2. Ensure `~/.pi/agent/skills` symlink → `~/.claude/skills/` on host
3. Install `@mariozechner/pi-coding-agent` via `npm install -g` inside container
   (non-fatal: logs warning if fails, baked npm cache may suffice)
4. AGENTS.md injection proceeds unchanged (pi reads it natively)
5. Task stored with `agent_type = "pi"`

**At attach time (pi=true)**:
- `shell_cmd` = `pi -c` (resume) or `pi` (fresh)
- `--btw`: `pi` without `-c`, `AGENT_TASK_ID` omitted (no hook side effects)
- Printed banner: `"Attaching to sandbox … [pi]…"`

**At inject time (Pi task)**:
- Runs `pi --print <message>` (or `pi -p <message>`) piped via stdin
- `AGENT_TASK_ID` set so nibble hooks fire on exit
- Blocks until pi exits

### Events / Side Effects — Skills Symlink

- `ensure_pi_skills_symlink()` called once at spawn time
- Host path: `~/.pi/agent/skills` → target: `~/.claude/skills/`
- If `~/.pi/agent/skills` already exists as a symlink: no-op
- If it exists as a directory: warn and skip (don't clobber user data)
- If it doesn't exist: `std::os::unix::fs::symlink(src, dst)`
- Errors are logged but non-fatal (spawn continues)

## State Machine / Flow

### Spawn Flow (pi=true)

```
[user: nibble sandbox spawn --pi <repo>]
        │
        ▼
[validate: --pi not combined with --hermes/--opencode/--kimi/--glm]
        │
        ▼
[check existing sandbox for repo → attach if found]
        │
        ▼
[ensure ~/.pi/ minimal structure on host]
[ensure ~/.pi/agent/skills → ~/.claude/skills/ symlink on host]
        │
        ▼
[podman run nibble-sandbox:latest with extra mounts:
   ~/.pi/ → /home/node/.pi/:rw ]
        │
        ▼
[run .nibble/setup.sh if present]
        │
        ▼
[podman exec: npm install -g @mariozechner/pi-coding-agent (non-fatal)]
        │
        ▼
[inject AGENTS.md / CLAUDE.md (unchanged)]
        │
        ▼
[store task with agent_type="pi"]
        │
        ▼
[attach → cmd_sandbox_attach(…, pi=true)]
```

### Attach Flow (pi=true)

```
[fresh=false, btw=false]  →  shell_cmd = "pi -c"
[fresh=true]              →  delete most-recent pi session file for workspace
                             shell_cmd = "pi"
[btw=true]                →  shell_cmd = "pi"
                             AGENT_TASK_ID omitted from podman exec env
```

## Invariants

1. **(INV-1) Pi is never combined with incompatible flags.**
   `--pi` with `--hermes`, `--opencode`, `--kimi`, or `--glm` → immediate `bail!`.

2. **(INV-2) `~/.pi/` always mounted.**
   If `~/.pi/` does not exist on the host, nibble creates the minimal directory
   structure before spawning.  The mount is always added to the podman args when
   `pi=true`.

3. **(INV-3) Skills symlink is never a directory when created by nibble.**
   `ensure_pi_skills_symlink()` only creates a symlink; if the path already exists
   as a non-symlink directory it logs a warning and skips — it never overwrites.

4. **(INV-4) `--btw` pi sessions do not pollute the main session.**
   `AGENT_TASK_ID` is omitted from `podman exec` args when `btw=true`, regardless
   of agent type.  Pi is started without `-c` so it opens a fresh context
   that does not become the "last session" clobbering the main task's resume point.

5. **(INV-5) `AgentType::Pi` round-trips through `as_str` / `from_str` losslessly.**
   `from_str("pi") == AgentType::Pi` and `AgentType::Pi.as_str() == "pi"`.

## Error Handling Strategy

| Error | Severity | Handling |
|-------|----------|----------|
| Pi not in `PATH` after npm install | Warning | Log to stderr; user may still use pi if baked in image |
| npm install fails | Warning | Log, continue spawn — container still usable for other agents |
| `~/.pi/` creation fails | Fatal | `bail!` — cannot mount non-existent dir |
| Skills symlink creation fails | Warning | Log, continue spawn |
| `--pi` + incompatible flag | Fatal | `bail!` with clear message |
| Pi session file deletion fails (--fresh) | Warning | Log, continue without `-c` (clean start anyway) |

## Acceptance Criteria

1. **(AC-1)** `AgentType::from_str("pi") == AgentType::Pi` and `AgentType::Pi.as_str() == "pi"`.

2. **(AC-2)** `AgentType::Pi` round-trips through JSON serialization/deserialization.

3. **(AC-3)** `PiConfig::default()` has `install_on_spawn = true`.

4. **(AC-4)** Parsing a `config.toml` with `[pi] install_on_spawn = false` yields `PiConfig { install_on_spawn: false }`.

5. **(AC-5)** Parsing a `config.toml` without a `[pi]` section yields `PiConfig::default()`.

6. **(AC-6)** `cmd_sandbox_attach` with `pi=true, hermes=true` returns an error containing `"--pi and --hermes are mutually exclusive"`.

7. **(AC-7)** `cmd_sandbox_attach` with `pi=true, opencode=true` returns an error.

8. **(AC-8)** `cmd_sandbox_attach` with `pi=true, kimi=true` returns an error.

9. **(AC-9)** `cmd_sandbox_attach` with `pi=true, glm=true` returns an error.

10. **(AC-10)** When `fresh=false, btw=false, pi=true`, the shell command contains `pi -c`.

11. **(AC-11)** When `fresh=true, pi=true`, the shell command is `pi` (no `-c`).

12. **(AC-12)** When `btw=true, pi=true`, the shell command is `pi` and `AGENT_TASK_ID` is absent from the exec env.

13. **(AC-13)** `ensure_pi_skills_symlink` creates a symlink at `~/.pi/agent/skills` pointing to `~/.claude/skills/` when neither path exists.

14. **(AC-14)** `ensure_pi_skills_symlink` is a no-op (no error) when the symlink already exists.

15. **(AC-15)** `ensure_pi_skills_symlink` logs a warning and does not overwrite when `~/.pi/agent/skills` is an existing non-symlink directory.

16. **(AC-16)** Pi tasks are displayed with emoji `"🥧"` and label `"Pi"` in Telegram notifications.

17. **(AC-17)** Spawning with `pi=true` adds `~/.pi/ → /home/node/.pi/:rw` to the podman run args (verified via `get_spawn_args` unit test).

## Constraints

- **Pi executable path**: after `npm install -g`, pi is at `/usr/local/bin/pi` inside the container (standard npm global install path for the `nibble-sandbox:latest` image). The inject command should use the full path or rely on PATH.
- **`--print` flag**: pi's non-interactive flag is `-p` / `--print`. Verify this is stable across versions; if it changes, update inject.
- **Session location**: pi stores sessions at `~/.pi/agent/sessions/<workspace-hash>/`. `--fresh` must scan and delete the most-recent file from the workspace-keyed subdirectory. Since the hash is computed by pi internally and not documented as stable, `--fresh` implementation uses a best-effort glob: find the newest `.jsonl` in `~/.pi/agent/sessions/*/` sorted by mtime.
- **No `--btw` tracking**: `--btw` for Pi does not track the throwaway session path. This means repeated `--btw` sessions in the same workspace will each find the previous `--btw` session as "most recent" via `-c` on the next real attach. Mitigation: btw never uses `-c`, so the main session is never advanced. **The main session's "last" file is whatever existed before the btw session started** — pi's `-c` finds the last session by mtime, and since btw doesn't use `-c`, it doesn't create a new "primary" history file. However, if the user does `pi -c` inside a btw container manually, the btw session becomes the newest. This is a known limitation, documented in help text.
- **Compatibility**: uses `nibble-sandbox:latest` — no new image build required.
- **Security**: No new credentials forwarded. Pi reads its own auth from `~/.pi/agent/auth.json` (mounted via `~/.pi/`).

## Open Questions

None. All ambiguities resolved in the design conversation with the user:
- Provider selection: handled inside pi (no CLI flag needed)
- Session resume: simple `-c` strategy (not path-tracked)
- Image: reuse `nibble-sandbox:latest`
- Skills: symlink approach
- Extensions: mounted via `~/.pi/` (already covers `~/.pi/agent/extensions/`)
- `--btw`: use `pi` without `-c`, omit `AGENT_TASK_ID`
