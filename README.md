# Nibble

> *In Futurama, Nibbler's species is tasked with protecting the universe from giant flying brains. Nibble is your orchestrator — keeping watch over the smaller agents so you don't have to.*

A CLI tool that runs Claude Code agents inside isolated Podman sandboxes, monitors their status, sends Telegram notifications when they need your attention, and lets you reply from your phone to unblock them — all without touching your keyboard.

## Features

- **Podman Sandboxes**: Run agents in rootless containers — repo mounted read-write, ports exposed, full dev flexibility inside
- **Setup Scripts**: Drop a `.nibble/setup.sh` in any repo to auto-install its toolchain and dependencies at spawn time
- **Persistent Session Continuity**: Every repo gets a stable session UUID — re-attaching and Telegram replies always resume the same conversation
- **Session GC**: Clean up old Claude conversation history with `nibble sandbox gc`
- **Telegram Notifications**: Receive the agent's last message on your phone when it finishes or needs a decision
- **Telegram Replies**: Reply to notifications from your phone to unblock agents (input injected via `podman exec`)
- **Auto-Resume**: Sandbox agents are tracked across host reboots
- **Cron Jobs**: Schedule prompts to run automatically inside sandboxes
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
6. Installs the Claude Code status line script (see [Status Line](#status-line))
7. Builds the sandbox image (`nibble-sandbox:latest` — node:20-slim + claude-code)
8. Enables `nibble-resume.service` (systemd user service, resumes agents on reboot)
9. Optionally sets up Telegram and the reply listener daemon

---

## Sandbox Usage

### Start an agent

```bash
# Spawn a sandbox and attach immediately
nibble sandbox spawn /path/to/repo
nibble sandbox spawn /path/to/repo --task "Fix the authentication bug"

# Spawn without attaching (run in background)
nibble sandbox spawn /path/to/repo --task "Fix the authentication bug"

```

On first run the sandbox image is built (~2-3 min). Subsequent spawns are fast.
To rebuild the image (e.g. after upgrading nibble): `./install.sh --rebuild`

The container gets:
- Your repo mounted read-write at `/workspace`
- `ANTHROPIC_API_KEY` and other configured env vars forwarded from host
- Ports forwarded via host network (services on `:3000`, `:8080`, etc. are reachable from outside)
- Full privileged mode inside (install anything with `apt`, `npm`, `pip`, etc.)
- Dependency caches persisted across container restarts via host-mounted volumes:
  - `~/.npm`, `~/.npm-global` (Node)
  - `~/.cargo/registry`, `~/.cargo/git`, `~/.rustup` (Rust)

### Pre-installing dependencies with `.nibble/setup.sh`

By default the sandbox image is a bare node+claude environment. To have your project's toolchain and dependencies ready **before the first attach**, add a setup script to your repo:

```bash
mkdir -p .nibble
cat > .nibble/setup.sh << 'EOF'
#!/usr/bin/env bash
set -euo pipefail

# Install build tools if missing
if ! command -v cc &>/dev/null; then
    sudo apt-get update -qq && sudo apt-get install -y -qq build-essential
fi

# Install your language toolchain and deps here, e.g. for Rust:
export PATH="$HOME/.cargo/bin:$PATH"
if ! command -v rustup &>/dev/null; then
    curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --no-modify-path
fi
cd /workspace && cargo build
EOF
chmod +x .nibble/setup.sh
```

When nibble spawns a sandbox and finds `.nibble/setup.sh`, it runs the script inside the container (blocking, output streamed to your terminal) before handing off to Claude. By the time you attach, the toolchain is installed and the project is compiled.

The script runs once per container lifetime (at spawn). Because cargo/npm/rustup caches are bind-mounted from the host, subsequent spawns skip downloads and complete in seconds.

If no setup script is found, nibble prints a reminder:
```
Setup: ⚠️  No .nibble/setup.sh found — dependencies won't be pre-installed.
       Create .nibble/setup.sh in the repo to auto-install deps on spawn.
       (Ask Claude to write it for you once inside the sandbox.)
```

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

## Status Line

nibble installs a Claude Code status line that shows live context and quota information directly in your terminal.

```
📁 ~/projects/myapp   main  🤖 claude-sonnet-4-5  │ ctx ████████ 92%  │ 5h ██████░░ 74% ↺14:30  │ 7d ████░░░░ 48% ↺Thu 09:00
```

**Sections:**

| Section | Description |
|---------|-------------|
| `📁 dir` | Current working directory (tilde-shortened) |
| ` branch` | Git branch name (yellow) |
| `🤖 model` | Active Claude model (magenta) |
| `ctx ████` | Context window remaining — bar turns orange <50%, red <20% |
| `5h ████` | 5-hour rate limit remaining, with reset time |
| `7d ████` | 7-day rate limit remaining, with reset time |

**Install behaviour:**

- The script is always copied to `~/.claude/statusline-command.sh` on each `./install.sh` run (safe to upgrade)
- The `statusLine` key is added to `~/.claude/settings.json` only if one is **not already present** — existing custom status lines are left untouched
- To opt out: remove or replace the `statusLine` key in `~/.claude/settings.json` after install; future installs will not overwrite it

**Customise:** edit `~/.claude/statusline-command.sh` directly. The script reads Claude's JSON context on stdin and outputs ANSI-coloured text.

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

This installs a systemd user service (`nibble-listener.service`) that long-polls Telegram. Every notification has a **↩ Reply** button — tap it, type your message, and it gets injected into the agent inside its container.

```bash
# Daemon management
systemctl --user status nibble-listener
journalctl --user -u nibble-listener -f
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

## Troubleshooting & Logging

### Checking listener logs

The listener logs all activity to stderr with a `[listen]` prefix. When running as a systemd service, view logs with:

```bash
# Follow live logs
journalctl --user -u nibble-listener -f

# Last 5 minutes
journalctl --user -u nibble-listener --since '5 min ago'
```

### Log lines and what they mean

| Log line | Meaning |
|----------|---------|
| `[listen] Poll ok: N update(s)` | Listener is alive and received N Telegram updates. If you don't see this, the listener is stuck or not running. |
| `[listen] Command menu registered` | Bot commands (/help, /sandboxes, etc.) registered with Telegram on startup. |
| `[listen] handle_update type=callback_query` | User tapped an inline button (e.g. ↩ Reply). |
| `[listen] Callback from user ...: data="reply:..."` | Reply button tapped; routing data extracted. |
| `[listen] Pending reply set: chat=... task=...` | `pending_reply` stored in DB — user's next message will route to this task. |
| `[listen] Routing via pending reply to task ...` | User's message matched a pending reply and is being injected. |
| `[listen] No pending reply for chat=...` | User sent a message but no pending reply was set (no button tap, or already consumed). |
| `[listen] Task not found: ...` | The task_id from routing doesn't exist in the DB. |
| `[listen] getUpdates error: ... 409 ...` | **Another listener instance is running.** See below. |
| `[listen] Running periodic prune…` / `Prune done` | Periodic health check of all containers. If "Prune done" never appears, a `podman inspect`/`exec` is hanging. |
| `[sandboxes] checking N container state entries` | `/sandboxes` is evaluating N containers from the DB. |
| `[sandboxes] container=... health=...` | Health check result for each container. |

### Common issues

#### HTTP 409 Conflict

```
[listen] getUpdates error: ... status code 409
```

Telegram only allows one `getUpdates` connection per bot token. A 409 means another process is already polling. This typically happens when:

1. You started `nibble listen` manually while the systemd service is also running
2. An agent inside a sandbox container accidentally started `nibble listen` (the binary is bind-mounted into containers)

**Fix:**

```bash
# Kill all listener instances
systemctl --user stop nibble-listener
pkill -9 -f 'nibble'

# Verify nothing is running
ps aux | grep 'nibble listen'

# Wait for Telegram's long-poll to expire (30s timeout)
sleep 35

# Reset Telegram's polling state
curl -s "https://api.telegram.org/bot<BOT_TOKEN>/getUpdates?offset=-1" | head -c 200

# Start exactly one instance
systemctl --user start nibble-listener
```

**Prevention:** nibble refuses to run `listen` inside a sandbox container (detected via `AGENT_TASK_ID` env var).

#### /sandboxes shows nothing

- Check logs for `[sandboxes]` lines — if containers show `health=Dead`, Podman may not be running or the container was removed
- If containers show `health=Stopped`, they will be auto-restarted; wait 2-3 seconds
- If no `[sandboxes]` lines appear at all, the listener isn't processing the command (see 409 above)

#### Reply doesn't route

1. Check that `[listen] Pending reply set` appears after tapping the reply button
2. Check that `[listen] Routing via pending reply` or `[listen] No pending reply` appears after sending your message
3. If "No pending reply" — the `pending_reply` was consumed by a different message, or the DB write failed

#### Listener gets stuck (no logs, no responses)

A `podman inspect` or `podman exec` command may be hanging (rare, but happens with certain Podman states). The listener is single-threaded — a hanging subprocess blocks the entire loop. Restart the listener:

```bash
systemctl --user restart nibble-listener
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

Every repo gets a **deterministic session UUID** derived from its canonical path. This means:

- Re-attaching to the same repo always resumes the same conversation — no matter how many times you detach and re-attach
- Telegram replies land in the same session as your interactive terminal session
- Container restarts after a reboot resume the same history

Session history is stored in `~/.claude/projects/<hash>/<uuid>.jsonl` on the host (mounted into the container), so it survives container recreation.

#### Starting fresh

```bash
# Back up the current session history and start a new conversation
nibble sandbox attach . --fresh
```

`--fresh` renames the current `.jsonl` to `.jsonl.bak` (preserving it for recovery) and starts Claude with a blank slate. The session UUID stays the same so Telegram injection keeps working without any DB changes.

#### Cleaning up old session history

Claude conversation files accumulate over time and can grow large. Use `gc` to clean them up:

```bash
# Delete old sessions and backups, keep the most recent session
nibble sandbox gc .
nibble sandbox gc <task-id>

# Wipe all sessions including the current one
nibble sandbox gc . --all
```

The GC command finds the right `~/.claude/projects/` subdirectory for the repo (by matching the known session UUID, or by scanning file contents as a fallback), then deletes all `.jsonl.bak` backup files and all `.jsonl` session files except the most recent active one. It reports how many files were removed and how much disk was freed.

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
| `nibble sandbox gc <id>` | Delete old session history, keep latest |
| `nibble sandbox gc <id> --all` | Wipe all session history |
| `./install.sh --rebuild` | Rebuild sandbox image |
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
