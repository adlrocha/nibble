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
REBUILD_IMAGE=false
for arg in "$@"; do
    case "$arg" in
        --telegram) RUN_TELEGRAM=true ;;
        --listen)   RUN_LISTEN=true ;;
        --rebuild)  REBUILD_IMAGE=true ;;
        *) die "Unknown argument: $arg" ;;
    esac
done

echo -e "${BOLD}=== Nibble — Install / Upgrade ===${NC}"
echo ""
echo "  Flags: --telegram  set up Telegram notifications"
echo "         --listen    set up Telegram reply listener daemon"
echo "         --rebuild   force rebuild the sandbox container image"
echo ""
[ "$REBUILD_IMAGE" = true ] && echo -e "  ${YELLOW}--rebuild${NC}: sandbox image will be rebuilt from scratch"

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

cp "$REPO_DIR/target/release/agent-bridge" "$BIN_DIR/agent-bridge"
chmod +x "$BIN_DIR/agent-bridge"
ok "agent-bridge"

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

for skill_dir in "$REPO_DIR/skills"/factory-*/; do
    skill_name="$(basename "$skill_dir")"
    dest="$CLAUDE_SKILLS_DIR/$skill_name"
    mkdir -p "$dest"
    cp "$skill_dir/SKILL.md" "$dest/SKILL.md"
    ok "skill: $skill_name → $dest/"
done

# ── 4b. Install global AGENTS.md for OpenCode ─────────────────────────────────
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

# ── 7. Telegram (optional) ────────────────────────────────────────────────────
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

# ── 8. Telegram listener daemon (optional) ────────────────────────────────────
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

# ── 9. Done ───────────────────────────────────────────────────────────────────
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
