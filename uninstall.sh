#!/bin/bash
# uninstall.sh — remove nibble binaries, config, services, and wrappers
#
# Usage:
#   ./uninstall.sh               # remove everything
#   ./uninstall.sh --keep-config  # keep ~/.nibble/ and ~/.claude/ settings

set -e

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

BIN_DIR="$HOME/.local/bin"
WRAPPERS_DIR="$HOME/.nibble/wrappers"
SYSTEMD_DIR="$HOME/.config/systemd/user"
CLAUDE_SETTINGS="$HOME/.claude/settings.json"

KEEP_CONFIG=false
for arg in "$@"; do
    case "$arg" in
        --keep-config) KEEP_CONFIG=true ;;
        *) echo "Unknown argument: $arg"; exit 1 ;;
    esac
done

step() { echo -e "\n${BOLD}▶ $1${NC}"; }
ok()   { echo -e "  ${GREEN}✓${NC} $1"; }
warn() { echo -e "  ${YELLOW}!${NC} $1"; }

echo -e "${BOLD}=== Nibble — Uninstall ===${NC}"
echo ""
if [ "$KEEP_CONFIG" = true ]; then
    echo "  --keep-config: keeping ~/.nibble/ and ~/.claude/ settings"
fi
echo ""

# ── 1. Kill running sandboxes (before removing binary) ────────────────────────
step "Checking for running sandboxes"

NIBBLE_BIN="$BIN_DIR/nibble"
if [ ! -x "$NIBBLE_BIN" ]; then
    NIBBLE_BIN="$(command -v nibble 2>/dev/null || true)"
fi

if [ -n "$NIBBLE_BIN" ] && [ -x "$NIBBLE_BIN" ]; then
    "$NIBBLE_BIN" sandbox kill --all 2>/dev/null && ok "Killed running sandboxes" || true
else
    warn "nibble binary not found — cannot kill sandboxes."
    warn "Remove containers manually: podman rm -f \$(podman ps -q --filter name=nibble-)"
fi

# ── 2. Remove podman containers and sandbox image ─────────────────────────────
step "Removing podman containers and sandbox image"

if command -v podman >/dev/null 2>&1; then
    CONTAINERS=$(podman ps -aq --filter name=nibble- 2>/dev/null || true)
    if [ -n "$CONTAINERS" ]; then
        podman rm -f $CONTAINERS 2>/dev/null && ok "Removed nibble containers" || true
    else
        ok "No nibble containers to remove"
    fi

    if podman image exists nibble-sandbox:latest 2>/dev/null; then
        podman rmi nibble-sandbox:latest 2>/dev/null && ok "Removed nibble-sandbox:latest image" \
            || warn "Could not remove nibble-sandbox:latest image (may have child references)"
    else
        ok "No nibble-sandbox image to remove"
    fi
else
    warn "podman not found — skipping container/image cleanup"
fi

# ── 3. Stop services ──────────────────────────────────────────────────────────
step "Stopping services"

for svc in nibble-listener nibble-resume nibble-reset nibble-cleanup; do
    if systemctl --user is-active --quiet "$svc.service" 2>/dev/null; then
        systemctl --user stop "$svc.service"
        ok "Stopped $svc.service"
    fi
done

# ── 4. Disable and remove systemd services ────────────────────────────────────
step "Removing systemd services"

for svc in nibble-listener nibble-resume nibble-reset nibble-cleanup; do
    if [ -f "$SYSTEMD_DIR/$svc.service" ]; then
        systemctl --user disable "$svc.service" 2>/dev/null || true
        rm -f "$SYSTEMD_DIR/$svc.service"
        ok "Removed $svc.service"
    fi
done

if systemctl --user daemon-reload 2>/dev/null; then
    ok "systemd daemon reloaded"
fi

# ── 5. Remove binaries ───────────────────────────────────────────────────────
step "Removing binaries"

for bin in nibble nibble-musl; do
    if [ -f "$BIN_DIR/$bin" ]; then
        rm -f "$BIN_DIR/$bin"
        ok "Removed $BIN_DIR/$bin"
    fi
done

# ── 6. Remove wrappers ───────────────────────────────────────────────────────
step "Removing wrappers"

if [ -d "$WRAPPERS_DIR" ]; then
    rm -rf "$WRAPPERS_DIR"
    ok "Removed $WRAPPERS_DIR/"
fi

# ── 7. Remove AI Factory skills ──────────────────────────────────────────────
step "Removing AI Factory skills"

CLAUDE_SKILLS_DIR="$HOME/.claude/skills"
for skill_dir in "$CLAUDE_SKILLS_DIR"/factory-*/; do
    [ -d "$skill_dir" ] || continue
    rm -rf "$skill_dir"
    ok "Removed $(basename "$skill_dir")"
done

# ── 8. Remove global AGENTS.md for OpenCode ──────────────────────────────────
OPENCODE_AGENTS="$HOME/.config/opencode/AGENTS.md"
if [ -f "$OPENCODE_AGENTS" ]; then
    rm -f "$OPENCODE_AGENTS"
    ok "Removed $OPENCODE_AGENTS"
fi

# ── 9. Remove Claude Code hooks ──────────────────────────────────────────────
step "Removing Claude Code hooks"

if [ -f "$CLAUDE_SETTINGS" ] && command -v jq >/dev/null 2>&1; then
    if grep -q "AGENT_TASK_ID" "$CLAUDE_SETTINGS" 2>/dev/null; then
        jq 'del(.hooks)' "$CLAUDE_SETTINGS" > "$CLAUDE_SETTINGS.tmp" \
            && mv "$CLAUDE_SETTINGS.tmp" "$CLAUDE_SETTINGS"
        ok "Removed nibble hooks from $CLAUDE_SETTINGS"
    else
        ok "No nibble hooks found in $CLAUDE_SETTINGS"
    fi
fi

# ── 10. Remove config and data ───────────────────────────────────────────────
step "Removing config and data"

if [ "$KEEP_CONFIG" = false ]; then
    NIBBLE_DIR="$HOME/.nibble"
    if [ -d "$NIBBLE_DIR" ]; then
        rm -rf "$NIBBLE_DIR"
        ok "Removed $NIBBLE_DIR/"
    fi
else
    warn "Keeping $HOME/.nibble/ (--keep-config)"
fi

# ── 11. Remind about shell aliases ───────────────────────────────────────────
SHELL_RC=""
[ -f "$HOME/.zshrc" ]  && SHELL_RC="$HOME/.zshrc"
[ -f "$HOME/.bashrc" ] && [ -z "$SHELL_RC" ] && SHELL_RC="$HOME/.bashrc"

if [ -n "$SHELL_RC" ] && grep -q "nibble/wrappers/claude-wrapper" "$SHELL_RC" 2>/dev/null; then
    echo ""
    warn "A shell alias for claude-wrapper was found in $SHELL_RC."
    warn "Remove it manually and reload your shell."
fi

echo ""
echo -e "${BOLD}${GREEN}Done!${NC} nibble has been uninstalled."
echo ""
echo "  Reinstall anytime with: ./install.sh"
