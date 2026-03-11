# Future Features for Agent Inbox Cron

This document tracks features that were discussed but deferred for future iterations.

## ~~Cron Job Expiry~~ ✅ Implemented

`expires_at` field on `cron_jobs` (DB schema v6). Jobs are auto-disabled when the
daemon tick fires after the expiry time. CLI: `--expires <RFC3339>` on `add`/`edit`,
`--expires none` to clear.

## Multi-Turn Cron Jobs

**Status:** Deferred (v1 has single-turn only)

Currently, cron jobs execute a single prompt. In the future, we may want to support multi-turn workflows where:
- The second prompt runs after the first one completes
- This allows for sequential tasks like "analyze code" → "create PR"

**Implementation considerations:**
- Need to track state between turns (which turn we're on, what was the previous output)
- Could store `turn_index` and `total_turns` in the cron_jobs table
- Would need to modify the cron execution logic to chain prompts

## Queue vs Skip Configuration

**Status:** Partially implemented (skip is default, queue is future)

Currently `skip_if_running` controls whether to skip when a job is still running. In the future we may want:
- `queue` mode: Queue the next run to execute after the current one finishes
- `parallel` mode: Allow multiple concurrent executions (dangerous but maybe useful)

**Implementation considerations:**
- Queue mode needs a job queue table or in-memory queue
- Need to handle queue overflow (max queue size)
- Need to track which runs are "queued" vs "executing"

## Missed Run Handling

**Status:** Deferred (currently missed runs are skipped)

Currently if the daemon is down during a scheduled run, that run is missed. Future options:
- `catch_up_once`: Run once immediately if missed
- `catch_up_all`: Run all missed occurrences (dangerous - could spam)
- Configurable per-job via `missed_run_policy` field

**Implementation considerations:**
- Need to detect daemon restarts (store last_check timestamp)
- On startup, check if any jobs are "overdue" and handle according to policy
- Careful with catch_up_all - could cause thundering herd

## Telegram Cron Management Enhancements

**Status:** Partially implemented (list only, add/del are future)

Current Telegram commands:
- `/cron list` - List cron jobs
- `/cron list <task_id>` - List jobs for specific sandbox

Future commands:
- `/cron add <task_id> <schedule>` - Interactive add (prompt for schedule, then prompt text)
- `/cron del <id>` - Delete a cron job
- `/cron run <id>` - Trigger a job immediately
- `/cron enable <id>` / `/cron disable <id>` - Toggle jobs

## Cron Job Output Capture

**Status:** Not implemented

Currently cron job output goes to the agent conversation. Future enhancement:
- Capture output and send summary to Telegram
- Store last N outputs in DB for review
- Option to "reply" to cron job output to continue the conversation

## Cron Job Templates

**Status:** Not implemented

Predefined cron patterns:
- `@daily` = `0 0 * * *`
- `@hourly` = `0 * * * *`
- `@weekly` = `0 0 * * 0`
- `@reboot` = Run once on daemon startup

## Cron Job History

**Status:** Not implemented

Track execution history:
- Success/failure status
- Duration
- Exit code
- Output summary
- Store in `cron_job_runs` table

## Cron Job Notifications

**Status:** Partially implemented — prompt preview in start notification done

Enhanced notification options:
- Notify on success (currently only on start/complete/failure)
- Notify on skip
- Custom notification messages per job
- Different notification channels per job (if multiple integrations added)
