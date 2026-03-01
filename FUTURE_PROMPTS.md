# Future iteration prompts

## Next iteration (v1.1)

- **TUI dashboard** (`agent-inbox watch`): replace the current 2-second clear-screen refresh with a proper terminal UI (ratatui). Show task list, last message preview, elapsed time, live status. Navigate with arrow keys, press Enter to inject a message.

- **Install toolchain in sandbox**: when spawning a sandbox, detect the project type (package.json, Cargo.toml, go.mod, etc.) and run the appropriate install command before starting Claude, so the agent has a working environment from the start. Also inject a system prompt reminding Claude it is running inside a sandbox container.

## Backlog

- **Output capture in notifications**: attach the last N lines of stdout/stderr to completion notifications, not just the last assistant message.
- **Task chaining/dependencies**: a way to say "start task B when task A completes" — natural for multi-step agent pipelines.
- **Telegram notifications for web agents**: push notifications for claude.ai and Gemini web sessions (browser extension integration).
