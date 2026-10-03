# Blueprint: Universal session tracking (host + sandbox, wrapper or not)

Date: 2026-10-03 · Tier: Standard (Retroactive mode: includes audit of the
in-flight window-task refactor found uncommitted in the tree)

## Problem

Nibble tracked sessions only when the launching process exported
`AGENT_TASK_ID` — i.e. launches through the wrappers (host interactive
shells) or sandbox attach windows. Sessions launched any other way
(`claude -p` / `omp -p` in scripts, non-interactive shells, `nibble sandbox
bash` shells, direct binary calls) produced no task row, no memory capture,
and no live status, even though their transcripts landed in the mounted
`~/.claude` / `~/.omp` / `~/.pi` dirs.

## Decision

No new wrappers (they cannot see launches that bypass them). Instead the
reporting layer derives a stable task ID from the agent's own session
identity when `AGENT_TASK_ID` is absent:

- Claude hooks: `TID="${AGENT_TASK_ID:-claude-<session_id>}"` — every hook
  payload carries `session_id`; added a `SessionStart` hook so the row
  exists at launch, not first prompt.
- pi/omp extension: derive `<agent>-<session_id>` from the session file
  name (`<timestamp>_<id>.jsonl[.gz]`) once known; `session-path` mapping
  then reports under the derived ID.
- Row creation rides the existing self-heal
  (`db::ensure_task_or_create`: agent type from `NIBBLE_AGENT_TYPE`,
  title/cwd from the reporting process, no pid — container-namespace safe).
  No Rust changes required for the mechanism.

## Invariants

- INV-A: `AGENT_TASK_ID`, when set, always wins; derived IDs are a fallback
  only (proved: hook simulation created zero derived rows with it set).
- INV-B: derived IDs are stable per underlying session id (claude session
  UUID / pi-family file id) — repeated events upsert the same row.
- INV-C: `report status <id> exited` on an unknown ID never creates a row
  (pre-existing guard, unchanged).
- INV-D: wrapped/window flows are byte-identical to before when
  `AGENT_TASK_ID` is set.

## Files

- `scripts/setup-claude-hooks.sh` — derived-TID in all six hooks + new
  SessionStart hook.
- `pi-extensions/nibble-memory.ts` — `noteSessionFile`/derived task ID;
  `agent_start` notes the session file before reporting status.
- `src/sandbox/hermes.rs` — audit fix: restore `-e` before
  `AGENT_TASK_ID=` (window-task refactor had dropped the flag; podman exec
  treated the assignment as the command and hermes attach was broken).

## Verification

- `cargo build --release` + `cargo test --release`: 285 passed, 0 failed
  (includes prior refactor's window-task tests).
- Real headless `claude -p` without `AGENT_TASK_ID`: row
  `claude-268031eb…` (agent=claude_code, title/cwd correct,
  `claude_session_id` mapped, retired `exited`).
- Real headless `omp -p` without `AGENT_TASK_ID`: row `omp-01a1031c…`
  (agent=omp, `pi_session_path` mapped — serde-flattened into top-level
  context JSON, retired `exited`).
- `nibble session list` shows both e2e sessions with workspace names and
  task links (both omp slug schemes decode).
- Installed hook executed with `AGENT_TASK_ID` sentinel → sentinel used,
  no derived row.
- In-sandbox: `~/.omp`/`~/.nibble` mounted, nibble reachable at
  `/usr/local/bin/nibble` in a live container; stale `nibble-musl`
  (pre-self-heal) rebuilt + reinstalled so new sandboxes self-heal.
- Host binary + hooks + extension (×3 dirs) reinstalled.

## Known limitations

- Long-lived watchers (`status --watch`, web service) occasionally hold the
  WAL lock past the 5 s busy timeout; first hook call can be lost.
  Eventual consistency: the next hook event re-runs `report status`, which
  self-heals the row. (Observation, pre-existing.)
- Claude hook capture selectors (`.message` on UserPromptSubmit,
  `.tool_output` on PostToolUse) may not match current Claude Code payload
  keys (`.prompt`, `.tool_response`); status reporting is unaffected.
  Left unchanged pending ground truth.

## Addendum: consolidated session corpus (~/.nibble/sessions)

A parallel migration moved all transcripts into
`~/.nibble/sessions/{omp,pi,claude/projects}` with symlinks back at the
agent paths. Audit of that move and the final design:

- Absolute symlinks dangled inside sandboxes (container home is
  `/home/node`, and `~/.pi/agent` itself is a dotfiles symlink at a
  different depth) → omp/pi sessions were unreadable in new containers.
- `nibble backup --sessions` walked with `follow_links(false)`, so a
  symlinked source archived as an empty dir — silent total backup loss.
- Final layout: corpus stays in `~/.nibble/sessions` (one place); agent
  paths keep **relative** symlinks (omp/claude resolve identically in
  host and container); `mount_agent_config_dir` overlays a symlinked
  sessions dir into the container (same pattern as the agent-dir
  overlay), making pi's dotfiles-depth link work in sandboxes too.
- `nibble backup` now canonicalizes symlinked sources and **always**
  includes the corpus (~/.nibble/sessions lives inside the backed-up
  tree); `--sessions` skips agent paths that resolve into the corpus so
  nothing is archived twice.
- Verified: fresh sandbox read+write through the overlay lands in the
  host corpus; unwrapped `omp -p`/`claude -p` tracked end-to-end on the
  new layout; real backup inspected entry-by-entry.
