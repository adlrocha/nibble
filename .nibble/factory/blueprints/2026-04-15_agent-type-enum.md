# Blueprint: AgentType Enum

## Summary

`Task::agent_type` is currently a freeform `String` with no validation, no compile-time
exhaustiveness, and string comparisons scattered throughout the codebase. Converting it to
a typed `AgentType` enum makes the supported agent list explicit, routes session-ID
storage with match arms instead of ad-hoc string comparisons, enables the compiler to
enforce exhaustive handling when new agents are added, and follows the existing pattern
set by `SandboxType` and `TaskStatus`.

## Scope

- **In scope**:
  - Define `AgentType` enum in `src/models/task.rs` with variants `ClaudeCode`, `OpenCode`, and `Unknown(String)` (open-world fallback for future agents / existing DB rows with unrecognised values)
  - `as_str()` → canonical wire strings (`"claude_code"`, `"opencode"`, `"<raw>"`)
  - `FromStr` impl with legacy/alias support
  - `Display` impl (delegates to `as_str`)
  - `serde` impl: serialize via `as_str`, deserialize via `FromStr` (same pattern as `TaskStatus`)
  - Change `Task::agent_type: String` → `Task::agent_type: AgentType`
  - Update `ReportAction::Start { agent_type: String }` in CLI — keep as `String` at the CLI boundary, parse to `AgentType` in the handler
  - Update `cmd_sandbox_spawn` to use `AgentType::ClaudeCode`
  - Update `ReportAction::SessionId` handler: replace `session_id.starts_with("ses_")` heuristic with `match task.agent_type { AgentType::OpenCode => …, _ => … }`  
    — **but keep `starts_with("ses_")` as a secondary guard** so the heuristic still works for `Unknown` types and for cases where a future agent writes a `ses_`-style ID
  - Update session ID resolution in `cmd_sandbox_attach`: replace `task.agent_type != "opencode"` comparisons with match arms
  - Update `agent_display` to match on `AgentType` variants
  - Update DB write (`as_str()`) and read (`FromStr`) — no schema change needed (values stored as TEXT are unchanged)
  - Update all construction sites and all tests
  - Update `wrappers/claude-wrapper` `"claude_code"` string — **no change needed** (it's passed as a CLI arg string, parsed by `FromStr` in the handler; wire format is unchanged)

- **Out of scope**:
  - Adding new agent variants beyond `ClaudeCode`, `OpenCode`, `Unknown`
  - DB schema migration
  - Changes to `wrappers/TEMPLATE-wrapper` or `wrappers/test-agent-wrapper`
  - Changing the CLI wire format (`"claude_code"` / `"opencode"` strings remain as-is)

- **Dependencies**: `src/models/task.rs`, `src/main.rs`, `src/db/mod.rs`, `src/agent_input.rs`

## Interfaces

### `AgentType` enum

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum AgentType {
    ClaudeCode,
    OpenCode,
    /// Catch-all for future or unrecognised agent types. Stores the raw string
    /// so round-trip serialization is lossless.
    Unknown(String),
}
```

| Method | Behaviour |
|--------|-----------|
| `as_str() -> &str` | `ClaudeCode` → `"claude_code"`, `OpenCode` → `"opencode"`, `Unknown(s)` → `s.as_str()` |
| `FromStr` | `"claude_code"` → `ClaudeCode`, `"opencode"` → `OpenCode`, anything else → `Unknown(s)` — **never errors** |
| `Display` | delegates to `as_str()` |
| `Serialize` | via `as_str()` |
| `Deserialize` | via `FromStr` (infallible) |
| `session_id_prefix() -> Option<&'static str>` | `OpenCode` → `Some("ses_")`, others → `None` — documents the known ID format per agent |

### `Task` struct change

```rust
// Before
pub agent_type: String,

// After
pub agent_type: AgentType,
```

### `ReportAction::Start` handler change

The `agent_type` CLI argument stays `String`. The handler parses it:
```rust
let agent_type = AgentType::from_str(&agent_type_str).unwrap(); // infallible
```

### Session ID routing (updated)

```rust
// ReportAction::SessionId handler
match task.agent_type {
    AgentType::OpenCode => ctx.opencode_session_id = Some(session_id),
    _ => {
        // For ClaudeCode and Unknown types, also apply the ses_ heuristic
        // so future opencode-style agents with ses_ IDs land in the right field.
        if session_id.starts_with("ses_") {
            ctx.opencode_session_id = Some(session_id);
        } else {
            ctx.claude_session_id = Some(session_id);
        }
    }
}
```

### Legacy fallback in `cmd_sandbox_attach` (updated)

```rust
let claude_session_id = task.context.as_ref().and_then(|c|
    c.claude_session_id.as_deref().or(match task.agent_type {
        AgentType::OpenCode => None,
        _ => c.session_id.as_deref(),  // legacy rows for non-opencode tasks
    })
);
let opencode_session_id = task.context.as_ref().and_then(|c|
    c.opencode_session_id.as_deref().or(match task.agent_type {
        AgentType::OpenCode => c.session_id.as_deref(),  // legacy opencode rows
        _ => None,
    })
);
```

### `agent_display` (updated)

```rust
fn agent_display(agent_type: &AgentType) -> (&'static str, String) {
    match agent_type {
        AgentType::ClaudeCode  => ("🤖", "Claude Code".to_string()),
        AgentType::OpenCode    => ("⚡", "OpenCode".to_string()),
        AgentType::Unknown(s)  => ("🔧", s.clone()),
    }
}
```

## Invariants

1. **(INV-1)** Round-trip invariant: `AgentType::from_str(x.as_str()) == x` for all variants including `Unknown`.
2. **(INV-2)** `FromStr` is infallible — any string is accepted; unknown values become `Unknown(s)`.
3. **(INV-3)** DB round-trip: a task written and read back has the same `agent_type` value.
4. **(INV-4)** Session ID routing for `OpenCode` tasks always writes `opencode_session_id`. The `ses_` heuristic remains for `Unknown` types only.
5. **(INV-5)** The wire format strings (`"claude_code"`, `"opencode"`) are unchanged — no DB migration needed.

## Error Handling

- `FromStr` never returns `Err` — unknown strings map to `Unknown(s)`. This ensures zero breakage for existing rows with unrecognised agent type strings.
- `agent_display` handles `Unknown` with a generic emoji — no panic path.

## Acceptance Criteria

1. **(AC-1)** `AgentType::from_str("claude_code") == AgentType::ClaudeCode`
2. **(AC-2)** `AgentType::from_str("opencode") == AgentType::OpenCode`
3. **(AC-3)** `AgentType::from_str("my_new_agent") == AgentType::Unknown("my_new_agent".into())`
4. **(AC-4)** `AgentType::ClaudeCode.as_str() == "claude_code"` and vice versa for all variants
5. **(AC-5)** DB round-trip: insert a task with `AgentType::OpenCode`, read it back, get `AgentType::OpenCode`
6. **(AC-6)** DB round-trip: insert a task with `AgentType::Unknown("my_bot")`, read it back, get `AgentType::Unknown("my_bot")`
7. **(AC-7)** `ReportAction::SessionId` for a `ClaudeCode` task with a UUID writes `claude_session_id`
8. **(AC-8)** `ReportAction::SessionId` for an `OpenCode` task with a `ses_...` ID writes `opencode_session_id`
9. **(AC-9)** `ReportAction::SessionId` for a `ClaudeCode` task with a `ses_...` ID (future cross-agent scenario) writes `opencode_session_id` via the `ses_` heuristic
10. **(AC-10)** `cargo test` passes with no failures

## Constraints

- No DB schema migration — `agent_type` wire strings are unchanged
- `FromStr` must be infallible (no `Err` variant)
- `Unknown(String)` must round-trip losslessly
- Follow `SandboxType` / `TaskStatus` pattern exactly for consistency
