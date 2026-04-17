# Deferred Features

Features that were considered and intentionally deferred. Each entry explains why
it was deferred and what would be needed to implement it.

---

## Pi Agent: Telegram Injection

**Context**: Pi (`AgentType::Pi`) does not have lifecycle hooks (equivalent to the
Claude Code Stop hook) that call `nibble report session-id` or send Telegram
completion notifications when a turn finishes.

**Problem**:
- `agent_input::inject_returning_child` is hardcoded to run `claude` — it would
  run the wrong binary in a Pi sandbox.
- Even if the binary were parameterised, Pi has no Stop hook to fire the completion
  notification. The safety-net fires 30 s after the process exits, but that's the
  entire UX — there's no per-turn "Claude finished" message from inside the agent.
- Completion status (did pi succeed? did it need attention?) is not surfaced.

**When to revisit**:
- If Pi adds a `--on-exit` hook or a way to run a shell command after each turn.
- Or if we build a thin wrapper script around `pi --print` that calls
  `nibble notify` on exit, mimicking the Claude Stop hook pattern.

**What's needed**:
1. Parameterise `agent_input::inject_returning_child` to accept the agent binary
   and resume args per `AgentType` (Pi: `pi --print`, Claude: `claude --resume`).
2. Add a Pi "epilogue" that runs `nibble notify` after `pi --print` exits, so
   Telegram gets a completion message (similar to the opencode epilogue pattern).
3. Route `agent_type == AgentType::Pi` to the new inject path in the Telegram
   listener (currently Pi tasks are rejected or fall through to Claude's path).

**Current behaviour**: Pi tasks can only be interacted with via
`nibble sandbox attach` (interactive TUI). Telegram messages to Pi sandboxes are
silently ignored / error.
