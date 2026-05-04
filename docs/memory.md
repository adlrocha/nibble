# nibble Memory System

Persistent, cross-agent memory that survives session ends, project switches, and agent garbage collection.

## Philosophy

The memory system is designed to be a **complete, standalone archive** of everything your agents do. If Claude Code deletes its `.jsonl` files, or Pi clears old sessions, the data lives on in the memory repo — versioned in git, searchable by keyword, and readable by any agent.

## Quick Reference

```bash
# At the start of every session — load relevant context
nibble memory context "what you're working on"

# Search past memories
nibble memory search "authentication pattern"

# Write a memory
nibble memory write "Chose ripgrep over SQLite for search" \
  --type decision --tags rust,storage --title "Search strategy"

# Show a memory + its linked session transcript
nibble memory show <memory-id> --with-session

# List memories for a session
nibble memory by-session <session-id>

# Archive a session manually
nibble memory archive <task-id>

# Clean up duplicate summaries
nibble memory dedup --yes
```

## Architecture

```
~/.nibble/memory/
├── .git/                          # Versioned in git
├── memories/                      # One .md file per memory
│   └── 2026-05-04_abc123_session_summary.md
├── lessons/                       # Active/resolved lessons
│   └── 2026-05-04_def456_impl_bug.md
├── sessions/                      # Human-readable index.md
├── capture/                       # Live JSONL event streams
│   └── <project>/<task-id>.jsonl
└── archive/                       # Standalone agent session copies
    ├── claude/<task-id>.jsonl
    └── pi/<task-id>.jsonl
```

## Capture Pipeline

Sessions are captured **live**, not at the end:

| Agent | Mechanism | Events Captured |
|-------|-----------|-----------------|
| Claude Code | Hooks in `~/.claude/settings.json` | User messages, tool calls, assistant responses |
| Pi | Extension in `~/.pi/agent/extensions/nibble-memory.ts` | User input, message_end, tool_call/tool_execution_end |
| Manual | `nibble memory capture <task-id> <role> <content>` | Anything you want |

### When Summarization Runs

Summarization happens **automatically at session end**:
- **Claude Code**: `Stop` hook fires `nibble memory summarize` in the background
- **Pi**: `session_shutdown` event triggers summarization after a 500ms delay
- **Manual**: Run `nibble memory summarize <task-id>` whenever you want

### Deduplication

If summarization runs twice for the same session (e.g., you manually ran it), the second run is **skipped unless you pass `--force`**.

## Memory Types

| Type | Use When | Example |
|------|----------|---------|
| `session_summary` | Auto-generated — one per session | "Session with 10 user, 32 assistant, 332 tool calls. Started with: Hermes agent stores..." |
| `decision` | You made a non-obvious choice | "Chose Markdown over SQLite for git-diffability" |
| `pattern` | Recurring convention discovered | "This codebase uses `anyhow::Result` everywhere" |
| `user_instruction` | User explicitly said to remember | "Never push to main without tests" |
| `observation` | Non-obvious factual note | "Podman exec hangs if container is paused" |
| `bug_record` | Bug + root cause + fix | "Missing null check in auth.rs:42" |

## Navigation

### Memory → Session

Every memory that came from a session has a `session_id` in its frontmatter. From a memory, you can:

```bash
# Show the memory with navigation links
nibble memory show <memory-id>

# Also print the full session transcript inline
nibble memory show <memory-id> --with-session

# List all memories linked to that session
nibble memory by-session <session-id>

# Read the raw session transcript
nibble session read <session-id>
```

### Session → Memory

From the session list, you can see which sessions have memories:

```bash
# Sessions with memory badges: M:3 = 3 linked memories
nibble session list --today

# Show the memories for a session
nibble memory by-session <task-id>
```

## Archive: Surviving Agent Garbage Collection

Agents like Claude Code periodically delete old `.jsonl` session files. The memory system defends against this in two ways:

1. **Capture files are git-tracked**: `~/.nibble/memory/capture/` is committed to git, not gitignored
2. **Original agent files are copied to archive/**: When summarization runs, the agent's original session file (if found on disk) is copied to `archive/<agent>/<task-id>.jsonl`

Even if every agent wipes its local history, `git clone` the memory repo and you have everything.

## Context Loading

The most important command. Run at the start of every session:

```bash
nibble memory context "Implementing user authentication with JWT"
```

This searches memories and active lessons for relevance and prints a briefing. It prevents:
- Re-implementing solved problems
- Re-introducing fixed bugs
- Violating user instructions
- Missing architectural constraints

## Maintenance

### Remove Duplicate Summaries

If you summarized a session multiple times (or old summaries predate deduplication):

```bash
# Dry run
nibble memory dedup

# Actually delete
nibble memory dedup --yes
```

### Rebuild Index

If `.index.json` gets corrupted:

```bash
nibble memory reindex
```

### Git Sync

Memories are stored in a git repo for versioning and cross-device sync:

```bash
nibble memory sync    # commit + push + pull
```

## Configuration

```toml
# ~/.nibble/config.toml
[memory]
enabled = true

[memory.sync]
remote = "git@github.com:you/nibble-memory.git"
auto_sync = false
author_name = "nibble"
author_email = "nibble@local"
```

## Known Limitations

- **LLM extraction is disabled**: We use heuristic summarization (first user message = title, turn counts, tool list). LLM-based extraction is planned for when a reliable local model is available.
- **Session linkage uses task IDs**: `nibble memory by-session` takes the nibble task ID, not the agent's internal session ID. This is because memories are stored by task_id.
- **opencode support is minimal**: opencode sessions are discovered but not auto-captured. You can still archive them manually with `nibble memory archive`.

## Files

| File | Purpose |
|------|---------|
| `~/.nibble/memory/memories/*.md` | Individual memory files with YAML frontmatter |
| `~/.nibble/memory/lessons/*.md` | Lesson files with severity, status, prevention |
| `~/.nibble/memory/capture/*/*.jsonl` | Live event capture (git-tracked) |
| `~/.nibble/memory/archive/*/*.jsonl` | Standalone agent session copies |
| `~/.nibble/memory/.index.json` | Rebuildable cache for fast listing |
| `~/.nibble/memory/sessions/index.md` | Human-readable table of contents |
