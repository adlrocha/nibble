# Nibble

> *In Futurama, Nibbler's species is tasked with protecting the universe from giant flying brains. Nibble is your orchestrator — keeping watch over the smaller agents so you don't have to.*

A CLI tool that runs Claude Code agents inside isolated Podman sandboxes and tracks their status — persistent sessions, unified task tracking, and a web session inspector.

---

## Feature Inventory

This section lists every feature area in the project. Use it to audit what's worth keeping.

| # | Feature | Status | Description |
|---|---------|--------|-------------|
| 1 | **Podman Sandboxes** | core | Per-repo rootless containers; repo mounted RW; `sleep infinity` PID 1; `podman exec` for attach |
| 2 | **Session Continuity** | core | Deterministic session UUID per repo; resume across detach/reboot; `--fresh` to start over |
| 3 | **Task DB** | core | SQLite backend tracking all tasks (sandboxed + non-sandboxed), states (running/completed/exited), session IDs |
| 4 | **Install script** | core | `install.sh` — builds binary, installs Podman if absent, builds sandbox image, wires Claude hooks |
| 5 | **Claude Code hooks** | core | Stop hook → `nibble report session-id` + memory capture; wrappers register tasks at startup |
| 6 | **Setup scripts** | dx | `.nibble/setup.sh` in any repo — auto-runs at spawn to install toolchain before first attach |
| 7 | **Git worktrees** | dx | `--branch` flag on spawn/attach/kill — creates/cleans up a worktree automatically per branch |
| 8 | **`--btw` sessions** | dx | `attach --btw` — side session that doesn't overwrite which session the main attach would continue (ad-hoc research, parallel work); kept on disk like any other |
| 9 | **Session retention** | core | Sessions are never deleted — `--fresh` only renames the current file to `.jsonl.bak`; Claude's own 30-day purge is disabled via `cleanupPeriodDays` |
| 10 | **Hermes Agent** | experimental | Singleton sandbox where you mount/unmount repos dynamically; `hermes gateway` as PID 1 |
| 11 | **Status line** | dx | Claude Code terminal status bar showing dir, branch, model, context %, 5h and 7d rate limit bars |
| 12 | **Health checks** | ops | `SandboxHealth` enum (Healthy/Degraded/Dead); `nibble prune` marks stale tasks exited and GCs old records |
| 13 | **Auto-resume on reboot** | ops | systemd user service (`nibble-resume.service`) restarts containers after host reboot |
| 14 | **Web session inspector** | dx | `nibble web` — dark-mode browser UI (port 7878) for browsing/searching pi sessions, usage dashboard, conversation viewer; runs as `nibble-web.service`, Tailscale-reachable with token auth. See [docs/web.md](docs/web.md) |
| 15 | **omp (oh-my-pi) support** | core | `--pi` runs upstream pi, `--omp` runs omp (oh-my-pi) — explicit, independent flags. Sandboxes mount both `~/.pi` and `~/.omp`; sessions are format-compatible and cross-resumable. `scripts/migrate-pi-to-omp.sh` migrates host config |
| 16 | **Session recovery** | core | Eager task→session mapping via extension-reported `session-path`; interactive picker when attach finds multiple sessions for a repo; `session list` shows task links; runbook in [docs/session-recovery.md](docs/session-recovery.md) |
| 17 | **Agent status panel** | core | `nibble status` live table (blocked/running/idle/exited with attention reasons) fed by `nibble report status` hooks; `nibble sidebar --install` gives every zellij tab an auto-refreshing status pane; `1`-`9` or `nibble goto` jumps focus to an agent's pane. Claude hooks (SessionStart → registered/running, Notification → blocked, Stop → idle, SessionEnd → exited) and the pi/omp extension (agent_start/settled/shutdown) report transitions. Every sandbox attach (including `--btw`) is its own tracked window task — concurrent windows on one sandbox never share or flap a row; the sandbox row mirrors the container. Status reports for unregistered IDs self-heal a placeholder row, so a lost `report start` can't make a session invisible. Launches without a wrapper (`claude -p`/`omp -p` in scripts, bare or `nibble sandbox bash` shells) are tracked too via a stable `claude-<session_id>` / `omp-<session_id>` task ID derived from the agent's own session — every session, host or sandbox, gets a row |

---

## Features

- **Podman Sandboxes**: Run agents in rootless containers — repo mounted read-write, ports exposed, full dev flexibility inside
- **Setup Scripts**: Drop a `.nibble/setup.sh` in any repo to auto-install its toolchain and dependencies at spawn time
- **Persistent Session Continuity**: Every repo gets a stable session UUID — re-attaching always resumes the same conversation
- **Sessions are never deleted**: every conversation (main, `--fresh`, `--btw`) stays on disk forever; Claude's built-in 30-day transcript purge is disabled
- **Auto-Resume**: Sandbox agents are tracked across host reboots
- **Unified Task Tracking**: Track sandboxed and non-sandboxed agents in one dashboard
- **3-State Model**: Running → Completed → Exited
- **SQLite Backend**: Fast, reliable, concurrent-safe storage

## How it Works

```
┌─────────────────────────────────────────────────────────────────────┐
│                          HOST SYSTEM                                │
│                                                                     │
│  nibble CLI               SQLite DB                                 │
│  ─────────                ─────────                                 │
│  sandbox / kill           tasks.db                                  │
│  list / watch             container_state                           │
│  prune                    session_id                                │
│         │                                                           │
│         │ podman run                                                │
│         ▼                                                           │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │  Podman Container                                            │   │
│  │                                                              │   │
│  │  claude --resume <id>  ←── podman exec (attach)             │   │
│  │                                                              │   │
│  │  Stop hook → nibble report session-id                       │   │
│  │  /workspace  (repo mounted RW)                              │   │
│  └──────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
```

**Hook flow**: Claude Code `Stop` hook → `nibble report session-id` + memory capture → async session summarization

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
```

After install, add aliases to your shell if prompted:

```bash
# ~/.zshrc or ~/.bashrc
alias claude='~/.agent-tasks/wrappers/claude-wrapper'
```

What `install.sh` does:

1. Installs Podman if not present (apt / dnf / pacman / brew)
2. Builds release binaries with `cargo build --release`
3. Installs `nibble` to `~/.local/bin/`
4. Copies wrappers to `~/.agent-tasks/wrappers/`
5. Installs Claude Code hooks to `~/.claude/settings.json`
6. Installs the Claude Code status line script (see [Status Line](#status-line))
7. Builds the sandbox image (`nibble-sandbox:latest`)
8. Enables `nibble-resume.service` (systemd user service, resumes agents on reboot)

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

# Ad-hoc side session that doesn't touch the main conversation
nibble sandbox attach . --btw
```

Detach with `exit` or `Ctrl+C`.

### What gets injected into your repo

When a sandbox spawns, nibble writes agent instructions into two files inside the repo:

| File | Purpose |
|------|---------|
| `AGENTS.md` | Sandbox environment info and toolchain detection |
| `.claude/CLAUDE.md` | Claude Code entry point — first line is `@../AGENTS.md` which imports the file above |

**How it works:**

- If the file doesn't exist, nibble creates it with just the injected block.
- If the file exists but has no nibble markers, nibble **appends** the block (your content is untouched).
- If the file exists and has nibble markers (`<!-- nibble-sandbox:begin -->` / `<!-- nibble-sandbox:end -->`), nibble **replaces** the marked block in-place.

Your own content outside the sentinel markers is never modified.

**Git impact:** The repo is bind-mounted from the host, so these injected changes are visible to git. If your repo tracks `AGENTS.md`, you'll see a diff. If it doesn't, you'll see a new untracked file. In both cases, these are sandbox-local artifacts — discard them with:

```bash
git checkout -- AGENTS.md          # if your repo tracks AGENTS.md
git clean -fd AGENTS.md .claude/   # if they're untracked
```

A recommended `.gitignore` addition for repos that use sandboxes:

```
.claude/
```

(`AGENTS.md` is not added to `.gitignore` automatically because many projects legitimately track it.)

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

## Hermes Agent

Hermes Agent is a long-running coding agent with a gateway daemon. Unlike per-repo sandboxes, Hermes runs as a **singleton sandbox** where you dynamically mount/unmount the repos you want it to work on.

### Setup

```bash
# Build and install nibble (includes hermes image build on first use)
./install.sh

# Make sure ~/.hermes/ is configured with your LLM provider
hermes setup
```

### Start the Hermes sandbox

```bash
nibble hermes init
```

This spawns a Podman container with `hermes gateway` as PID 1. The gateway stays running in the background, and you can attach/detach from the CLI at any time.

### Mount repos

```bash
# Add a repo (container restarts to apply)
nibble hermes mount /path/to/my-project

# Add with a custom mount name
nibble hermes mount /path/to/my-project --name work-project

# Repos appear inside the container at /repos/<name>
```

Mounting requires a container restart. You'll be prompted to confirm — use `--yes` to skip the prompt:

```bash
nibble hermes mount /path/to/repo --yes
```

### Attach to the Hermes CLI

```bash
nibble hermes attach          # resume last session
nibble hermes attach --fresh  # start a new session
```

Auto-spawns the sandbox if it's not running.

### Unmount a repo

```bash
nibble hermes unmount /path/to/my-project
```

### Check status

```bash
nibble hermes list
```

Shows the sandbox status and all mounted repos.

### Stop the sandbox

```bash
nibble hermes kill
```

The sandbox stops but **mounted repos are preserved** in the database. Running `nibble hermes init` or `nibble hermes attach` again will re-spawn with all previously mounted repos.

### Security model

Only the repos you explicitly mount are accessible inside the container. No home directory is exposed. The Hermes agent runs as non-root (`node` user) with `--network host` to reach a local LLM.

---

## Monitoring

```bash
# Live agent status table (blocked 🔴 / running 🟢 / idle ⚪ / exited ⚫)
nibble status

# Auto-refreshing status — run it in a dedicated (zellij) pane
nibble status --watch

# Open the status side panel in the current zellij tab (ad-hoc)
nibble sidebar

# Install the always-on sidebar: every tab of every new zellij session
# gets a narrow status pane on the right. Never overwrites a custom layout —
# if you already have one it writes layouts/nibble.kdl instead and tells
# you how to adopt it. Remove with: nibble sidebar --uninstall
nibble sidebar --install

# Jump to the zellij pane hosting an agent (task ID or unique prefix)
nibble goto <task>
# Machine-readable output
nibble status --json

# Include recently exited tasks (dimmed)
nibble status --all

# List sandbox containers
nibble sandbox list

# Browse agent sessions (pi/omp/claude)
nibble session list

# Prune stale tasks (dead PIDs / gone containers → mark exited)
nibble prune
```

Status transitions are reported by the Claude Code hooks (UserPromptSubmit /
PostToolUse → running, Notification → blocked with the prompt summary, Stop →
idle, SessionEnd → exited) and by the pi/omp extension events
(agent_start → running, agent_settled → completed, session_shutdown → exited).
A blocked agent shows 🔴 with its attention reason at the top of the table —
that's your "look at me now" signal when steering several agents at once.

The `--watch` view is interactive: pressing `1`-`9` jumps zellij focus to the
pane hosting that agent (pane IDs are recorded by the wrappers at agent start
and by `sandbox attach`; cross-tab jumps work), and `q` quits.

Agent panes are named after their task title automatically (wrappers at
start, `sandbox attach` on re-attach), so the frame border tells you what
each agent is working on.

Requires zellij ≥ 0.42 for pane jumping (`zellij action focus-pane-id`).

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
| `branch` | Git branch name (yellow) |
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

## How it works

### Sandbox model

Each repo gets **one long-lived container**. The container starts with `sleep infinity` as PID 1 and keeps running between sessions. Claude Code is launched transiently via `podman exec` on attach and exits when you detach — the container itself is unaffected.

If you try to spawn a sandbox for a repo that already has one, nibble re-attaches to the existing container instead.

### Session continuity

Every repo gets a **deterministic session UUID** derived from its canonical path. This means:

- Re-attaching to the same repo always resumes the same conversation — no matter how many times you detach and re-attach
- Container restarts after a reboot resume the same history

Session history is stored in `~/.claude/projects/<hash>/<uuid>.jsonl` on the host (mounted into the container), so it survives container recreation.

#### Starting fresh

```bash
# Back up the current session history and start a new conversation
nibble sandbox attach . --fresh
```

`--fresh` renames the current `.jsonl` to `.jsonl.bak` and starts Claude with a blank slate. The backup is never deleted — nibble keeps every session file forever, and Claude Code's own transcript cleanup is pinned to ~100 years (`cleanupPeriodDays` in `~/.claude/settings.json`, set by `scripts/setup-claude-hooks.sh`). The session UUID stays the same so re-attaches keep working without any DB changes.

### Container crash detection

If a container disappears unexpectedly (OOM, host kill, etc.), `nibble prune` detects it and marks the task exited so you can re-spawn.

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
| `nibble status` | Live agent status table (blocked 🔴 / running 🟢 / idle ⚪ / exited ⚫) |
| `nibble status --watch` | Auto-refreshing status (for a dedicated pane) |
| `nibble status --json` | Machine-readable status output |
| `nibble sidebar` | Open the agent-status side panel in the current zellij tab |
| `nibble sidebar --install` | Always-on sidebar: every zellij tab gets a status pane |
| `nibble goto <task>` | Jump zellij focus to the pane hosting an agent |
| `nibble prune` | Mark stale processes as exited |
| `nibble sandbox spawn <repo>` | Start a sandboxed agent |
| `nibble sandbox list` | List open sandboxes |
| `nibble sandbox attach <id>` | Attach to sandbox |
| `nibble sandbox kill <id>` | Stop sandbox |
| `nibble sandbox kill --all` | Stop all sandboxes |
| `nibble sandbox resume --all` | Resume agents after reboot |
| `nibble session list` | Browse/search agent sessions (pi/omp/claude) |
| `nibble hermes init` | Start Hermes Agent sandbox (singleton) |
| `nibble hermes attach` | Attach to Hermes CLI (auto-spawns if needed) |
| `nibble hermes mount <path>` | Mount a repo into the Hermes sandbox |
| `nibble hermes unmount <path>` | Unmount a repo from the Hermes sandbox |
| `nibble hermes list` | Show Hermes sandbox status and mounted repos |
| `nibble hermes kill` | Stop Hermes sandbox (repos preserved) |
| `./install.sh --rebuild` | Rebuild sandbox image |
| `install.sh` | Install / upgrade |

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
├── config.rs                # TOML config loader
├── display/mod.rs           # Terminal task list rendering
└── monitor/mod.rs           # Process liveness monitoring
```

## License

Apache 2.0
