# Agent Inbox

A CLI-first notification system that tracks tasks across multiple LLM/coding agents (Claude Web, Gemini Web, Claude Code, OpenCode, etc.) and provides a simple inbox-style dashboard to view tasks requiring attention.

## Features

- **Unified Task Tracking**: Track tasks across different AI agents in one place
- **3-State Model**: Simple, reliable status tracking (Running → Completed → Exited)
- **Desktop Notifications**: Get notified when agents finish generating
- **Telegram Notifications**: Receive the agent's last message on your phone when it needs your attention
- **Transparent Wrappers**: Auto-track CLI agents without changing your workflow
- **Browser Extension**: Track Claude.ai and Gemini conversations
- **Repo-Aware**: Shows git repo and branch in task titles for CLI agents
- **Auto-Reset on Restart**: Clears stale tasks automatically on login
- **SQLite Backend**: Fast, reliable, and concurrent-safe storage

## Task States

- **Running**: Agent is actively generating output
- **Completed**: Agent finished generating, waiting for user input
- **Exited**: Agent/tab closed or process terminated

## Installation

### Prerequisites

- Rust 1.70+ (for building)
- `jq` (for hook message extraction — `pacman -S jq` / `apt install jq`)
- Linux (tested on Arch Linux)

### Install / Upgrade

A single script handles everything: build, binaries, wrappers, and Claude Code hooks.

```bash
git clone <repo-url>
cd agent-inbox

# First-time install or upgrade
./install.sh

# Install + set up Telegram bot in one go
./install.sh --telegram
```

After running, add aliases to your `~/.zshrc` or `~/.bashrc` if the script reports them missing:

```bash
alias claude='~/.agent-tasks/wrappers/claude-wrapper'
alias opencode='~/.agent-tasks/wrappers/opencode-wrapper'
```

Then reload your shell and **restart Claude Code** for hooks to take effect.

What `install.sh` does:
1. Builds release binaries with `cargo build --release`
2. Installs `agent-inbox` and `agent-bridge` to `~/.local/bin/`
3. Copies updated wrappers to `~/.agent-tasks/wrappers/`
4. Removes stale hooks from `~/.claude/settings.json` and reinstalls the latest version
5. Skips Telegram setup unless `--telegram` is passed

### Telegram Notifications (optional)

Get notified on your phone whenever Claude Code or OpenCode finishes a turn or needs a permission decision, with the agent's last message included so you know what it's waiting on.

The easiest way to set this up is via `install.sh`:

```bash
./install.sh --telegram
```

Or run the setup script directly:

```bash
./scripts/setup-telegram.sh
```

The script will:
1. Walk you through creating a bot via @BotFather in Telegram
2. Auto-detect your chat ID
3. Write `~/.agent-tasks/config.toml`
4. Send a test message to confirm everything works

**Config file** (`~/.agent-tasks/config.toml`):

```toml
[telegram]
enabled = true
bot_token = "123456789:ABCdefGHIjklMNOpqrSTUvwxYZ"
chat_id = "123456789"
```

Set `enabled = false` to temporarily disable notifications without removing the config.

**Message format — turn finished:**

```
🤖 Claude Code
📁 agent-inbox · main
⏱ 4m 32s
────────────────────────────
I've finished refactoring the auth module. Here's what I changed: ...
```

**Message format — permission / attention needed:**

```
🚨 Needs your attention
🤖 Claude Code
📁 agent-inbox · main
⏱ 2m 10s
────────────────────────────
Claude needs permission to run: npm install
```

If the agent output exceeds 4096 characters, only the last 4096 are sent (with a truncation notice) so you always see the most recent output.

**How it works per agent:**

- **Claude Code**: The `Stop` hook reads `last_assistant_message` from stdin and sends it. The `Notification` hook fires on `permission_prompt` events and sends a `🚨` alert with the permission description.
- **OpenCode**: The wrapper records terminal output with `script` (preserving the interactive TUI), strips ANSI codes on exit, and sends the result.

### 3. Auto-Reset on Login/Restart

Automatically clear stale tasks when you log in:

```bash
./scripts/setup-auto-reset.sh
```

This creates a systemd user service that runs `agent-inbox reset --force` on each login.

To disable: `systemctl --user disable agent-inbox-reset.service`

### 4. Browser Extension (Claude.ai & Gemini)

To track web-based agents:

```bash
# 1. Load extension in browser
#    - Go to chrome://extensions (or brave://extensions)
#    - Enable "Developer mode"
#    - Click "Load unpacked" and select the `extension/` directory

# 2. Get extension ID from the extensions page

# 3. Update native messaging manifest
mkdir -p ~/.config/BraveSoftware/Brave-Browser/NativeMessagingHosts/
# (or ~/.config/google-chrome/NativeMessagingHosts/ for Chrome)

cat > ~/.config/BraveSoftware/Brave-Browser/NativeMessagingHosts/com.agent_tasks.bridge.json << EOF
{
  "name": "com.agent_tasks.bridge",
  "description": "Native messaging host for Agent Inbox extension",
  "path": "$HOME/.local/bin/agent-bridge",
  "type": "stdio",
  "allowed_origins": [
    "chrome-extension://YOUR_EXTENSION_ID/"
  ]
}
EOF

# 4. Reload the extension
```

## Usage

### Basic Commands

```bash
# Show running tasks (default)
agent-inbox

# List all tasks
agent-inbox list --all

# List tasks by status
agent-inbox list --status running
agent-inbox list --status completed
agent-inbox list --status exited

# Show detailed task information
agent-inbox show <task-id>

# Clear a specific task
agent-inbox clear <task-id>

# Clear all completed and exited tasks
agent-inbox clear-all

# Force clear ALL tasks (useful when stuck)
agent-inbox reset --force

# Watch tasks in real-time (refreshes every 2s)
agent-inbox watch

# Manual cleanup of old completed tasks
agent-inbox cleanup --retention-secs 3600
```

### Manual Task Reporting

```bash
# Start a task
TASK_ID=$(uuidgen)
agent-inbox report start "$TASK_ID" "claude_code" "$PWD" "My task description"

# Mark task as running (generating)
agent-inbox report running "$TASK_ID"

# Mark task as completed (finished generating)
agent-inbox report complete "$TASK_ID"

# Mark task as exited (process terminated)
agent-inbox report exited "$TASK_ID" --exit-code 0
```

### Sending Notifications Manually

```bash
# Normal completion notification (with task context)
agent-inbox notify --task-id "$TASK_ID" --message "Agent output here"

# Attention-required notification — shows 🚨 banner (permission, question, etc.)
agent-inbox notify --task-id "$TASK_ID" --message "Do you want to proceed?" --attention

# Without task context
agent-inbox notify --message "Something needs your attention" --attention
```

If Telegram is not configured the command exits cleanly with a warning — it will not break hooks or wrappers.

## Scripts Reference

| Script | Purpose |
|--------|---------|
| `install.sh` | **One-command install / upgrade** — build, binaries, wrappers, hooks. Pass `--telegram` to also run Telegram setup |
| `scripts/setup-telegram.sh` | Interactive Telegram bot setup — writes `~/.agent-tasks/config.toml` |
| `scripts/setup-claude-hooks.sh` | Install Claude Code hooks globally (`~/.claude/settings.json`) |
| `scripts/setup-auto-reset.sh` | Install systemd service for auto-reset on login |
| `scripts/setup-wrappers.sh` | Install wrappers and add shell aliases automatically |
| `scripts/install-extension.sh` | Build and install the browser extension native messaging host |
| `scripts/fix-native-messaging.sh` | Interactive fix for browser native messaging configuration |
| `wrappers/claude-wrapper` | Wrapper script for Claude Code CLI |
| `wrappers/opencode-wrapper` | Wrapper script for OpenCode CLI (records output for notifications) |

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                        User                                   │
└──────────────────────────────────────────────────────────────┘
          │                    │                    │
          ▼                    ▼                    ▼
┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐
│  Claude Code     │  │  Claude.ai       │  │  Gemini          │
│  (wrapper+hooks) │  │  (extension)     │  │  (extension)     │
└────────┬─────────┘  └────────┬─────────┘  └────────┬─────────┘
         │                     │                     │
┌────────────────┐              ▼                     │
│  OpenCode      │    ┌──────────────────┐           │
│  (wrapper)     │    │  agent-bridge    │           │
└────────┬───────┘    │  (native msg)    │           │
         │            └────────┬─────────┘           │
         │                     │                     │
         ▼                     ▼                     ▼
┌──────────────────────────────────────────────────────────────┐
│                      agent-inbox CLI                          │
│   report / notify / list / watch / reset / ...               │
└───────────────────────┬──────────────────────────────────────┘
                        │
            ┌───────────┴───────────┐
            ▼                       ▼
  ┌──────────────────┐    ┌──────────────────────┐
  │    SQLite DB     │    │   Telegram Bot API   │
  │ ~/.agent-tasks/  │    │  (on Stop / session  │
  │   tasks.db       │    │   end — sends last   │
  └──────────────────┘    │   agent message)     │
                          └──────────────────────┘
                                     │
                                     ▼
                             📱 Your phone
```

**Notification flow (Claude Code):**
1. Claude Code fires the `Stop` hook with `last_assistant_message` on stdin
2. Hook calls `agent-inbox report complete` then `agent-inbox notify --message "$MSG"`
3. `agent-inbox notify` reads `~/.agent-tasks/config.toml`, prepends task context, sends to Telegram

**Notification flow (OpenCode):**
1. Wrapper runs OpenCode inside `script` to capture PTY output
2. On exit, ANSI codes are stripped and the output is passed to `agent-inbox notify`
3. Same Telegram send path as above

## Development

```bash
# Run tests
cargo test

# Build for development
cargo build

# Build release
cargo build --release
```

## License

Apache 2.0
