# Future iteration prompts

## Next iteration (v1.1)

- **TUI dashboard** (`agent-inbox watch`): replace the current 2-second clear-screen refresh with a proper terminal UI (ratatui). Show task list, last message preview, elapsed time, live status. Navigate with arrow keys, press Enter to inject a message.

- **Session ID race on `--fresh` attach**: when `attach --fresh` is used the Stop hook writes a new session ID, but the DB still holds the old one until the hook fires. A Telegram-triggered inject in that window uses the stale ID. Fix: clear `session_id` from the task when `--fresh` is passed so inject falls back to `--continue`.

- **Listen daemon watchdog**: the systemd service restarts the daemon on crash, but there is no health-check endpoint. Add a `agent-inbox listen --health-port <N>` flag that serves a minimal HTTP ping so an external watchdog (or the systemd `WatchdogSec=`) can detect hangs.

- **Multi-sandbox default target**: when two sandboxes are running and the user replies to an old notification, routing depends on message-ID lookup which may be stale. Add a `/select <task-id>` Telegram command that pins a "default" sandbox target for plain (non-reply) messages.

## Backlog

- **Output capture in notifications**: attach the last N lines of stdout/stderr to completion notifications, not just the last assistant message.
- **Task chaining/dependencies**: a way to say "start task B when task A completes" — natural for multi-step agent pipelines.
- **Telegram notifications for web agents**: push notifications for claude.ai and Gemini web sessions (browser extension integration).
