# Cron Feature Implementation Summary

This document summarizes the cron feature implementation for agent-inbox.

## Overview

The cron feature allows scheduling prompts to be automatically injected into sandbox containers at specified intervals. This enables automated tasks like daily standups, periodic health checks, or scheduled maintenance.

## Changes Made

### 1. Dependencies (`Cargo.toml`)
- Added `croner = "2.1"` for cron expression parsing

### 2. Database Schema (`src/db/mod.rs`)
- Bumped schema version to 4
- Added `cron_jobs` table with fields:
  - `id`, `task_id`, `label`, `schedule`, `prompt`
  - `enabled`, `skip_if_running`
  - `last_run`, `next_run`, `created_at`
- Added `idx_cron_next_run` index for efficient due job queries
- Added cron job database methods:
  - `insert_cron_job`, `update_cron_job`, `get_cron_job`
  - `list_cron_jobs`, `delete_cron_job`, `get_due_cron_jobs`

### 3. Models (`src/models/task.rs`)
- Added `CronJob` struct with all necessary fields

### 4. Cron Module (`src/cron/mod.rs`)
New module providing:
- `compute_next_run()` - Calculate next execution time from cron expression
- `validate_schedule()` - Validate cron expressions
- `parse_cron_markdown()` - Parse cron definitions from markdown files
- `format_cron_markdown()` - Format cron jobs as markdown

### 5. CLI (`src/cli/mod.rs`)
Added `Cron` command with subcommands:
- `add <task_id_or_path> --schedule <expr> --prompt <text> --file <path> --label <name>`
- `list [task_id_or_path]`
- `edit <id> [--schedule] [--prompt] [--label] [--enable/--disable]`
- `del <id>`
- `run <id>` (for testing)

### 6. Main (`src/main.rs`)
- Added cron command handlers:
  - `cmd_cron_add()` - Create new cron jobs from CLI or markdown file
  - `cmd_cron_list()` - Display cron jobs in table format
  - `cmd_cron_edit()` - Modify existing cron jobs
  - `cmd_cron_run()` - Execute a cron job immediately

### 7. Telegram Listener (`src/notifications/telegram_listener.rs`)
- Added cron tick to listen loop (checks every 2 polling cycles ≈ 1 min)
- Added `check_and_run_cron_jobs()` function:
  - Finds due jobs
  - Health-checks target sandboxes
  - Spawns background injection with heartbeats
  - Sends Telegram notifications (start with prompt preview, heartbeat, completion)
  - Updates job timestamps
- Added Telegram commands:
  - `/help` - Show all available commands
  - `/cron list [task_id]` - List cron jobs

## Usage Examples

### Create a cron job via CLI:
```bash
agent-inbox cron add ~/projects/myapp --schedule "0 9 * * 1-5" --prompt "Review yesterday's commits" --label "Daily Standup"
```

### Create a cron job via markdown file:
```bash
# Create my-cron.md
agent-inbox cron add ~/projects/myapp --file my-cron.md
```

Markdown format (`my-cron.md`):
```markdown
# Daily Standup

schedule = "0 9 * * 1-5"
enabled = true
skip_if_running = true

## Prompt

Please review yesterday's commits and prepare a summary of what was accomplished.
Focus on the main branch changes.
```

### List cron jobs:
```bash
agent-inbox cron list
agent-inbox cron list ~/projects/myapp
```

### Telegram commands:
- `/help` - Show all commands
- `/sandboxes` - List running sandboxes
- `/spawn /path/to/repo [task]` - Spawn new sandbox
- `/cron list` - List all cron jobs
- `/cron list <task_id>` - List jobs for specific sandbox

## Architecture

The cron feature integrates seamlessly with the existing infrastructure:
- Uses the same `inject_returning_child()` path as Telegram replies
- Same heartbeat mechanism (2-minute updates)
- Same safety-net completion detection
- Leverages existing health check and notification systems

## Design Decisions

1. **Single-turn only** - Multi-turn workflows deferred to future
2. **Skip if running** - Default behavior prevents queue buildup
3. **Missed runs are skipped** - No catch-up to avoid thundering herd
4. **System timezone** - Croner uses local time by default
5. **Markdown file support** - Easier maintenance of complex prompts

## Future Enhancements

See `future.md` for:
- Multi-turn cron jobs
- Queue mode (vs skip)
- Catch-up policies for missed runs
- Full Telegram cron management (add/del via bot)
- Cron job execution history
