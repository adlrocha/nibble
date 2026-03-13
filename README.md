# Nibble

> *In Futurama, Nibbler's species is tasked with protecting the universe from giant flying brains. Nibble is your orchestrator — keeping watch over the smaller agents so you don't have to.*

A CLI tool that runs Claude Code agents inside isolated Podman sandboxes, monitors their status, sends Telegram notifications when they need your attention, and lets you reply from your phone to unblock them — all without touching your keyboard.

## Features

- **Podman Sandboxes**: Run agents in rootless containers — repo mounted read-write, ports exposed, full dev flexibility inside
- **Telegram Notifications**: Receive the agent's last message on your phone when it finishes or needs a decision
- **Telegram Replies**: Reply to notifications from your phone to unblock agents (input injected via `podman exec`)
- **Auto-Resume**: Sandbox agents are tracked across host reboots
- **Unified Task Tracking**: Track sandboxed and non-sandboxed agents in one dashboard
- **3-State Model**: Running → Completed → Exited
- **SQLite Backend**: Fast, reliable, concurrent-safe storage

## How it Works

```
┌─────────────────────────────────────────────────────────────────────┐
│                          HOST SYSTEM                                │
│                                                                     │
│  nibble CLI               SQLite DB           Telegram Listener     │
│  ─────────                ─────────           ────────────────      │
│  sandbox / kill           tasks.db            long-polls Telegram   │
│  list / watch             container_state     routes replies to     │
│  prune / inject           session_id          sandbox via exec      │
│         │                                              │            │
│         │ podman run                                   │            │
│         ▼                                              ▼            │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │  Podman Container                                            │   │
│  │                                                              │   │
│  │  claude --resume <id>  ←── podman exec -i (stdin message)   │   │
│  │                                                              │   │
│  │  Stop hook → nibble report session-id / last-message        │   │
│  │  /workspace  (repo mounted RW)                              │   │
│  └──────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
                         │
                         ▼
               📱 Telegram (your phone)
```

**Notification flow**: Claude Code `Stop` hook → `nibble report last-message` + `nibble notify` → Telegram message with Reply button

**Reply flow**: Telegram reply → listener daemon → `podman exec -i claude --resume <session-id>` (stdin) → Claude processes turn → Stop hook fires → next notification sent

## Installation

### Prerequisites

- Rust 1.70+ (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- `jq` (`pacman -S jq` / `apt install jq` / `brew install jq`)
- Linux (tested on Arch Linux; macOS works for non-sandbox mode)

Podman is installed automatically by `install.sh` if not present.

### Install

```bash
git clone <repo-url>
cd nibble

# Full install (builds binary, installs podman if needed, builds sandbox image)
./install.sh

# Also set up Telegram notifications
./install.sh --telegram

# Also start the reply listener daemon
./install.sh --telegram --listen
```

After install, add aliases to your shell if prompted:

```bash
# ~/.zshrc or ~/.bashrc
alias claude='~/.agent-tasks/wrappers/claude-wrapper'
```

What `install.sh` does:
1. Installs Podman if not present (apt / dnf / pacman / brew)
2. Builds release binaries with `cargo build --release`
3. Installs `nibble` and `agent-bridge` to `~/.local/bin/`
4. Copies wrappers to `~/.agent-tasks/wrappers/`
5. Installs Claude Code hooks to `~/.claude/settings.json`
6. Builds the sandbox image (`nibble-sandbox:latest` — node:20-slim + claude-code)
7. Enables `nibble-resume.service` (systemd user service, resumes agents on reboot)
8. Optionally sets up Telegram and the reply listener daemon

---

## Sandbox Usage

### Start an agent

```bash
# Spawn a sandbox and attach immediately
nibble sandbox spawn /path/to/repo
nibble sandbox spawn /path/to/repo --task "Fix the authentication bug"

# Spawn without attaching (run in background)
nibble sandbox spawn /path/to/repo --task "Fix the authentication bug"

# Build/refresh the sandbox image only (no agent)
nibble sandbox build
```

On first run the sandbox image is built (~2-3 min). Subsequent spawns are fast.

The container gets:
- Your repo mounted read-write at `/workspace`
- `ANTHROPIC_API_KEY` and other configured env vars forwarded from host
- Ports `3000-3100` and `8000-8100` available on the host
- Full privileged mode inside (install anything with `apt`, `npm`, `pip`, etc.)
- Dependency caches (`~/.npm`, `~/.npm-global`) persisted across restarts

### List open sandboxes

```bash
nibble sandbox list
```

Output:
```
TASK ID              STARTED            STATUS       REPO
──────────────────────────────────────────────────────────────────────────────────────
a1b2c3d4             2026-03-13 09:12   healthy      /home/you/projects/myapp
```

### Attach to a running sandbox

Drop into an interactive Claude Code session inside the container. Detaching doesn't stop the container — you can re-attach any time.

```bash
# By repo path — most convenient from inside the repo
nibble sandbox attach .
nibble sandbox attach /path/to/repo

# By task ID or prefix
nibble sandbox attach <task-id>
nibble sandbox attach a1b2c3d4

# Start a fresh conversation instead of resuming
nibble sandbox attach . --fresh

# Use an alternative LLM backend
nibble sandbox attach . --kimi
nibble sandbox attach . --glm
```

Detach with `exit` or `Ctrl+C`.

### Watch container logs

```bash
podman logs -f nibble-<timestamp>-<short-id>
```

### Kill a sandbox

```bash
# By repo path
nibble sandbox kill .
nibble sandbox kill /path/to/repo

# By task ID or prefix
nibble sandbox kill <task-id>

# Kill all sandboxes
nibble sandbox kill --all
```

Stops the container and marks the task as exited.

### Resume after reboot

Run automatically on login via systemd, or manually:

```bash
nibble sandbox resume --all
```

---

## Monitoring

```bash
# Show running tasks (default)
nibble

# List all tasks
nibble list --all

# Filter by status
nibble list --status running
nibble list --status completed
nibble list --status exited

# Live dashboard (refreshes every 2s)
nibble watch

# Detailed view of one task
nibble show <task-id>

# Clean up completed/exited tasks
nibble clear-all
nibble reset --force   # wipe everything

# Prune stale tasks (dead PIDs / gone containers → mark exited)
# Runs automatically every ~5 min inside the listen daemon
nibble prune
```

---

## Telegram Setup

### Notifications

```bash
./install.sh --telegram
```

The setup script walks you through creating a bot via @BotFather and writes `~/.agent-tasks/config.toml`:

```toml
[telegram]
enabled = true
bot_token = "123456789:ABCdefGHIjklMNOpqrSTUvwxYZ"
chat_id   = "123456789"
# allowed_username = "yourusername"   # optional extra auth
```

**Message format:**

```
🤖 Claude Code
📁 myapp · main
⏱ 4m 32s
────────────────────────────
I've finished the refactor. Here's what changed: ...
```

```
🚨 Needs your attention
🤖 Claude Code
📁 myapp · main
⏱ 2m 10s
────────────────────────────
Claude needs permission to run: npm install
```

### Reply listener (send messages to agents from phone)

```bash
./install.sh --listen
```

This installs a systemd user service (`agent-listener.service`) that long-polls Telegram. Every notification has a **↩ Reply** button — tap it, type your message, and it gets injected into the agent inside its container.

```bash
# Daemon management
systemctl --user status agent-listener
journalctl --user -u agent-listener -f
```

You can also inject directly from the terminal (bypasses Telegram):

```bash
nibble inject <task-id> "Yes, proceed with the migration"
```

#### Telegram bot commands

```
/help           — show available commands
/sandboxes      — list running sandboxes with reply buttons
/spawn <path>   — spawn a new sandbox
/cron list      — list scheduled cron jobs
```

---

## Cron Jobs

Schedule prompts to run automatically inside a sandbox:

```bash
# Add a daily standup at 9am weekdays
nibble cron add ~/projects/myapp \
    --schedule "0 9 * * 1-5" \
    --prompt "Review yesterday's commits and summarise what was done." \
    --label "Daily Standup"

# From a markdown file (easier for long prompts)
nibble cron add ~/projects/myapp --file my-cron.md

# List all cron jobs
nibble cron list
nibble cron list ~/projects/myapp

# Edit a job
nibble cron edit "Daily Standup" --schedule "0 8 * * 1-5"
nibble cron edit "Daily Standup" --disable

# Delete a job
nibble cron del "Daily Standup"

# Run immediately (for testing)
nibble cron run "Daily Standup"
```

Markdown file format (`my-cron.md`):
```markdown
# Daily Standup

schedule = "0 9 * * 1-5"
enabled = true
skip_if_running = true

## Prompt

Please review yesterday's commits and prepare a summary of what was accomplished.
Focus on the main branch changes.
```

---

## How it works

### Sandbox model

Each repo gets **one long-lived container**. The container starts with `sleep infinity` as PID 1 and keeps running between sessions. Claude Code is launched transiently via `podman exec` on attach and exits when you detach — the container itself is unaffected.

If you try to spawn a sandbox for a repo that already has one, nibble re-attaches to the existing container instead.

### Session continuity

When you attach, Claude resumes the most recent conversation for the repo using `--resume`. Every attach — whether interactive from the terminal or a Telegram reply injected remotely — picks up the same conversation history. Session state is stored inside the container at `/workspace/.claude/projects/`.

### Telegram injection

When you reply to a notification from your phone, the listener daemon:

1. Receives the reply via Telegram long-poll
2. Runs `claude --continue` inside the container with your message on stdin (non-interactive, one-shot turn)
3. Claude loads the full prior conversation history and processes the new message
4. The Stop hook inside the container fires when Claude finishes and sends the response back to Telegram

The injected turn and an interactive attach session share the same conversation history — they are just different ways to add a turn to the same session.

### Container crash notifications

If a container disappears unexpectedly (OOM, host kill, etc.), the prune daemon detects it and sends a Telegram notification so you can re-spawn. Normal session exits (detach, turn complete) are not notified.

---

## Sandbox Security Model

Sandboxes use **rootless Podman** — containers run as your user, so even a full container escape grants no root access on the host. The goal is to protect your host from:

- Claude Code modifying files outside the repo
- Prompt injection attacks that could run arbitrary host commands
- Accidental `rm -rf` or other destructive operations on the host

Inside the container, agents have full privileges (install packages, run anything). This is intentional — it's a dev environment, and flexibility matters more than internal isolation.

Network is host-mode, so services started inside the container (e.g. `npm run dev` on port 3000) are immediately accessible on the host.

---

## Task States

| State | Meaning |
|-------|---------|
| **Running** | Agent is actively generating output |
| **Completed** | Agent finished, waiting for user input |
| **Exited** | Container stopped or process terminated |

---

## Command Reference

| Command | Purpose |
|---------|---------|
| `nibble` | Show running tasks |
| `nibble list --all` | List all tasks |
| `nibble watch` | Live dashboard |
| `nibble show <id>` | Task detail |
| `nibble clear-all` | Clear completed/exited tasks |
| `nibble prune` | Mark stale processes as exited |
| `nibble inject <id> <msg>` | Send message to agent |
| `nibble notify --message <msg>` | Send Telegram notification |
| `nibble sandbox spawn <repo>` | Start a sandboxed agent |
| `nibble sandbox list` | List open sandboxes |
| `nibble sandbox attach <id>` | Attach to sandbox |
| `nibble sandbox kill <id>` | Stop sandbox |
| `nibble sandbox kill --all` | Stop all sandboxes |
| `nibble sandbox resume --all` | Resume agents after reboot |
| `nibble sandbox build` | Rebuild sandbox image |
| `nibble cron add` | Schedule a prompt |
| `nibble cron list` | List cron jobs |
| `nibble cron edit <id>` | Modify a cron job |
| `nibble cron del <id>` | Delete a cron job |
| `nibble cron run <id>` | Run immediately |
| `install.sh` | Install / upgrade |
| `install.sh --telegram` | Set up Telegram bot |
| `install.sh --listen` | Start reply listener daemon |

---

## Development

```bash
cargo test          # run all tests
cargo build         # dev build
cargo build --release
```

### Project structure

```
src/
├── main.rs                  # CLI entry point, all command handlers
├── cli/mod.rs               # clap argument definitions
├── models/task.rs           # Task, TaskStatus, SandboxType, SandboxConfig
├── db/mod.rs                # SQLite operations, schema migrations
├── sandbox/
│   ├── mod.rs               # Sandbox trait, ContainerInfo, helpers
│   └── podman.rs            # Podman implementation + Dockerfile
├── agent_input.rs           # Input injection via podman exec -i (sandbox tasks)
├── notifications/
│   ├── telegram.rs          # Send Telegram messages
│   └── telegram_listener.rs # Long-polling daemon, reply routing
├── config.rs                # TOML config loader
├── display/mod.rs           # Terminal task list rendering
└── monitor/mod.rs           # Process liveness monitoring
```

## License

Apache 2.0
