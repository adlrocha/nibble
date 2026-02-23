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
for arg in "$@"; do
    case "$arg" in
        --telegram) RUN_TELEGRAM=true ;;
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

mkdir -p "$BIN_DIR" "$WRAPPERS_DIR"

# ── 2. Build ──────────────────────────────────────────────────────────────────
step "Building release binaries"

cargo build --release --manifest-path "$REPO_DIR/Cargo.toml" \
    || die "Cargo build failed"

ok "Build succeeded"

# ── 3. Install binaries ───────────────────────────────────────────────────────
step "Installing binaries to $BIN_DIR"

cp "$REPO_DIR/target/release/agent-inbox" "$BIN_DIR/agent-inbox"
chmod +x "$BIN_DIR/agent-inbox"
ok "agent-inbox"

cp "$REPO_DIR/target/release/agent-bridge" "$BIN_DIR/agent-bridge"
chmod +x "$BIN_DIR/agent-bridge"
ok "agent-bridge"

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

# ── 5. Claude Code hooks ──────────────────────────────────────────────────────
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

# ── 7. Done ───────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}${GREEN}Done!${NC} Restart Claude Code for hooks to take effect."
echo ""
echo "  Verify:  agent-inbox --help"
echo "  Test:    agent-inbox notify --message 'install test' --attention"
echo ""
