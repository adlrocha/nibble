#!/bin/bash
# install.sh — build and install nibble end-to-end
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
WRAPPERS_DIR="$HOME/.nibble/wrappers"
CLAUDE_SETTINGS="$HOME/.claude/settings.json"

step() { echo -e "\n${BOLD}▶ $1${NC}"; }
ok()   { echo -e "  ${GREEN}✓${NC} $1"; }
warn() { echo -e "  ${YELLOW}!${NC} $1"; }
die()  { echo -e "  ${RED}✗${NC} $1" >&2; exit 1; }

# ── Parse flags ───────────────────────────────────────────────────────────────
RUN_TELEGRAM=false
RUN_LISTEN=false
RUN_LLAMA=false
REBUILD_IMAGE=false
for arg in "$@"; do
    case "$arg" in
        --telegram) RUN_TELEGRAM=true ;;
        --listen)   RUN_LISTEN=true ;;
        --llama)    RUN_LLAMA=true ;;
        --rebuild)  REBUILD_IMAGE=true ;;
        *) die "Unknown argument: $arg" ;;
    esac
done

echo -e "${BOLD}=== Nibble — Install / Upgrade ===${NC}"
echo ""
echo "  Flags: --telegram  set up Telegram notifications"
echo "         --listen    set up Telegram reply listener daemon"
echo "         --llama     set up llama-server systemd service"
echo "         --rebuild   force rebuild the sandbox container image"
echo ""
[ "$REBUILD_IMAGE" = true ] && echo -e "  ${YELLOW}--rebuild${NC}: sandbox image will be rebuilt from scratch"

# ── 1. Prerequisites ──────────────────────────────────────────────────────────
step "Checking prerequisites"

# Source rustup env if cargo is not yet on PATH (common in non-login shells
# inside sandboxes where ~/.cargo/bin isn't added automatically).
if ! command -v cargo >/dev/null 2>&1; then
    for _rustup_env in \
        "$HOME/.cargo/env" \
        "$HOME/.nibble/cache/rustup/env" \
        "$HOME/.rustup/env"
    do
        # shellcheck source=/dev/null
        [ -f "$_rustup_env" ] && source "$_rustup_env" && break
    done
    # Also try well-known toolchain bin paths
    for _cargo_dir in \
        "$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin" \
        "$HOME/.nibble/cache/rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin"
    do
        [ -x "$_cargo_dir/cargo" ] && export PATH="$_cargo_dir:$PATH" && break
    done
fi
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
        && cp "$REPO_DIR/target/x86_64-unknown-linux-musl/release/nibble" \
              "$BIN_DIR/nibble-musl" \
        && chmod +x "$BIN_DIR/nibble-musl" \
        && ok "nibble-musl (static, for containers)" \
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
if systemctl --user is-active --quiet nibble-listener.service 2>/dev/null; then
    LISTENER_WAS_ACTIVE=true
    systemctl --user stop nibble-listener.service
    ok "Stopped nibble-listener.service for upgrade"
fi

cp "$REPO_DIR/target/release/nibble" "$BIN_DIR/nibble"
chmod +x "$BIN_DIR/nibble"
ok "nibble"


# Restart the listener if it was running before.
if [ "$LISTENER_WAS_ACTIVE" = true ]; then
    systemctl --user start nibble-listener.service
    ok "Restarted nibble-listener.service"
fi

# Warn if BIN_DIR is not on PATH
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$BIN_DIR"; then
    warn "$BIN_DIR is not in your PATH. Add to your ~/.zshrc or ~/.bashrc:"
    warn "  export PATH=\"\$HOME/.local/bin:\$PATH\""
fi

# ── 4. Install wrappers ───────────────────────────────────────────────────────
step "Installing wrappers to $WRAPPERS_DIR"

cp "$REPO_DIR/wrappers/claude-wrapper" "$WRAPPERS_DIR/claude-wrapper"
chmod +x "$WRAPPERS_DIR/claude-wrapper"
ok "claude-wrapper"

# ── 4a. Install AI Factory skills ─────────────────────────────────────────────
# Skills are installed to ~/.claude/skills/<name>/SKILL.md — this path is
# discovered automatically by both Claude Code and OpenCode (compat mode).
CLAUDE_SKILLS_DIR="$HOME/.claude/skills"
mkdir -p "$CLAUDE_SKILLS_DIR"

for skill_dir in "$REPO_DIR/skills"/{factory,nibble}-*/; do
    skill_name="$(basename "$skill_dir")"
    dest="$CLAUDE_SKILLS_DIR/$skill_name"
    mkdir -p "$dest"
    cp "$skill_dir/SKILL.md" "$dest/SKILL.md"
    ok "skill: $skill_name → $dest/"
done

# ── 4b. Install Claude Code statusline ────────────────────────────────────────
# Always copy the script; only add the statusLine key to settings.json if one
# is not already configured (so custom statuslines aren't clobbered).
STATUSLINE_SCRIPT="$HOME/.claude/statusline-command.sh"
cp "$REPO_DIR/scripts/statusline-command.sh" "$STATUSLINE_SCRIPT"
chmod +x "$STATUSLINE_SCRIPT"
ok "statusline-command.sh → $STATUSLINE_SCRIPT"

if command -v jq >/dev/null 2>&1; then
    if [ -f "$CLAUDE_SETTINGS" ] && jq -e '.statusLine' "$CLAUDE_SETTINGS" >/dev/null 2>&1; then
        ok "statusLine already configured in settings.json — skipping"
    else
        # Merge statusLine into settings.json (create file if absent)
        STATUS_JSON=$(jq -n --arg cmd "bash \$HOME/.claude/statusline-command.sh" \
            '{statusLine: {type: "command", command: $cmd}}')
        if [ -f "$CLAUDE_SETTINGS" ]; then
            jq -s '.[0] * .[1]' "$CLAUDE_SETTINGS" <(echo "$STATUS_JSON") > "$CLAUDE_SETTINGS.tmp" \
                && mv "$CLAUDE_SETTINGS.tmp" "$CLAUDE_SETTINGS"
        else
            echo "$STATUS_JSON" > "$CLAUDE_SETTINGS"
        fi
        ok "statusLine configured in settings.json"
    fi
else
    warn "jq not found — could not configure statusLine in settings.json"
    warn "Add manually: { \"statusLine\": { \"type\": \"command\", \"command\": \"bash \$HOME/.claude/statusline-command.sh\" } }"
fi

# ── 4c. Install global AGENTS.md for OpenCode ─────────────────────────────────
# OpenCode loads ~/.config/opencode/AGENTS.md for every project as a global
# system prompt.  We extract only the section between <!-- nibble:global:begin -->
# and <!-- nibble:global:end --> from AGENTS.md — that contains the factory
# pipeline instructions without the sandbox-specific environment/toolchain blocks
# that only apply inside nibble containers.
OPENCODE_CONFIG_DIR="$HOME/.config/opencode"
mkdir -p "$OPENCODE_CONFIG_DIR"
awk '/<!-- nibble:global:begin -->/{found=1; next} /<!-- nibble:global:end -->/{found=0; next} found' \
    "$REPO_DIR/AGENTS.md" > "$OPENCODE_CONFIG_DIR/AGENTS.md"
ok "global AGENTS.md → $OPENCODE_CONFIG_DIR/AGENTS.md (extracted from AGENTS.md)"

# Check shell aliases
SHELL_RC=""
[ -f "$HOME/.zshrc" ]  && SHELL_RC="$HOME/.zshrc"
[ -f "$HOME/.bashrc" ] && [ -z "$SHELL_RC" ] && SHELL_RC="$HOME/.bashrc"

if [ -n "$SHELL_RC" ]; then
    MISSING_ALIASES=()
    grep -q "nibble/wrappers/claude-wrapper" "$SHELL_RC" 2>/dev/null || MISSING_ALIASES+=("claude")

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
fi

# ── 5. Sandbox image ─────────────────────────────────────────────────────────
step "Building sandbox image (node:20-slim + claude-code + opencode)"

SANDBOX_BUILD_ARGS=""
[ "$REBUILD_IMAGE" = true ] && SANDBOX_BUILD_ARGS="--rebuild"

if "$BIN_DIR/nibble" sandbox build $SANDBOX_BUILD_ARGS; then
    ok "Sandbox image ready: nibble-sandbox:latest"
else
    warn "Sandbox image build failed."
    warn "Retry with: ./install.sh --rebuild"
fi

# ── 5b. Hermes sandbox image ──────────────────────────────────────────────────
# Hermes uses a separate image (nibble-hermes:latest) with Python + Hermes Agent.
# Build it lazily on first `nibble hermes init`, or eagerly here if podman is available.
if "$BIN_DIR/nibble" sandbox build --image nibble-hermes:latest $SANDBOX_BUILD_ARGS 2>/dev/null; then
    ok "Hermes sandbox image ready: nibble-hermes:latest"
else
    ok "Hermes image will be built on first 'nibble hermes init'"
fi

# ── 5a. Install systemd auto-resume service ───────────────────────────────────
SYSTEMD_DIR="$HOME/.config/systemd/user"
mkdir -p "$SYSTEMD_DIR"
cat > "$SYSTEMD_DIR/nibble-resume.service" << UNIT
[Unit]
Description=Nibble — resume sandbox agents after reboot
After=default.target

[Service]
Type=oneshot
ExecStart=$BIN_DIR/nibble sandbox resume --all
RemainAfterExit=yes

[Install]
WantedBy=default.target
UNIT

if systemctl --user daemon-reload 2>/dev/null; then
    systemctl --user enable nibble-resume.service 2>/dev/null \
        && ok "Auto-resume service enabled (nibble-resume.service)" \
        || warn "Could not enable auto-resume service. Enable manually: systemctl --user enable nibble-resume.service"
else
    warn "systemd user session not available. Auto-resume on reboot won't work."
fi

# ── 6. Claude Code hooks ──────────────────────────────────────────────────────
step "Installing Claude Code hooks"

mkdir -p "$HOME/.claude"

# Pre-remove any existing nibble/agent-inbox hooks so setup-claude-hooks.sh always
# writes the latest version.
if grep -q "AGENT_TASK_ID" "$CLAUDE_SETTINGS" 2>/dev/null; then
    if command -v jq >/dev/null 2>&1; then
        jq 'del(.hooks)' "$CLAUDE_SETTINGS" > "$CLAUDE_SETTINGS.tmp" \
            && mv "$CLAUDE_SETTINGS.tmp" "$CLAUDE_SETTINGS"
    fi
fi

bash "$REPO_DIR/scripts/setup-claude-hooks.sh"

# ── 7. Memory system setup ──────────────────────────────────────────────────
step "Setting up memory system"

MEMORY_DIR="$HOME/.nibble/memory"
NEEDS_SETUP=false

# Auto-initialize if directory doesn't exist yet
if [ ! -d "$MEMORY_DIR/.git" ]; then
    if "$BIN_DIR/nibble" memory reindex 2>/dev/null; then
        ok "Memory directory created and git-initialized"
    else
        warn "Could not auto-initialize memory dir"
    fi
fi

# Check current state
if [ -d "$MEMORY_DIR/.git" ]; then
    ok "memory directory initialized ($MEMORY_DIR)"

    # Quick stats
    MEM_COUNT=$(find "$MEMORY_DIR/memories" -name '*.md' 2>/dev/null | wc -l | tr -d ' ')
    LESSON_COUNT=$(find "$MEMORY_DIR/lessons" -name '*.md' 2>/dev/null | wc -l | tr -d ' ')
    ok "$MEM_COUNT memories, $LESSON_COUNT lessons"

    # Check if remote is wired
    MEM_REMOTE=$(git -C "$MEMORY_DIR" remote 2>/dev/null | head -1)
    if [ -n "$MEM_REMOTE" ]; then
        MEM_REMOTE_URL=$(git -C "$MEMORY_DIR" remote get-url "$MEM_REMOTE" 2>/dev/null)
        ok "sync remote: $MEM_REMOTE_URL"
    else
        NEEDS_SETUP=true
    fi
else
    NEEDS_SETUP=true
fi

if [ "$NEEDS_SETUP" = true ]; then
    echo ""
    warn "Memory system needs configuration."

    # Only offer interactive wizard when stdin is a terminal
    if [ -t 0 ]; then
        echo ""
        echo -n "  Launch setup wizard? [Y/n] "
        read -r answer
        case "$answer" in
            [Nn]* | [Nn][Oo] )
                echo ""
                echo "  Skipped. Run anytime:"
                echo -e "    ${BOLD}nibble memory config --setup${NC}"
                echo ""
                ;;
            * )
                echo ""
                "$BIN_DIR/nibble" memory config --setup
                ;;
        esac
    else
        echo ""
        echo "  Run the setup wizard:"
        echo -e "    ${BOLD}nibble memory config --setup${NC}"
        echo ""
        echo "  Or clone an existing memory repo:"
        echo "    git clone <your-repo-url> ~/.nibble/memory"
        echo ""
    fi
fi

# ── 8. Telegram (optional) ────────────────────────────────────────────────────
if [ "$RUN_TELEGRAM" = true ]; then
    step "Setting up Telegram notifications"
    bash "$REPO_DIR/scripts/setup-telegram.sh"
else
    CONFIG_FILE="$HOME/.nibble/config.toml"
    if grep -q "enabled = true" "$CONFIG_FILE" 2>/dev/null; then
        ok "Telegram already configured ($CONFIG_FILE)"
    else
        echo ""
        warn "Telegram not configured. Run when ready:"
        warn "  ./install.sh --telegram"
    fi
fi

# ── 9. Telegram listener daemon (optional) ────────────────────────────────────
if [ "$RUN_LISTEN" = true ]; then
    step "Setting up Telegram reply listener (systemd service)"
    bash "$REPO_DIR/scripts/setup-listen.sh"
else
    # Offer the hint only when Telegram is already configured but listener isn't running.
    CONFIG_FILE="$HOME/.nibble/config.toml"
    if grep -q "enabled = true" "$CONFIG_FILE" 2>/dev/null; then
        if ! systemctl --user is-active --quiet nibble-listener.service 2>/dev/null; then
            echo ""
            warn "Telegram reply listener not running. Enable with:"
            warn "  ./install.sh --listen"
        else
            ok "Telegram reply listener already running"
        fi
    fi
fi

# ── 10. Llama server (optional) ────────────────────────────────────────────────
if [ "$RUN_LLAMA" = true ]; then
    step "Setting up llama-server service"
    bash "$REPO_DIR/scripts/setup-llama-server.sh"
else
    if [ ! -f /etc/systemd/system/llama-server.service ]; then
        echo ""
        warn "llama-server not installed. Set up with:"
        warn "  ./install.sh --llama"
    fi
fi

# ── 11. Done ───────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}${GREEN}Done!${NC} Restart Claude Code for hooks to take effect."
echo ""
echo "  Verify:      nibble --help"
echo "  Test notify: nibble notify --message 'install test' --attention"
echo ""
echo -e "${BOLD}Sandbox usage:${NC}"
echo "  Start agent:  nibble sandbox spawn /path/to/repo"
echo "  List agents:  nibble sandbox list"
echo "  Attach:       nibble sandbox attach <task-id>"
echo "  Attach (oc):  nibble sandbox attach <task-id> --opencode"
echo "  Kill agent:   nibble sandbox kill <task-id>"
echo "  Watch:        nibble watch"
echo "  Rebuild img:  ./install.sh --rebuild"
echo ""
echo -e "${BOLD}Hermes usage:${NC}"
echo "  Start:        nibble hermes init"
echo "  Attach:       nibble hermes attach"
echo "  Mount repo:   nibble hermes mount /path/to/repo"
echo "  Unmount repo: nibble hermes unmount /path/to/repo"
echo "  List repos:   nibble hermes list"
echo "  Stop:         nibble hermes kill"
echo ""
echo -e "${BOLD}Memory usage:${NC}"
echo "  Config:       nibble memory config"
echo "  Write:        nibble memory write 'decision: chose Rust over Go'"
echo "  Search:       nibble memory search 'database decision'"
echo "  List:         nibble memory list"
echo "  Lessons:      nibble memory lessons"
echo "  Sync:         nibble memory sync"
echo ""
