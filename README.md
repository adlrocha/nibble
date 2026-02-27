# Agent Inbox

A CLI tool that runs Claude Code agents inside isolated Podman sandboxes, monitors their status, sends Telegram notifications when they need your attention, and lets you reply from your phone to unblock them — all without touching your keyboard.

## Features

- **Podman Sandboxes**: Run agents in rootless containers — repo mounted read-write, ports exposed, full dev flexibility inside
- **Telegram Notifications**: Receive the agent's last message on your phone when it finishes or needs a decision
- **Telegram Replies**: Reply to notifications from your phone to unblock agents (input injected via named pipe)
- **Auto-Resume**: Sandbox agents are tracked across host reboots
- **Unified Task Tracking**: Track sandboxed and non-sandboxed agents in one dashboard
- **3-State Model**: Running → Completed → Exited
- **SQLite Backend**: Fast, reliable, concurrent-safe storage

## How it Works

```
┌─────────────────────────────────────────────────────────────────────┐
│                          HOST SYSTEM                                │
│                                                                     │
│  agent-inbox CLI          SQLite DB           Telegram Listener     │
│  ─────────────            ─────────           ────────────────      │
│  sandbox / kill           tasks.db            long-polls Telegram   │
│  list / watch             container_state     routes replies to     │
│  resume                                       container via pipe    │
│         │                                              ▲            │
│         │ podman run                                   │            │
│         ▼                                              │            │
│  ┌──────────────────────────────────────┐              │            │
│  │  Podman Container                    │              │            │
│  │                                      │              │            │
│  │  claude-code ←── input-forwarder ────┼──── podman exec ──────── │
│  │                  (reads named pipe)  │                           │
│  │  /workspace  (repo mounted RW)       │                           │
│  │  ports 3000-3100, 8000-8100 open     │                           │
│  └──────────────────────────────────────┘                           │
└─────────────────────────────────────────────────────────────────────┘
                         │
                         ▼
               📱 Telegram (your phone)
```

**Notification flow**: Claude Code `Stop` hook → `agent-inbox notify` → Telegram message with Reply button

**Reply flow**: Telegram reply → listener daemon → `podman exec` → `/tmp/agent-input.pipe` → `input-forwarder.sh` → Claude's TTY

## Installation

### Prerequisites

- Rust 1.70+ (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- `jq` (`pacman -S jq` / `apt install jq` / `brew install jq`)
- Linux (tested on Arch Linux; macOS works for non-sandbox mode)

Podman is installed automatically by `install.sh` if not present.

### Install

```bash
git clone <repo-url>
cd agent-inbox

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
3. Installs `agent-inbox`, `agent-bridge`, and `agent-sandbox` to `~/.local/bin/`
4. Copies wrappers to `~/.agent-tasks/wrappers/`
5. Installs Claude Code hooks to `~/.claude/settings.json`
6. Builds the sandbox image (`agent-inbox-sandbox:latest` — node:20-slim + claude-code)
7. Enables `agent-inbox-resume.service` (systemd user service, resumes agents on reboot)
8. Optionally sets up Telegram and the reply listener daemon

---

## Sandbox Usage

### Start an agent

```bash
# Quickest way — use the convenience script
agent-sandbox /path/to/repo
agent-sandbox /path/to/repo "Fix the authentication bug"

# Or directly via agent-inbox
agent-inbox sandbox /path/to/repo
agent-inbox sandbox /path/to/repo --task "Fix the authentication bug"

# Build/refresh the sandbox image only (no agent)
agent-sandbox --build-only
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
agent-sandbox list
# or
agent-inbox sandboxes
```

Output:
```
TASK ID                CONTAINER                        STATUS     REPO
──────────────────────────────────────────────────────────────────────────────────────────
a1b2c3d4e5f6g7h8i9    agent-inbox-a1b2c3d4e5f6g7h8     running    /home/you/projects/myapp
```

### Attach to a running sandbox

Drop into an interactive bash shell inside the container. The Claude Code agent keeps running — attaching and detaching doesn't interrupt it.

```bash
agent-sandbox attach <task-id>
# or
agent-inbox attach <task-id>
```

Detach with `exit` or `Ctrl+D`.

### Watch logs

```bash
podman logs -f agent-inbox-<task-id>
```

### Kill a sandbox

```bash
agent-inbox kill <task-id>
```

Stops the container and marks the task as exited.

### Resume after reboot

Run automatically on login via systemd, or manually:

```bash
agent-sandbox --resume
# or
agent-inbox resume --all
```

---

## Monitoring

```bash
# Show running tasks (default)
agent-inbox

# List all tasks
agent-inbox list --all

# Filter by status
agent-inbox list --status running
agent-inbox list --status completed
agent-inbox list --status exited

# Live dashboard (refreshes every 2s)
agent-inbox watch

# Detailed view of one task
agent-inbox show <task-id>

# Clean up completed/exited tasks
agent-inbox clear-all
agent-inbox reset --force   # wipe everything
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

This installs a systemd user service (`agent-inbox-listen.service`) that long-polls Telegram. Every notification has a **↩ Reply** button — tap it, type your message, and it gets injected into the agent inside its container.

```bash
# Daemon management
systemctl --user status agent-inbox-listen
journalctl --user -u agent-inbox-listen -f
```

You can also inject directly from the terminal (bypasses Telegram):

```bash
agent-inbox inject <task-id> "Yes, proceed with the migration"
```

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

## Scripts Reference

| Script / Command | Purpose |
|-----------------|---------|
| `install.sh` | One-command install / upgrade |
| `install.sh --telegram` | Also set up Telegram bot |
| `install.sh --listen` | Also start reply listener daemon |
| `agent-sandbox <repo>` | Start a sandboxed agent |
| `agent-sandbox list` | List open sandboxes |
| `agent-sandbox attach <id>` | Attach interactive shell to sandbox |
| `agent-sandbox --resume` | Resume agents after reboot |
| `agent-sandbox --build-only` | Rebuild sandbox image |
| `scripts/setup-telegram.sh` | Interactive Telegram bot setup |
| `scripts/setup-claude-hooks.sh` | Install Claude Code hooks |
| `scripts/setup-listen.sh` | Install reply listener systemd service |

---

## Development

```bash
cargo test          # run all tests (58 unit tests)
cargo build         # dev build
cargo build --release
```

### Project structure

```
src/
├── main.rs                  # CLI entry point, all command handlers
├── cli/mod.rs               # clap argument definitions
├── models/task.rs           # Task, TaskStatus, SandboxType, SandboxConfig
├── db/mod.rs                # SQLite operations, schema migrations (v3)
├── sandbox/
│   ├── mod.rs               # Sandbox trait, ContainerInfo, helpers
│   └── podman.rs            # Podman implementation + Dockerfile + input-forwarder script
├── agent_input.rs           # Input injection (container pipe or host PTY)
├── notifications/
│   ├── telegram.rs          # Send Telegram messages
│   └── telegram_listener.rs # Long-polling daemon, reply routing
├── config.rs                # TOML config loader
├── display/mod.rs           # Terminal task list rendering
└── monitor/mod.rs           # Process liveness monitoring
```

## License

Apache 2.0
