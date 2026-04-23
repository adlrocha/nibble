# Blueprint: nibble Memory Management System

## Summary

A local-first, agent-agnostic memory system that captures session activity from all
coding agents (Claude Code, Pi, opencode), stores structured memories as **Markdown
files** in a git-tracked directory, and surfaces relevant knowledge to agents via a
CLI + skill interface. The system replaces the ad-hoc `factory-lessons` Markdown file
with a proper lessons engine that auto-resolves and semantically matches to the current
task context.

Four design principles distinguish this from the previous blueprint:

1. **Markdown files, not a database.** Every memory and lesson is a `.md` file with
   YAML frontmatter. The file system is the database. `ripgrep` provides keyword
   search, a lightweight embeddings cache provides semantic search, and an index
   cache provides fast stats. All caches are rebuildable from the Markdown files.
   Git provides versioning, backup, and cross-device sync.

2. **Event-driven capture, not polling.** Each agent has its own hook/extension
   mechanism. We use those mechanisms directly instead of scanning transcript files
   on a timer. This is simpler, more reliable, and captures data immediately.

3. **Agent-specific adapters, unified storage.** Claude Code uses shell hooks.
   Pi uses TypeScript extensions. opencode uses a post-exit epilogue. Each adapter
   emits the same JSONL event format into a shared capture directory. A single
   ingestion path reads those events.

4. **Skill-first access with agent autonomy.** Agents interact with memory through
   a skill that teaches them to use `nibble memory` CLI commands and to read
   materialized files. Agents decide what's worth remembering using judgment
   criteria, not rigid rules. The skill works identically regardless of which
   agent loads it.

---

## Why Markdown Files Instead of a Database

The previous blueprint used SQLite (FTS5 + sqlite-vec) as the canonical store. After
analysis, Markdown files are a better fit for nibble's scale and use case:

| Dimension | SQLite | Markdown files |
|---|---|---|
| **Scale** | Needed for 100K+ rows | We expect 100s-1000s of memories — file system handles this trivially |
| **Search** | FTS5 (built-in) | `ripgrep` (already available, faster at our scale, no index maintenance) |
| **Semantic search** | sqlite-vec extension | Embeddings cache file + Rust cosine similarity |
| **Human readability** | Requires a query tool | `cat` / `less` / any editor |
| **Git diff** | Binary blob, no diff | Full text diff, meaningful history |
| **Backup/sync** | Custom export + import | `git push` / `git pull` |
| **Concurrent writes** | WAL mode, lock contention | One file per memory = zero contention |
| **Schema migrations** | Version tracking, ALTER TABLE | Change frontmatter format, reindex |
| **Corruption** | WAL snapshot issues | File system is robust; git adds recovery |
| **Portability** | Requires SQLite tools | Any text editor, any OS |

**Rebuildable caches** replace the DB for performance-critical operations:

- `.index.json` — maps memory_id → file path, type, tags, project, date. Rebuilt by
  scanning all `.md` files. Used for `list`, `stats`, fast lookups. < 100ms to rebuild
  for 10K entries.
- `.embeddings.json` — maps memory_id → embedding vector. Rebuilt by calling the LLM
  embedding endpoint for each memory. Used for semantic search. Expensive to rebuild
  but cacheable.

Both caches are gitignored. They can be deleted and regenerated with `nibble memory reindex`.

---

## Feasibility: Agent Compatibility Matrix

### Claude Code

| Capability | Mechanism | Status |
|---|---|---|
| Session start/end | `Stop` hook (installed in `~/.claude/settings.json`) | ✅ Already used |
| Capture per-turn | `UserPromptSubmit` hook + `PostToolUse` hook | ✅ Available, not yet used |
| Session transcript | `~/.claude/projects/<hash>/<uuid>.jsonl` | ✅ Readable |
| Notification | `Notification` hook (permission_prompt) | ✅ Already used |
| Skills | `~/.claude/skills/<name>/SKILL.md` | ✅ Already used by factory |

**Capture approach**: Extend `setup-claude-hooks.sh` to add `PostToolUse` and a
modified `Stop` hook that append structured events to the capture JSONL. Also add
a `UserPromptSubmit` hook to capture user messages. All hooks already receive JSON
on stdin with full context.

### Pi (pi-coding-agent)

| Capability | Mechanism | Status |
|---|---|---|
| Session start/end | `session_start` / `session_shutdown` events | ✅ Extension API |
| Capture per-turn | `turn_end` / `tool_call` / `tool_result` events | ✅ Extension API |
| Session transcript | `~/.pi/agent/sessions/<path-hash>/<ts>_<uuid>.jsonl` | ✅ Readable JSONL |
| Skills | `~/.pi/agent/skills/<name>/SKILL.md` (symlinked from `~/.claude/skills/`) | ✅ Already configured |
| Custom tools | `pi.registerTool()` | ✅ Can register `memory_search` tool |

**Capture approach**: Install a Pi extension at `~/.pi/agent/extensions/nibble-memory.ts`
that listens to `turn_end` and `session_shutdown` events and appends structured events
to the capture JSONL. The extension can also register a `memory_search` tool so Pi's
LLM can query memory directly without going through bash.

### opencode

| Capability | Mechanism | Status |
|---|---|---|
| Session start/end | Post-exit epilogue (already in attach command) | ✅ Already used |
| Capture per-turn | No hook system; session stored in SQLite | ⚠️ Post-hoc only |
| Session transcript | `~/.local/share/opencode/opencode.db` (SQLite) | ✅ Queryable |
| Skills | No native skill system | ❌ Uses CLAUDE.md/AGENTS.md |

**Capture approach**: opencode has no hook API. We rely on its SQLite session store.
A lazy ingestion scan discovers new sessions. For immediate writes, opencode agents
use `nibble memory write` via bash (same as any other agent).

### Conclusion

All three agents are fully compatible. Claude Code and Pi support real-time
event-driven capture. opencode requires post-hoc ingestion but can still write
memories explicitly. The skill-based read interface works for all agents since
they all have bash access inside sandboxes.

---

## Scope

### In scope

- **Memory capture**: Agent-specific adapters that emit structured JSONL events.
  Claude Code hooks, Pi extension, opencode post-hoc scan.
- **Storage**: Markdown files with YAML frontmatter in `~/.nibble/memory/`,
  git-tracked for versioning, backup, and cross-device sync. No database.
  A rebuildable `.index.json` cache for fast listing/stats, and a
  rebuildable `.embeddings.json` cache for semantic search.
- **Memory types**: session_summary, decision, pattern, user_instruction,
  observation, bug_record.
- **Lessons engine**: Separate from general memory. Lessons have a lifecycle
  (active → resolved → encoded). Auto-resolved when the relevant skill file
  is updated or the underlying issue is fixed. Semantically matched to current
  task context at session start.
- **LLM extraction**: On session end, a local LLM (default: localhost:6969)
  processes the captured events and extracts structured memories. Falls back
  to heuristic extraction if LLM is unavailable.
- **Materialized summaries**: Human-readable Markdown files in
  `~/.nibble/memory/sessions/<project>/` that agents can `cat` directly.
  An `index.md` provides a table of contents with links.
- **CLI**: `nibble memory search/write/list/stats/forget/summarize/lessons/inspect/sync`
- **Skill**: `~/.claude/skills/nibble-memory/SKILL.md` — teaches agents the
  memory protocol with judgment criteria (not rigid rules).
- **Global memory with project tagging**: All memories are searchable by all
  sessions. Memories are tagged with `source_repo` for context but never
  siloed by project.
- **Git-based backup and sync**: `~/.nibble/memory/` is a git repo.
  `nibble memory sync` commits, pushes, and pulls from a configurable remote.
  Cross-device sync through a private repo.
- **Agent autonomy**: Agents decide what to remember, when to promote or demote
  memories, and when to update existing knowledge. The skill provides judgment
  criteria, not checklists.

### Out of scope (v1)

- Cloud storage backend (git repo is sufficient).
- MCP server or HTTP API.
- TUI beyond basic `nibble memory inspect` (pager-based).
- Multi-provider LLM chain (single configurable provider in v1).
- Semantic deduplication beyond embedding similarity threshold.

---

## Architecture

### Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│                        HOST SYSTEM                                  │
│                                                                     │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐              │
│  │ Claude Code  │  │     Pi       │  │   opencode    │              │
│  │              │  │              │  │              │              │
│  │ Stop hook    │  │ extension    │  │ (post-hoc    │              │
│  │ PostToolUse  │  │ turn_end     │  │  scan)       │              │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘              │
│         │                 │                  │                       │
│         ▼                 ▼                  ▼                       │
│  ┌──────────────────────────────────────────────────────┐           │
│  │  ~/.nibble/memory/capture/<project>/<session>.jsonl  │           │
│  │  {"ts":...,"role":"user","content":"fix auth"}       │           │
│  │  {"ts":...,"role":"assistant","content":"Found..."}  │           │
│  │  {"ts":...,"role":"tool","name":"bash","result":...} │           │
│  └──────────────────────────┬───────────────────────────┘           │
│                             │                                       │
│            session end trigger                                       │
│                             │                                       │
│                             ▼                                       │
│  ┌──────────────────────────────────────────────────────┐           │
│  │  nibble memory summarize <session>                   │           │
│  │                                                      │           │
│  │  1. Read capture JSONL                               │           │
│  │  2. Call local LLM → extract memories + lessons      │           │
│  │  3. Compute embeddings (local LLM)                   │           │
│  │  4. Write Markdown files to memories/ and lessons/   │           │
│  │  5. Write summary.md to sessions/<project>/          │           │
│  │  6. Update .index.json and .embeddings.json caches   │           │
│  │  7. git add + git commit                             │           │
│  └──────────────────────────────────────────────────────┘           │
│                             │                                       │
│                             ▼                                       │
│  ┌──────────────────────────────────────────────────────┐           │
│  │  ~/.nibble/memory/         ← git repo               │           │
│  │                                                      │           │
│  │  memories/    — one .md file per memory (tracked)    │           │
│  │  lessons/     — one .md file per lesson (tracked)    │           │
│  │  sessions/    — summaries per project (tracked)      │           │
│  │  index.md     — table of contents (tracked)          │           │
│  │  capture/     — raw JSONL events (gitignored)        │           │
│  │  .index.json  — listing cache (gitignored)           │           │
│  │  .embeddings.json — vector cache (gitignored)        │           │
│  └──────────────────────────────────────────────────────┘           │
│                             │                                       │
│         ┌───────────────────┼───────────────────┐                   │
│         ▼                   ▼                   ▼                   │
│  ┌────────────┐   ┌────────────────┐   ┌──────────────┐            │
│  │ CLI query  │   │ Skill + inject │   │ Direct file  │            │
│  │ search/    │   │ relevant mems  │   │ access:      │            │
│  │ list/stats │   │ at session     │   │ cat, grep,   │            │
│  └────────────┘   │ start          │   │ less, editor │            │
│                   └────────────────┘   └──────────────┘            │
│                                                                     │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │  Sandbox (bind-mounted ~/.nibble/)                            │   │
│  │                                                              │   │
│  │  Agent reads skill → nibble memory search/write              │   │
│  │  Agent reads files → cat ~/.nibble/memory/sessions/...       │   │
│  └──────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
```

### Event Capture Format (unified JSONL)

All agents write to the same format. One JSONL file per session:

```
~/.nibble/memory/capture/<project>/<session-id>.jsonl
```

Each line is a JSON object:

```json
{"ts":"2026-04-21T10:30:00Z","role":"user","content":"Fix the auth bug in src/auth.rs"}
{"ts":"2026-04-21T10:30:05Z","role":"assistant","content":"I'll examine the auth module..."}
{"ts":"2026-04-21T10:30:10Z","role":"tool","name":"bash","input":"cat src/auth.rs","output":"...","duration_ms":120}
{"ts":"2026-04-21T10:32:00Z","role":"assistant","content":"The bug is a missing null check..."}
```

Fields:
- `ts`: ISO 8601 timestamp
- `role`: "user" | "assistant" | "tool" | "system"
- `content`: text content (for user/assistant/system)
- `name`: tool name (for tool role)
- `input`: tool input (for tool role)
- `output`: tool output, truncated to 4096 chars (for tool role)
- `duration_ms`: tool execution time (for tool role)

### Memory File Format

Each memory is stored as a Markdown file with YAML frontmatter:

**Path**: `~/.nibble/memory/memories/<YYYY-MM-DD>_<short-id>_<type>.md`

Example: `memories/2026-04-21_a1b2c3d4_decision.md`

```markdown
---
memory_id: a1b2c3d4-5678-90ab-cdef-1234567890ab
type: decision
agent: claude_code
session_id: abc123
task_id: def456
project: nibble
tags: [rust, database, sqlite, search]
confidence: 0.85
created_at: 2026-04-21T10:30:00Z
updated_at: 2026-04-21T10:30:00Z
access_count: 3
---

Chose ripgrep-based keyword search over SQLite FTS5 for the memory system.

The memory store is expected to contain hundreds to low thousands of entries.
At this scale, ripgrep is faster than FTS5, requires no index maintenance,
and produces results that are instantly readable by humans and agents.

FTS5 or a vector database can be introduced in v2 if scale requires it.
```

### Lesson File Format

**Path**: `~/.nibble/memory/lessons/<YYYY-MM-DD>_<short-id>_<category>.md`

Example: `lessons/2026-04-21_b2c3d4e5_impl_bug.md`

```markdown
---
lesson_id: b2c3d4e5-6789-0abc-def0-1234567890ab
category: impl_bug
severity: high
status: active
project: nibble
tags: [rust, sqlite, concurrency]
source_session: abc123
occurrence_count: 2
created_at: 2026-04-21T10:30:00Z
updated_at: 2026-04-22T14:00:00Z
---

SQLite WAL mode busy_timeout must be ≥ 5000ms when multiple sandboxes
write to the same database concurrently.

## Prevention

Always set `PRAGMA busy_timeout=5000` when opening any SQLite connection
that may face concurrent writers. Test with two simultaneous `nibble memory write`
calls from different sandboxes.
```

### Session Summary Format

**Path**: `~/.nibble/memory/sessions/<project>/<YYYY-MM-DD>_<session>.summary.md`

```markdown
# Session: Fix auth bug in src/auth.rs

**Date**: 2026-04-21
**Agent**: Claude Code
**Project**: nibble
**Duration**: ~12 min

## Summary

Fixed a null check bug in the auth middleware where expired tokens caused a panic
instead of returning 401. Added regression test.

## Decisions
- Chose to return 401 immediately rather than attempting token refresh (refresh is
  not supported in this auth flow)

## Patterns Observed
- Auth middleware follows a chain-of-responsibility pattern

## Bugs Fixed
- `src/auth.rs:42` — missing null check on `token.expiry` caused panic on expired tokens

## Lessons
- Always check Option/Result before accessing fields in Rust auth code
```

### index.md (Table of Contents)

Regenerated after each write or summarization. Agents can `cat` this for an overview.

```markdown
# Memory Index

*Last updated: 2026-04-21T15:00:00Z · 47 memories, 5 active lessons*

## Recent Sessions

| Date | Project | Agent | Summary |
|------|---------|-------|---------|
| [2026-04-21 · nibble](sessions/nibble/2026-04-21_abc123.summary.md) | nibble | Claude Code | Fixed auth bug... |
| [2026-04-20 · my-app](sessions/my-app/2026-04-20_def456.summary.md) | my-app | Pi | Implemented user model... |

## Active Lessons (5)

1. **[high]** [Rust: derive macros don't work with `#[serde(default)]` on enum variants](lessons/2026-04-15_x1y2z3_impl_bug.md)
2. **[medium]** [SQLite WAL mode: always set busy_timeout ≥ 5000ms](lessons/2026-04-21_b2c3d4e5_impl_bug.md)
3. **[medium]** [Podman: `podman exec` hangs if container is paused](lessons/2026-04-18_c3d4e5f6_impl_bug.md)
4. **[low]** [Claude Code hooks: jq required for message extraction](lessons/2026-04-10_d4e5f6a7_process.md)
5. **[critical]** [Never run `rm -rf` in workspace root from agent hooks](lessons/2026-04-12_e5f6a7b8_qa_catch.md)

## Key Decisions (10)

- [Chose ripgrep over SQLite FTS5 for memory search](memories/2026-04-21_a1b2c3d4_decision.md) (2026-04-21)
- [Markdown files as canonical store for git-diffability](memories/2026-04-21_f7g8h9i0_decision.md) (2026-04-21)
- ...
```

Agents can also search across all memory files directly:
```bash
grep -r "auth" ~/.nibble/memory/memories/
grep -r "auth" ~/.nibble/memory/sessions/
```

### Cache Files

**`.index.json`** — rebuildable cache for fast lookups:
```json
{
  "version": 1,
  "generated_at": "2026-04-21T15:00:00Z",
  "memories": {
    "a1b2c3d4-5678-90ab-cdef-1234567890ab": {
      "path": "memories/2026-04-21_a1b2c3d4_decision.md",
      "type": "decision",
      "project": "nibble",
      "tags": ["rust", "database"],
      "created_at": "2026-04-21T10:30:00Z",
      "confidence": 0.85
    }
  },
  "lessons": {
    "b2c3d4e5-6789-0abc-def0-1234567890ab": {
      "path": "lessons/2026-04-21_b2c3d4e5_impl_bug.md",
      "category": "impl_bug",
      "severity": "high",
      "status": "active",
      "occurrence_count": 2
    }
  },
  "stats": {
    "total_memories": 47,
    "total_lessons": 8,
    "active_lessons": 5,
    "by_type": {"decision": 12, "observation": 15, "pattern": 8, ...},
    "oldest": "2026-04-10T08:00:00Z",
    "newest": "2026-04-21T15:00:00Z"
  }
}
```

**`.embeddings.json`** — rebuildable vector cache:
```json
{
  "version": 1,
  "model": "default",
  "dimensions": 768,
  "generated_at": "2026-04-21T15:00:00Z",
  "vectors": {
    "a1b2c3d4-5678-90ab-cdef-1234567890ab": [0.023, -0.015, ...],
    "b2c3d4e5-6789-0abc-def0-1234567890ab": [0.041, 0.008, ...]
  }
}
```

Both caches are regenerated by `nibble memory reindex`. The embeddings cache is
expensive to rebuild (one LLM call per memory) but cheap to read.

### Git Sync

`~/.nibble/memory/` is a git repository. This provides:

- **Version history**: every memory write is a commit. Accidental deletions are
  recoverable with `git log` + `git checkout`.
- **Cross-device sync**: push to a private remote (GitHub, GitLab, self-hosted).
  Pull on another machine.
- **Human review**: `git diff` shows exactly what changed.
- **Collaboration** (future): share a memory repo with teammates.

**`.gitignore`** (in `~/.nibble/memory/`):
```
capture/
.index.json
.embeddings.json
*.tmp
```

**`nibble memory sync`** behavior:
1. `git add -A`
2. `git commit -m "memory: <summary of changes>"` (or skip if nothing changed)
3. `git pull --rebase` (resolve conflicts by keeping both — Markdown files don't
   conflict if they have different names)
4. `git push`

**Config:**
```toml
[memory.sync]
# Git remote for memory repo. Set to enable push/pull sync.
# Example: "git@github.com:user/nibble-memory.git"
remote = ""
# Auto-sync after summarization (default: false — opt-in)
auto_sync = false
# Commit author
author_name = "nibble"
author_email = "nibble@local"
```

### Claude Code Adapter

Extend `setup-claude-hooks.sh` to add three hooks:

**UserPromptSubmit** — captures user messages:
```bash
if [ -n "$AGENT_TASK_ID" ]; then
  INPUT=$(cat)
  MSG=$(printf "%s" "$INPUT" | jq -r ".message // empty")
  [ -n "$MSG" ] && nibble memory capture "$AGENT_TASK_ID" "user" "$MSG" 2>/dev/null || true
fi
```

**PostToolUse** — captures tool calls + results (async):
```bash
if [ -n "$AGENT_TASK_ID" ]; then
  INPUT=$(cat)
  TOOL=$(printf "%s" "$INPUT" | jq -r ".tool_name // empty")
  TOOL_INPUT=$(printf "%s" "$INPUT" | jq -c ".tool_input // {}" | cut -c1-4096)
  TOOL_OUTPUT=$(printf "%s" "$INPUT" | jq -r ".tool_output // \"\"" | cut -c1-4096)
  nibble memory capture "$AGENT_TASK_ID" "tool" "" --tool-name "$TOOL" --tool-input "$TOOL_INPUT" --tool-output "$TOOL_OUTPUT" 2>/dev/null || true
fi
```

**Stop** (extended) — captures final assistant message + triggers summarization:
```bash
# Existing: session-id + notify
SID=$(printf "%s" "$INPUT" | jq -r ".sessionId // empty")
MSG=$(printf "%s" "$INPUT" | jq -r ".last_assistant_message // \"(no message)\"")
[ -n "$SID" ] && nibble report session-id "$AGENT_TASK_ID" "$SID" 2>/dev/null
nibble memory capture "$AGENT_TASK_ID" "assistant" "$MSG" 2>/dev/null || true
nibble notify --task-id "$AGENT_TASK_ID" --message "$MSG" 2>/dev/null || true
# NEW: trigger async summarization
nibble memory summarize "$AGENT_TASK_ID" &>/dev/null &
```

### Pi Adapter

A TypeScript extension at `~/.pi/agent/extensions/nibble-memory.ts`:

```typescript
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { execSync } from "child_process";

export default function (pi: ExtensionAPI) {
  const capture = (taskId: string, role: string, content: string,
                    extra?: Record<string, string>) => {
    const args = [`memory`, `capture`, taskId, role, content];
    for (const [k, v] of Object.entries(extra ?? {})) {
      args.push(`--${k}`, v);
    }
    try {
      execSync(`nibble ${args.map(a => `'${a.replace(/'/g, "'\\''")}'`).join(' ')}`,
               { timeout: 5000, stdio: 'pipe' });
    } catch {}
  };

  const getTaskId = () => process.env.AGENT_TASK_ID ?? '';

  pi.on("turn_end", async (event, ctx) => {
    const taskId = getTaskId();
    if (!taskId) return;
    for (const msg of event.messages ?? []) {
      if (msg.role === "assistant") {
        const text = Array.isArray(msg.content)
          ? msg.content.filter((b: any) => b.type === "text").map((b: any) => b.text).join("\n")
          : String(msg.content);
        if (text) capture(taskId, "assistant", text);
      }
    }
  });

  pi.on("session_shutdown", async (_event, ctx) => {
    const taskId = getTaskId();
    if (!taskId) return;
    try {
      execSync(`nibble memory summarize '${taskId}'`, { timeout: 30000, stdio: 'pipe' });
    } catch {}
  });
}
```

The Pi extension is installed by `install.sh` alongside the skills. Pi auto-discovers
extensions in `~/.pi/agent/extensions/`.

### opencode Adapter

opencode has no hook system. Two paths:

1. **Post-hoc ingestion**: `nibble memory ingest --agent opencode` reads directly
   from `~/.local/share/opencode/opencode.db`. Can be triggered manually or by
   the listen daemon.

2. **Explicit writes**: opencode agents inside sandboxes use `nibble memory write`
   via bash, same as any other agent.

### LLM Extraction

When `nibble memory summarize <session>` runs:

1. Read the capture JSONL for that session.
2. If the JSONL doesn't exist (e.g., opencode), try to read the agent's native
   transcript (Claude JSONL, Pi JSONL, opencode SQLite) as fallback.
3. Build an extraction prompt with the last N turns (truncated to fit context window).
4. Call the configured LLM provider (default: `http://localhost:6969/v1`).

**Extraction prompt:**
```
You are a memory extraction system. Given a coding session transcript, extract
structured memories that would be useful for future sessions.

For each memory, provide:
- type: "session_summary" | "decision" | "pattern" | "user_instruction" |
  "observation" | "bug_record"
- content: concise description (max 500 words)
- tags: relevant technology/topic tags (comma-separated)
- confidence: 0.0 to 1.0

Rules:
- session_summary: exactly ONE per transcript. What was accomplished? Current state?
- decision: "Chose X because Y" — architectural or implementation choices.
- user_instruction: "Always do X" / "Never do Y" — only when USER states a
  preference to remember.
- pattern: Recurring patterns in codebase or workflow.
- observation: Factual notes about codebase, config, or environment.
- bug_record: Bugs found and how they were resolved.

Also identify any lessons learned:
- Things that went wrong and how to prevent them
- Mistakes that were caught and corrected
- Knowledge that would have helped earlier

Respond in JSON: {"memories": [...], "lessons": [...]}
If nothing worth remembering, return {"memories": [], "lessons": []}.
```

5. Parse the JSON response. On failure, fall back to heuristic extraction.
6. For each memory and lesson, write a Markdown file.
7. Compute embeddings for each new file and append to `.embeddings.json`.
8. Update `.index.json` and `index.md`.
9. `git add -A && git commit -m "memory: summarize session <id>"`.

### Lessons Engine

Lessons are separate from general memories because they have a different lifecycle:

| Dimension | Memories | Lessons |
|---|---|---|
| Origin | Auto-extracted or agent-written | Extracted from failures, QA catches, bugs |
| Lifecycle | Write once, search on demand | Active → resolved → encoded into skill |
| Loading | On-demand (`nibble memory search`) | **Proactive** — semantically matched to current task |
| Scope | Tagged with project, globally accessible | Always global |
| Search | ripgrep keyword + optional embedding | Embedding-first semantic search |
| Storage | `memories/` directory | `lessons/` directory |

**Semantic matching at session start:**

When an agent starts a session, the skill instructs it to run:
```bash
nibble memory lessons --context "brief description of what you're about to work on"
```

This command:
1. Computes an embedding for the context string using the local LLM.
2. Loads `.embeddings.json` and computes cosine similarity against all active
   lesson vectors.
3. Returns the top-N most relevant active lessons (default: 5, configurable).
4. Falls back to keyword search (grep lesson files) if embeddings are unavailable.

**Auto-resolution:**

Lessons transition from `active` → `resolved` automatically when:
1. **Encoded into skill**: A skill file (e.g., `factory-lessons/SKILL.md`) is updated
   and the lesson's content appears in the updated file. Detected by
   `nibble memory lessons --sync` which scans skill files and compares content.
2. **Duplicate detection**: A new lesson with similar embedding (>0.95 cosine
   similarity) to an existing active lesson increments `occurrence_count` in the
   existing file's frontmatter instead of creating a duplicate. If
   `occurrence_count >= 3`, the lesson is promoted to `severity = high` (if not
   already critical).
3. **Manual resolution**: `nibble memory lesson-resolve <id> --note "Fixed by..."`

---

## Interfaces

### CLI Subcommands

```
nibble memory search  <query>     [--project <path>] [--type <type>] [--limit N] [--semantic]
nibble memory list                  [--project <path>] [--type <type>] [--since <date>] [--limit N]
nibble memory show    <id>                                        # show full memory file
nibble memory write   <content>    [--type <type>] [--project <path>] [--tags a,b,c]
                                    [--update <id>]                  # update existing memory
nibble memory forget  <id>                                         # delete memory file
nibble memory stats                                               # counts, from .index.json
nibble memory capture <task-id> <role> <content>  [--tool-name N] [--tool-input I] [--tool-output O]
                                                    # internal: called by hooks/extensions
nibble memory summarize <task-id>  [--force]        # extract memories from captured events
nibble memory ingest  [--agent <type>]              # post-hoc ingestion (opencode, legacy sessions)
nibble memory lessons [--context <desc>] [--status active] [--severity high] [--limit N] [--sync]
nibble memory lesson-add <content> [--category <cat>] [--severity <sev>] [--prevention <prev>]
nibble memory lesson-resolve <id>  [--note <text>]
nibble memory inspect               [--project <path>]  # pager-based browsing
nibble memory reindex                                    # rebuild .index.json + .embeddings.json
nibble memory sync                                       # git add + commit + pull + push
```

### Search Implementation

`nibble memory search` has two modes:

**Keyword (default)**: Uses `ripgrep` to search across memory Markdown files.
Fast, no index needed, works everywhere.

```rust
// Simplified: run rg and parse results
fn search_keyword(query: &str, filters: &SearchFilters) -> Vec<MemoryEntry> {
    let base = memory_dir().join("memories");
    let mut cmd = Command::new("rg");
    cmd.arg("-l").arg("--sortr").arg("modified")
       .arg(query).arg(&base);
    // ... apply filters, parse frontmatter from matched files
}
```

**Semantic (`--semantic`)**: Computes an embedding for the query, then finds
nearest neighbors in `.embeddings.json`.

```rust
fn search_semantic(query: &str, limit: usize) -> Vec<MemoryEntry> {
    let query_vec = compute_embedding(query)?;
    let cache = load_embeddings_cache()?;
    let mut scored: Vec<(f32, &str)> = cache.vectors.iter()
        .map(|(id, vec)| (cosine_similarity(&query_vec, vec), id.as_str()))
        .filter(|(score, _)| *score > 0.5)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    scored.truncate(limit);
    // Load Markdown files for top results
}
```

### Data Types

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MemoryType {
    SessionSummary,
    Decision,
    Pattern,
    UserInstruction,
    Observation,
    BugRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LessonCategory {
    SpecGap,
    ImplBug,
    TestGap,
    AuditBlindSpot,
    QaCatch,
    Process,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LessonSeverity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LessonStatus {
    Active,
    Resolved,
    Encoded,
}

pub struct MemoryEntry {
    pub memory_id: String,
    pub memory_type: MemoryType,
    pub agent_type: String,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub project: Option<String>,
    pub content: String,
    pub tags: Vec<String>,
    pub confidence: f32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub access_count: u32,
    pub file_path: PathBuf,        // path to the .md file
}

pub struct LessonEntry {
    pub lesson_id: String,
    pub category: LessonCategory,
    pub severity: LessonSeverity,
    pub status: LessonStatus,
    pub content: String,
    pub prevention: String,
    pub project: Option<String>,
    pub tags: Vec<String>,
    pub source_session: Option<String>,
    pub occurrence_count: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub resolution_note: Option<String>,
    pub file_path: PathBuf,
}
```

---

## Config Addition (`~/.nibble/config.toml`)

```toml
[memory]
enabled = true

[memory.llm]
# LLM provider for memory extraction and embeddings.
# Uses OpenAI-compatible API format (works with llama.cpp, Ollama, LM Studio, etc.)
provider = "openai_compatible"
base_url = "http://localhost:6969/v1"
api_key = ""
# Model for extraction (chat completion)
model = "default"
# Model for embeddings (embedding endpoint)
embedding_model = "default"
# Embedding dimensions (must match the model output)
embedding_dims = 768

[memory.sync]
# Git remote for memory repo. Set to enable push/pull sync.
# Example: "git@github.com:user/nibble-memory.git"
remote = ""
# Auto-sync after summarization (default: false — opt-in)
auto_sync = false
# Commit author
author_name = "nibble"
author_email = "nibble@local"
```

Environment variable overrides:
- `NIBBLE_MEMORY_LLM_BASE_URL` overrides `config.toml` `base_url`
- `NIBBLE_MEMORY_LLM_API_KEY` overrides `config.toml` `api_key`

---

## Directory Structure

```
~/.nibble/
├── tasks.db                        # Existing task DB (unchanged)
├── config.toml                     # Existing, now with [memory] section
├── memory/                         # NEW — git repo
│   ├── .git/                       # git repository
│   ├── .gitignore                  # ignore capture/, caches
│   ├── memories/                   # One .md file per memory
│   │   ├── 2026-04-21_a1b2c3d4_decision.md
│   │   ├── 2026-04-21_d4e5f6_observation.md
│   │   └── ...
│   ├── lessons/                    # One .md file per lesson
│   │   ├── 2026-04-21_b2c3d4e5_impl_bug.md
│   │   └── ...
│   ├── sessions/                   # Session summaries, grouped by project
│   │   ├── nibble/
│   │   │   ├── 2026-04-21_abc123.summary.md
│   │   │   └── 2026-04-21_def456.summary.md
│   │   ├── my-app/
│   │   │   └── 2026-04-20_ghi789.summary.md
│   │   └── index.md                # Table of contents
│   ├── capture/                    # JSONL event captures (gitignored, append-only)
│   │   ├── nibble/
│   │   │   ├── <session-1>.jsonl
│   │   │   └── <session-2>.jsonl
│   │   └── my-project/
│   │       └── <session-3>.jsonl
│   ├── .index.json                 # Rebuildable cache (gitignored)
│   └── .embeddings.json            # Rebuildable vector cache (gitignored)
```

---

## Skill: nibble-memory

Installed at `~/.claude/skills/nibble-memory/SKILL.md` (bind-mounted into all
sandboxes via the existing `~/.nibble/` volume):

```markdown
---
name: nibble-memory
description: Persistent memory for cross-session knowledge. Search past sessions,
  write memories, load relevant lessons, and manage what gets remembered.
---

# nibble Memory System

You have access to persistent memory that survives across sessions, projects, and
devices. Use it to avoid solving the same problem twice.

## Quick Reference

```bash
# Search memories (keyword or semantic)
nibble memory search "authentication pattern"
nibble memory search "podman healthcheck" --project /workspace
nibble memory search "rust error handling" --semantic

# Write a memory
nibble memory write "We chose Markdown over SQLite for git-diffability" \
  --type decision --tags rust,storage,git

# Update an existing memory when you learn more
nibble memory write "Updated: Markdown chosen, see full rationale in decision file" \
  --update <memory_id>

# Delete a stale or incorrect memory
nibble memory forget <memory_id>

# Browse all memories
nibble memory list --type decision
nibble memory stats

# Read session summaries directly
cat ~/.nibble/memory/sessions/index.md
grep -r "auth" ~/.nibble/memory/sessions/
grep -r "auth" ~/.nibble/memory/memories/

# Load relevant lessons for your current task
nibble memory lessons --context "what you're working on"
```

## Judgment: When to Read

Search memory **before** starting any non-trivial task. Someone (possibly you in a
past session) may have already solved this or made a relevant decision.

Also read memory when you're stuck — the answer might be in a past session's
observations or bug records.

## Judgment: When to Write

Ask yourself: **"If I started fresh tomorrow, would I want to know this?"**

If yes, write it down. Common triggers:

- You just made a non-obvious choice between alternatives (→ `decision`)
- The user explicitly said to remember something (→ `user_instruction`)
- You discovered a pattern or convention in the codebase (→ `pattern`)
- You fixed a bug and want to record the root cause (→ `bug_record`)
- You observed something non-obvious about the system (→ `observation`)
- Something went wrong that could have been prevented (→ `nibble memory lesson-add`)

Most turns don't produce memorable knowledge. That's fine. Write selectively.

## Judgment: When to Update or Forget

- **Update** a memory when new information changes the conclusion
  (`nibble memory write "..." --update <id>`)
- **Forget** a memory when it's stale, incorrect, or no longer relevant
  (`nibble memory forget <id>`)
- **Promote** an observation to a decision if it turned out to be a deliberate choice

## Memory Types

| Type | When to use | Example |
|------|------------|---------|
| `decision` | Architectural or implementation choice | "Chose ripgrep over FTS5 for keyword search at our scale" |
| `user_instruction` | Explicit user preference to remember | "Never push to main without running tests first" |
| `pattern` | Recurring code or workflow pattern | "This codebase uses Result<T> everywhere, never unwrap()" |
| `observation` | Non-obvious factual note | "The Podman API hangs if the container is paused" |
| `bug_record` | Bug + root cause + fix | "Missing null check in auth.rs:42 caused panic on expired tokens" |
| `session_summary` | Auto-generated — don't write manually | Generated by `nibble memory summarize` |

## Lessons

Lessons are things that went wrong and how to prevent them. They're loaded
proactively at session start based on what you're working on.

```bash
# Add a lesson from something you just learned the hard way
nibble memory lesson-add "Rust derive macros don't work with serde(default) on enum variants" \
  --category impl_bug --severity medium \
  --prevention "Use serde(default) only on struct fields, not enum variants"

# Load lessons relevant to your current task
nibble memory lessons --context "implementing rust authentication middleware"
```

## Tips

- All memories are global — any session in any project can search any memory
- Memories are tagged with the project they came from for context, never siloed
- Use `--tags` generously for better retrieval
- Search before writing to avoid duplicates
- You can read memory files directly: `cat ~/.nibble/memory/memories/<file>.md`
- The entire memory store is versioned in git — nothing is truly lost
```

The skill is installed by `install.sh` alongside the factory skills. The install
loop changes from `factory-*/` to `{factory,nibble}-*/`.

---

## Implementation Phases

### Phase 1: Foundation (File Store + CLI + Skill)

**Goal**: Agents can write, search, and manage memories as Markdown files.

**Files to create/modify:**
```
src/
├── memory/
│   ├── mod.rs          — module root, memory dir init, git init
│   ├── models.rs       — MemoryEntry, LessonEntry, enums, frontmatter parsing
│   ├── store.rs        — file CRUD: read/write/delete .md files, frontmatter
│   ├── search.rs       — ripgrep keyword search + embedding cosine similarity
│   ├── index.rs        — .index.json and .embeddings.json management
│   ├── cli.rs          — CLI command handlers
│   └── git.rs          — git init, add, commit, push, pull
├── config.rs           — add MemoryConfig, LlmConfig, SyncConfig
├── cli/mod.rs          — add Memory subcommand group
├── main.rs             — wire memory commands
skills/
└── nibble-memory/
    └── SKILL.md        — agent skill
install.sh              — add nibble-memory skill installation
```

**Acceptance criteria:**
1. `nibble memory write "test" --type observation` creates a `.md` file in
   `~/.nibble/memory/memories/` with correct YAML frontmatter
2. `nibble memory search "test"` returns the written memory (via ripgrep)
3. `nibble memory list` shows all memories (from `.index.json`)
4. `nibble memory stats` shows counts by type
5. `nibble memory forget <id>` deletes the memory file
6. `nibble memory show <id>` displays the full Markdown content
7. Skill is installed and visible to agents in sandboxes
8. `nibble memory lessons --context "auth bug"` returns relevant active lessons
9. `nibble memory lesson-add "test lesson" --category impl_bug` creates a lesson file
10. `nibble memory reindex` rebuilds `.index.json` from files
11. `~/.nibble/memory/` is a git repo with correct `.gitignore`
12. `nibble memory sync` does git add + commit + push + pull

### Phase 2: Auto-Capture + Summarization

**Goal**: Sessions are automatically captured and summarized into memory files.

**Files to create/modify:**
```
src/memory/
├── capture.rs          — JSONL event writer, capture CLI handler
├── summarize.rs        — LLM extraction + heuristic fallback
├── llm.rs              — OpenAI-compatible provider (reuses ureq)
├── embeddings.rs       — embedding computation via LLM
├── materialize.rs      — write session summary .md + regenerate index.md
scripts/
└── setup-claude-hooks.sh — add PostToolUse, UserPromptSubmit hooks
~/.pi/agent/extensions/
└── nibble-memory.ts    — Pi extension
install.sh              — install Pi extension + updated hooks
```

**Acceptance criteria:**
13. Claude Code session: after Stop hook fires, capture JSONL exists with user +
    assistant + tool events
14. Pi session: after session_shutdown, capture JSONL exists
15. `nibble memory summarize <task-id>` reads capture JSONL, calls LLM, writes
    memory `.md` files
16. Session `summary.md` exists in `~/.nibble/memory/sessions/<project>/`
17. `index.md` is regenerated after each summarization
18. If LLM is unavailable, heuristic extraction runs as fallback
19. Lessons are extracted from session events alongside memories
20. `.index.json` and `.embeddings.json` are updated after summarization
21. Git commit is made after summarization

### Phase 3: Semantic Search + Lesson Lifecycle

**Goal**: Semantic matching for lessons, auto-resolution, migration from factory-lessons.

**Files to create/modify:**
```
src/memory/
├── search.rs           — refine semantic search with caching
├── embeddings.rs       — embedding computation + incremental cache updates
├── lessons.rs          — lesson lifecycle, auto-resolution, sync with skills
```

**Acceptance criteria:**
22. `nibble memory search "auth" --semantic` uses embedding similarity
23. `nibble memory lessons --context "fixing rust auth"` returns lessons
    semantically related to auth/Rust, not just keyword matches
24. Duplicate lesson detection: writing a lesson similar to existing one
    (cosine sim > 0.95) increments `occurrence_count`
25. `nibble memory lessons --sync` detects lessons encoded in skill files
    and marks them as `encoded`
26. Lesson with `occurrence_count >= 3` is auto-promoted to `severity = high`
27. `nibble memory lessons --import-skill` migrates factory-lessons entries

---

## Error Handling Strategy

| Error | Expected? | Handling |
|-------|-----------|----------|
| LLM API unreachable | Yes (local model may be down) | Fall back to heuristic extraction |
| LLM returns malformed JSON | Yes | Log warning, fall back to heuristic |
| LLM timeout (>60s) | Yes | Fall back to heuristic |
| Capture JSONL write fails | No (disk/permissions) | Log error, don't block agent |
| Memory file write fails | No (disk/permissions) | Return error to caller |
| `.index.json` corrupted or missing | Yes | Rebuild from Markdown files automatically |
| `.embeddings.json` missing | Yes | Fall back to keyword-only search |
| ripgrep not found | No (should be installed) | Fall back to `grep -r` |
| Git push fails (network, auth) | Yes | Log warning, retry on next sync |
| Git merge conflict | Rare (unique filenames) | Keep both versions (no real conflict) |
| Embedding computation fails | Yes | Store memory without embedding; index later |

---

## Invariants

1. **(INV-1)** Every memory file has a unique filename derived from date + short ID +
   type. Two different memories never share a file.

2. **(INV-2)** `memory_id` (UUID v4) in frontmatter is stable. Writing with
   `--update <id>` modifies the existing file rather than creating a new one.

3. **(INV-3)** `content` is ≤ 4096 characters. Overlong content is truncated at
   write time with `…` suffix.

4. **(INV-4)** Capture JSONL files are append-only. Never modified after write.
   Each session gets its own file.

5. **(INV-5)** `.index.json` and `.embeddings.json` are always rebuildable from
   the Markdown files. They are never the source of truth.

6. **(INV-6)** LLM extraction failure never prevents capture. Events are always
   written to JSONL. Summarization can be retried later with `--force`.

7. **(INV-7)** `project` in frontmatter is populated on every memory, but never
   used to restrict search — only for display and filtering.

8. **(INV-8)** Lesson auto-resolution only transitions `active` → `resolved` or
   `active` → `encoded`. Never transitions back without explicit user action.

9. **(INV-9)** Git commits are made after every write and summarization. The
   git history is the audit trail.

10. **(INV-10)** `.gitignore` always excludes `capture/`, `.index.json`,
    `.embeddings.json`, and `*.tmp`.

---

## Dependencies

### Existing (no new crates needed for Phase 1)
- `serde_json` — frontmatter parsing, JSONL events
- `serde_yaml` — YAML frontmatter (add to Cargo.toml)
- `chrono` — timestamps
- `uuid` — memory_id generation
- `anyhow` — error handling
- `clap` — CLI

### New for Phase 1
- `serde_yaml` — YAML frontmatter parsing/writing
- No database crate needed

### New for Phase 2
- `ureq` (already a dependency for Telegram) — LLM API calls

### Phase 3
- No new dependencies. Embeddings are stored as JSON arrays in `.embeddings.json`.
  Cosine similarity computed in pure Rust.

---

## Security

- Memory content is stored unencrypted locally (same threat model as
  `~/.claude/projects/`).
- LLM API calls are only made from the host (not from sandboxes). API keys
  are never exposed to containers.
- Capture JSONL files are written by hooks on the host. Sandboxes only have
  read access to materialized summaries and CLI access to search/write.
- No memory content is sent to external services except the configured LLM
  provider. Default is localhost (local model).
- Git remote should use SSH or HTTPS with credential storage. Memory content
  may contain code snippets and architectural decisions — treat as sensitive.

---

## Migration from factory-lessons

The existing `factory-lessons/SKILL.md` is a flat Markdown file with manually
appended entries. Migration path:

1. `nibble memory lessons --import-skill ~/.claude/skills/factory-lessons/SKILL.md`
   parses the existing entries and creates a `.md` file in `lessons/` for each,
   with `status = active` (or `encoded` if the entry matches a current skill).
2. The `factory-lessons` skill continues to work for backwards compatibility,
   but new lessons should go through `nibble memory lesson-add`.
3. The `nibble-memory` skill supersedes `factory-lessons` for loading — it
   calls `nibble memory lessons --context "..."` instead of reading the
   static Markdown.

---

## Known Risks

### 1. LLM availability for summarization
If the local LLM server is down when a session ends, summarization falls back
to heuristic extraction. The capture JSONL is always preserved, so re-running
`nibble memory summarize <session> --force` later produces better results.

### 2. Embedding model consistency
If the embedding model changes (different model, different dimensions), all
existing embeddings become invalid. Mitigation: store the model name and
dimensions in `.embeddings.json` header; refuse to mix. `nibble memory reindex`
recomputes all embeddings from scratch.

### 3. Storage growth
Capture JSONL files accumulate. A session with heavy tool use can produce 1-5MB.
Mitigation: `nibble memory stats` shows storage usage. Capture files are
gitignored. A future `nibble memory gc` can compress or archive old captures.

### 4. Memory poisoning
A malicious repo could trick an agent into writing false memories. Mitigations:
`project` tracking shows where memories came from, git history enables audit
and rollback, the extraction prompt limits capture to conversation context
(not raw file content).

### 5. ripgrep availability
`nibble memory search` relies on `rg` being installed. Mitigation: fall back to
`grep -r` if `rg` is not found (slower but always available). Both the host
and sandbox images should have `ripgrep` installed.
