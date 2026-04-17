# Blueprint: Per-Agent Session ID Storage

## Summary

`TaskContext.session_id` is a single field shared by all agent types. When opencode runs
last in a sandbox it stores a `ses_...` ID there; the next `nibble sandbox attach` (without
`--opencode`) reads that same field and passes `--resume ses_...` to Claude, which silently
fails or opens the wrong session. The fix is to split the single `session_id` field into two
agent-scoped fields — `claude_session_id` and `opencode_session_id` — so each agent only ever
reads its own stored ID.

## Scope

- **In scope**:
  - Add `claude_session_id: Option<String>` and `opencode_session_id: Option<String>` to `TaskContext`
  - Deprecate (keep for read-back, but stop writing) `session_id` in `TaskContext`
  - Update `nibble report session-id` handler to write the correct typed field based on the task's `agent_type`
  - Update `cmd_sandbox_attach` to read from the correct typed field
  - Update the opencode post-exit epilogue (inside the shell command string) to call `nibble report session-id` as before (handler routing is transparent to the shell script)
  - The Claude Stop hook calls `nibble report session-id` as before — no change to hook script
  - Migrate existing rows: on read, if typed field is absent but legacy `session_id` is present and agent type matches, treat it as the typed value (no DB migration needed)
- **Out of scope**:
  - DB schema migration (store in existing `context` JSON blob — no new columns)
  - Changes to `wrappers/claude-wrapper` or `scripts/setup-claude-hooks.sh`
  - Changes to the opencode epilogue shell string (logic stays the same)
  - Any other agent types beyond `claude_code` and `opencode`

- **Dependencies**: `src/models/task.rs`, `src/main.rs` (handler + attach), `src/db/mod.rs` (no schema change needed)

## Interfaces

### Public API / Exports

| Name | Input | Output | Errors |
|------|-------|--------|--------|
| `ReportAction::SessionId` handler | `task_id: String`, `session_id: String` | writes DB | `TaskNotFound` |
| `cmd_sandbox_attach` (claude path) | `task: &Task`, `opencode: bool` | reads `claude_session_id` | — |
| `cmd_sandbox_attach` (opencode path) | `task: &Task`, `opencode: bool` | reads `opencode_session_id` | — |

### Data Types

**`TaskContext`** — extended:
```rust
pub struct TaskContext {
    pub url: Option<String>,
    pub project_path: Option<String>,
    /// DEPRECATED — kept for reading legacy rows only; new code writes typed fields below.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Session ID for Claude Code (`--session-id` UUID format).
    pub claude_session_id: Option<String>,
    /// Session ID for opencode (`ses_...` format).
    pub opencode_session_id: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}
```

`session_id` is preserved with `skip_serializing_if = "Option::is_none"` so:
- Legacy rows that have it will deserialize it fine.
- New writes never emit it (no new rows will have it).

### Events / Side Effects

- `nibble report session-id <task_id> <sid>`:
  - Loads task from DB
  - Determines agent type from `task.agent_type`
  - Sets `ctx.claude_session_id` if `agent_type == "claude_code"`, else sets `ctx.opencode_session_id`
  - Writes updated task back to DB via `db.update_task`

## Invariants

1. **(INV-1)** A Claude attach (`--opencode` not set) MUST only pass `--resume`/`--session-id` with a value sourced from `claude_session_id`. It MUST NOT use `opencode_session_id` or the legacy `session_id`.
2. **(INV-2)** An opencode attach (`--opencode` set) MUST only pass `--session` with a value sourced from `opencode_session_id`. It MUST NOT use `claude_session_id` or the legacy `session_id`.
3. **(INV-3)** The `nibble report session-id` handler MUST route to the correct typed field based on `task.agent_type`; it MUST NOT write to the legacy `session_id` field.
4. **(INV-4)** Legacy rows (existing `session_id`, no typed field) MUST still resume correctly: when the typed field is absent, fall back to legacy `session_id` if and only if the agent type matches (claude reads legacy for claude tasks; opencode reads legacy for opencode tasks).

## Error Handling Strategy

- `TaskNotFound` in the session-id handler: propagate as `anyhow::Error` (non-zero exit, hook stderr suppressed with `2>/dev/null`).
- Unknown `agent_type` in the handler: default to writing `claude_session_id` (safe; opencode IDs start with `ses_` and are visually distinct).
- Legacy fallback in attach: if typed field is `None`, check legacy `session_id`; if also `None`, start fresh (existing behaviour).

## Acceptance Criteria

1. **(AC-1)** After an opencode session ends, `cmd_sandbox_attach` (Claude, no `--opencode`) starts Claude fresh (no `--resume` flag), not with the opencode `ses_...` ID.
2. **(AC-2)** After a Claude session ends, `cmd_sandbox_attach --opencode` starts opencode fresh, not with the Claude UUID.
3. **(AC-3)** After a Claude session, `cmd_sandbox_attach` (Claude) resumes with `--resume <claude_session_id>`.
4. **(AC-4)** After an opencode session, `cmd_sandbox_attach --opencode` resumes with `--session <opencode_session_id>`.
5. **(AC-5)** `nibble report session-id` called from a task with `agent_type = "claude_code"` writes `claude_session_id` and leaves `opencode_session_id` untouched.
6. **(AC-6)** `nibble report session-id` called from a task with `agent_type = "opencode"` writes `opencode_session_id` and leaves `claude_session_id` untouched.
7. **(AC-7)** A legacy row (only `session_id` set, no typed fields) is read back correctly: Claude attach resumes with the legacy value; opencode attach does NOT use it (starts fresh).
8. **(AC-8)** `cargo test` passes with no new failures.

## Constraints

- No DB schema migration — all new fields live inside the existing `context` JSON blob.
- Backward-compatible JSON serialization: new fields are `Option<String>` and absent when `None` (using `skip_serializing_if`).
- No changes to CLI surface — `nibble report session-id` signature is unchanged.

## Open Questions

None — requirements are fully understood from codebase exploration.
