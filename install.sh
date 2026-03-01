#!/bin/bash
# install.sh — build and install agent-inbox end-to-end
#
# Usage:
#   ./install.sh             # full install / upgrade
#   ./install.sh --telegram  # also (re)run Telegram bot setup

set -e

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$HOME/.local/bin"
WRAPPERS_DIR="$HOME/.agent-tasks/wrappers"
CLAUDE_SETTINGS="$HOME/.claude/settings.json"

step() { echo -e "\n${BOLD}▶ $1${NC}"; }
ok()   { echo -e "  ${GREEN}✓${NC} $1"; }
warn() { echo -e "  ${YELLOW}!${NC} $1"; }
die()  { echo -e "  ${RED}✗${NC} $1" >&2; exit 1; }

# ── Parse flags ───────────────────────────────────────────────────────────────
RUN_TELEGRAM=false
RUN_LISTEN=false
for arg in "$@"; do
    case "$arg" in
        --telegram) RUN_TELEGRAM=true ;;
        --listen)   RUN_LISTEN=true ;;
        *) die "Unknown argument: $arg" ;;
    esac
done

echo -e "${BOLD}=== Agent Inbox — Install / Upgrade ===${NC}"

# ── 1. Prerequisites ──────────────────────────────────────────────────────────
step "Checking prerequisites"

command -v cargo >/dev/null 2>&1 || die "cargo not found. Install Rust from https://rustup.rs"
ok "cargo ($(cargo --version))"

command -v jq >/dev/null 2>&1 \
    && ok "jq found" \
    || warn "jq not found — hooks will not extract message bodies. Install jq for full functionality."

# ── Podman (required for sandbox) ─────────────────────────────────────────────
if command -v podman >/dev/null 2>&1; then
    ok "podman ($(podman --version))"
else
    warn "podman not found — attempting to install…"
    if command -v apt-get >/dev/null 2>&1; then
        sudo apt-get update -qq && sudo apt-get install -y podman \
            || die "Failed to install podman via apt-get. Install manually: https://podman.io/docs/installation"
    elif command -v dnf >/dev/null 2>&1; then
        sudo dnf install -y podman \
            || die "Failed to install podman via dnf."
    elif command -v pacman >/dev/null 2>&1; then
        sudo pacman -S --noconfirm podman \
            || die "Failed to install podman via pacman."
    elif command -v brew >/dev/null 2>&1; then
        brew install podman \
            || die "Failed to install podman via brew."
    else
        die "Cannot auto-install podman. Visit https://podman.io/docs/installation"
    fi
    ok "podman installed ($(podman --version))"
fi

# Warn if podman is not in rootless mode (not a hard failure)
PODMAN_ROOTLESS=$(podman info --format '{{.Host.Security.Rootless}}' 2>/dev/null || echo "unknown")
if [ "$PODMAN_ROOTLESS" = "true" ]; then
    ok "podman running rootless (good)"
else
    warn "podman is NOT rootless. For better security configure rootless mode:"
    warn "  https://github.com/containers/podman/blob/main/docs/tutorials/rootless_tutorial.md"
fi

mkdir -p "$BIN_DIR" "$WRAPPERS_DIR"

# ── 2. Build ──────────────────────────────────────────────────────────────────
step "Building release binaries"

cargo build --release --manifest-path "$REPO_DIR/Cargo.toml" \
    || die "Cargo build failed"

ok "Build succeeded (host binary)"

# Also build a statically linked musl binary for use inside sandbox containers.
# The host binary is linked against the host glibc which may not be available
# inside the container (e.g. Arch host vs Debian container).
if command -v musl-gcc >/dev/null 2>&1; then
    RUSTFLAGS="-C target-feature=+crt-static" \
        cargo build --release \
            --manifest-path "$REPO_DIR/Cargo.toml" \
            --target x86_64-unknown-linux-musl \
        && cp "$REPO_DIR/target/x86_64-unknown-linux-musl/release/agent-inbox" \
              "$BIN_DIR/agent-inbox-musl" \
        && chmod +x "$BIN_DIR/agent-inbox-musl" \
        && ok "agent-inbox-musl (static, for containers)" \
        || warn "musl build failed — container hooks won't send Telegram notifications"
else
    warn "musl-gcc not found — skipping static build (install: sudo pacman -S musl)"
    warn "Container hooks won't send Telegram notifications until this is built."
    warn "After installing musl, re-run: ./install.sh"
fi

# ── 3. Install binaries ───────────────────────────────────────────────────────
step "Installing binaries to $BIN_DIR"

# Stop the listener service before overwriting the binary (avoids "Text file busy").
LISTENER_WAS_ACTIVE=false
if systemctl --user is-active --quiet agent-inbox-listen.service 2>/dev/null; then
    LISTENER_WAS_ACTIVE=true
    systemctl --user stop agent-inbox-listen.service
    ok "Stopped agent-inbox-listen.service for upgrade"
fi

cp "$REPO_DIR/target/release/agent-inbox" "$BIN_DIR/agent-inbox"
chmod +x "$BIN_DIR/agent-inbox"
ok "agent-inbox"

cp "$REPO_DIR/target/release/agent-bridge" "$BIN_DIR/agent-bridge"
chmod +x "$BIN_DIR/agent-bridge"
ok "agent-bridge"

# Restart the listener if it was running before.
if [ "$LISTENER_WAS_ACTIVE" = true ]; then
    systemctl --user start agent-inbox-listen.service
    ok "Restarted agent-inbox-listen.service"
fi

# Warn if BIN_DIR is not on PATH
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$BIN_DIR"; then
    warn "$BIN_DIR is not in your PATH. Add to your ~/.zshrc or ~/.bashrc:"
    warn "  export PATH=\"\$HOME/.local/bin:\$PATH\""
fi

# ── 4. Install wrappers ───────────────────────────────────────────────────────
step "Installing wrappers to $WRAPPERS_DIR"

cp "$REPO_DIR/wrappers/claude-wrapper"   "$WRAPPERS_DIR/claude-wrapper"
cp "$REPO_DIR/wrappers/opencode-wrapper" "$WRAPPERS_DIR/opencode-wrapper"
chmod +x "$WRAPPERS_DIR/claude-wrapper" "$WRAPPERS_DIR/opencode-wrapper"
ok "claude-wrapper"
ok "opencode-wrapper"

# Check shell aliases
SHELL_RC=""
[ -f "$HOME/.zshrc" ]  && SHELL_RC="$HOME/.zshrc"
[ -f "$HOME/.bashrc" ] && [ -z "$SHELL_RC" ] && SHELL_RC="$HOME/.bashrc"

if [ -n "$SHELL_RC" ]; then
    MISSING_ALIASES=()
    grep -q "agent-tasks/wrappers/claude-wrapper"   "$SHELL_RC" 2>/dev/null || MISSING_ALIASES+=("claude")
    grep -q "agent-tasks/wrappers/opencode-wrapper" "$SHELL_RC" 2>/dev/null || MISSING_ALIASES+=("opencode")

    if [ ${#MISSING_ALIASES[@]} -eq 0 ]; then
        ok "Shell aliases already in $SHELL_RC"
    else
        warn "Add these aliases to $SHELL_RC and reload your shell:"
        for name in "${MISSING_ALIASES[@]}"; do
            warn "  alias ${name}='$WRAPPERS_DIR/${name}-wrapper'"
        done
        warn "  source $SHELL_RC"
    fi
else
    warn "Could not detect shell RC. Add aliases manually:"
    warn "  alias claude='$WRAPPERS_DIR/claude-wrapper'"
    warn "  alias opencode='$WRAPPERS_DIR/opencode-wrapper'"
fi

# ── 5. Sandbox image ─────────────────────────────────────────────────────────
step "Building sandbox image (node:20-slim + claude-code)"

if agent-inbox sandbox --build-only 2>/dev/null; then
    ok "Sandbox image ready: agent-inbox-sandbox:latest"
else
    warn "Sandbox image build failed or agent-inbox not yet in PATH."
    warn "Run after install:  agent-inbox sandbox --build-only"
fi

# ── 5a. Install agent-sandbox convenience script ──────────────────────────────
cat > "$BIN_DIR/agent-sandbox" << 'SCRIPT'
#!/bin/bash
# agent-sandbox — manage sandboxed Claude Code agents
#
# Claude runs inside a rootless Podman container with:
#   - A persistent tmux session named 'claude' (attach/detach without losing work)
#   - Full read/write access to the repo at /workspace
#   - Host network so ports 3000-3100 and 8000-8100 are accessible on the host
#   - ANTHROPIC_API_KEY and ANTHROPIC_BASE_URL forwarded from the host environment
#   - ~/.claude mounted so credentials are shared with the host Claude instance
#   - Input injection via 'tmux send-keys' for Telegram replies

set -e

AGENT_INBOX="agent-inbox"
IMAGE="agent-inbox-sandbox:latest"

usage() {
    cat <<'EOF'
agent-sandbox — run Claude Code agents in isolated Podman containers

USAGE
  agent-sandbox <repo_path> [task_description]   Spawn a new agent
  agent-sandbox <command> [args]

COMMANDS
  <repo_path>              Spawn a sandboxed Claude Code agent for the repo.
                           Claude starts automatically with --dangerously-skip-permissions.
                           The most recent Claude session for that repo is continued
                           automatically (host or sandbox sessions, they share ~/.claude).
                           Optionally pass a task description as a second argument.
  <repo_path> --fresh      Spawn a sandbox with a brand-new Claude session.
  <repo_path> --session-id <id>  Spawn a sandbox resuming a specific session.

  attach <task-id>             Attach to Claude inside a running sandbox.
                               Resumes the session stored at spawn time automatically.
  attach <task-id> --fresh     Start a fresh Claude session instead of resuming.
  attach <task-id> --kimi      Use Kimi as the LLM backend for this session
                               (reads KIMI_BASE_URL and KIMI_API_KEY from env).

  list               List all sandboxes and their status.

  inject <task-id> <message>   Inject a message into the attached Claude session.
  kill <task-id>               Stop a running sandbox and remove it.
  kill --all                   Stop and remove all running sandboxes.

  resume             Re-sync sandbox state after a host reboot.
                     (Also runs automatically via systemd on login.)

  --build-only       Build or refresh the sandbox base image without starting an agent.

  --rebuild          Force a clean rebuild of the sandbox base image.

  --help, -h         Show this help message.

EXAMPLES
  agent-sandbox ~/projects/myapp
  agent-sandbox ~/projects/myapp "Fix the login bug"
  agent-sandbox ~/projects/myapp --fresh
  agent-sandbox ~/projects/myapp --session-id abc12345
  agent-sandbox list
  agent-sandbox attach a1b2c3
  agent-sandbox attach a1b2c3 --fresh
  agent-sandbox attach a1b2c3 --kimi
  agent-sandbox kill a1b2c3
  agent-sandbox resume
  agent-sandbox --build-only
  agent-sandbox --rebuild

WORKFLOW
  1. Start a sandbox:    agent-sandbox ~/projects/myapp
  2. Attach to Claude:   agent-sandbox attach <task-id>
  3. Detach safely:      Ctrl+X D
  4. Re-attach later:    agent-sandbox attach <task-id>
  5. Done:               agent-sandbox kill <task-id>

TELEGRAM INJECTION
  Replies sent from Telegram are injected via 'tmux send-keys' inside the container.
  Start the listener:    agent-inbox listen
EOF
}

require_task_id() {
    local cmd="$1"
    local task_id="${2:-}"
    if [ -z "$task_id" ]; then
        echo "Error: $cmd requires a task-id" >&2
        echo "Usage: agent-sandbox $cmd <task-id>" >&2
        echo "       agent-sandbox list    # to see task IDs" >&2
        exit 1
    fi
    echo "$task_id"
}

# ── dispatch ──────────────────────────────────────────────────────────────────

if [ $# -eq 0 ] || [ "$1" = "--help" ] || [ "$1" = "-h" ]; then
    usage
    exit 0
fi

if [ "$1" = "--build-only" ]; then
    exec "$AGENT_INBOX" _sandbox_build
fi

if [ "$1" = "--rebuild" ]; then
    exec "$AGENT_INBOX" _sandbox_build --rebuild
fi

if [ "$1" = "--resume" ] || [ "$1" = "resume" ]; then
    exec "$AGENT_INBOX" _sandbox_resume --all
fi

if [ "$1" = "--list" ] || [ "$1" = "list" ]; then
    exec "$AGENT_INBOX" _sandbox_list
fi

if [ "$1" = "attach" ]; then
    TASK_ID=$(require_task_id attach "${2:-}")
    shift 2
    ATTACH_FLAGS=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --fresh) ATTACH_FLAGS="$ATTACH_FLAGS --fresh"; shift ;;
            --kimi)  ATTACH_FLAGS="$ATTACH_FLAGS --kimi";  shift ;;
            *) shift ;;
        esac
    done
    # shellcheck disable=SC2086
    exec "$AGENT_INBOX" _sandbox_attach "$TASK_ID" $ATTACH_FLAGS
fi

if [ "$1" = "inject" ]; then
    TASK_ID=$(require_task_id inject "${2:-}")
    MESSAGE="${3:-}"
    if [ -z "$MESSAGE" ]; then
        echo "Error: inject requires a message" >&2
        echo "Usage: agent-sandbox inject <task-id> <message>" >&2
        exit 1
    fi
    exec "$AGENT_INBOX" inject "$TASK_ID" "$MESSAGE"
fi

if [ "$1" = "kill" ]; then
    if [ "${2:-}" = "--all" ]; then
        exec "$AGENT_INBOX" _sandbox_kill --all
    fi
    TASK_ID=$(require_task_id kill "${2:-}")
    exec "$AGENT_INBOX" _sandbox_kill "$TASK_ID"
fi

# Default: spawn a new sandbox for the given repo path.
# Supported flags: --fresh, --session-id <id>, and a trailing task description.
REPO_PATH="$1"
shift
TASK_DESC=""
FRESH_FLAG=""
SESSION_ID_FLAG=""

while [ $# -gt 0 ]; do
    case "$1" in
        --fresh)
            FRESH_FLAG="--fresh"
            shift
            ;;
        --session-id)
            SESSION_ID_FLAG="--session-id $2"
            shift 2
            ;;
        *)
            TASK_DESC="$1"
            shift
            ;;
    esac
done

SPAWN_ARGS="$REPO_PATH"
[ -n "$TASK_DESC" ]       && SPAWN_ARGS="$SPAWN_ARGS --task $TASK_DESC"
[ -n "$FRESH_FLAG" ]      && SPAWN_ARGS="$SPAWN_ARGS $FRESH_FLAG"
[ -n "$SESSION_ID_FLAG" ] && SPAWN_ARGS="$SPAWN_ARGS $SESSION_ID_FLAG"

# shellcheck disable=SC2086
exec "$AGENT_INBOX" _sandbox_spawn $SPAWN_ARGS
SCRIPT
chmod +x "$BIN_DIR/agent-sandbox"
ok "agent-sandbox convenience script installed"

# ── 5b. Install systemd auto-resume service ───────────────────────────────────
SYSTEMD_DIR="$HOME/.config/systemd/user"
mkdir -p "$SYSTEMD_DIR"
cat > "$SYSTEMD_DIR/agent-inbox-resume.service" << UNIT
[Unit]
Description=Agent Inbox — resume sandbox agents after reboot
After=default.target

[Service]
Type=oneshot
ExecStart=$BIN_DIR/agent-inbox resume --all
RemainAfterExit=yes

[Install]
WantedBy=default.target
UNIT

if systemctl --user daemon-reload 2>/dev/null; then
    systemctl --user enable agent-inbox-resume.service 2>/dev/null \
        && ok "Auto-resume service enabled (agent-inbox-resume.service)" \
        || warn "Could not enable auto-resume service. Enable manually: systemctl --user enable agent-inbox-resume.service"
else
    warn "systemd user session not available. Auto-resume on reboot won't work."
fi

# ── 6. Claude Code hooks ──────────────────────────────────────────────────────
step "Installing Claude Code hooks"

mkdir -p "$HOME/.claude"

# If existing hooks are from a previous agent-inbox install, remove them first
# so setup-claude-hooks.sh writes the latest version (it skips if AGENT_TASK_ID
# is already present).
if grep -q "AGENT_TASK_ID" "$CLAUDE_SETTINGS" 2>/dev/null; then
    if command -v jq >/dev/null 2>&1; then
        jq 'del(.hooks)' "$CLAUDE_SETTINGS" > "$CLAUDE_SETTINGS.tmp" \
            && mv "$CLAUDE_SETTINGS.tmp" "$CLAUDE_SETTINGS"
        ok "Removed stale hooks from existing settings"
    else
        warn "jq not available — cannot auto-update existing hooks."
        warn "Manually remove the \"hooks\" block from $CLAUDE_SETTINGS then re-run."
    fi
fi

bash "$REPO_DIR/scripts/setup-claude-hooks.sh"

# ── 6. Telegram (optional) ────────────────────────────────────────────────────
if [ "$RUN_TELEGRAM" = true ]; then
    step "Setting up Telegram notifications"
    bash "$REPO_DIR/scripts/setup-telegram.sh"
else
    CONFIG_FILE="$HOME/.agent-tasks/config.toml"
    if grep -q "enabled = true" "$CONFIG_FILE" 2>/dev/null; then
        ok "Telegram already configured ($CONFIG_FILE)"
    else
        echo ""
        warn "Telegram not configured. Run when ready:"
        warn "  ./install.sh --telegram"
    fi
fi

# ── 7. Telegram listener daemon (optional) ────────────────────────────────────
if [ "$RUN_LISTEN" = true ]; then
    step "Setting up Telegram reply listener (systemd service)"
    bash "$REPO_DIR/scripts/setup-listen.sh"
else
    # Offer the hint only when Telegram is already configured but listener isn't running.
    CONFIG_FILE="$HOME/.agent-tasks/config.toml"
    if grep -q "enabled = true" "$CONFIG_FILE" 2>/dev/null; then
        if ! systemctl --user is-active --quiet agent-inbox-listen.service 2>/dev/null; then
            echo ""
            warn "Telegram reply listener not running. Enable with:"
            warn "  ./install.sh --listen"
        else
            ok "Telegram reply listener already running"
        fi
    fi
fi

# ── 8. Done ───────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}${GREEN}Done!${NC} Restart Claude Code for hooks to take effect."
echo ""
echo "  Verify:      agent-inbox --help"
echo "  Test notify: agent-inbox notify --message 'install test' --attention"
echo ""
echo -e "${BOLD}Sandbox usage:${NC}"
echo "  Start agent:  agent-sandbox /path/to/repo"
echo "  List agents:  agent-inbox list"
echo "  Watch agents: agent-inbox watch"
echo "  Kill agent:   agent-inbox kill <task-id>"
echo "  Attach shell: podman exec -it agent-inbox-<task-id> bash"
echo ""
