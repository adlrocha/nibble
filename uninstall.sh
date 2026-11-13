#!/bin/bash
# uninstall.sh — remove nibble binaries, config, services, and wrappers
#
# Usage:
#   ./uninstall.sh               # remove everything
#   ./uninstall.sh --keep-config  # keep ~/.nibble/ and ~/.claude/ settings
#
# Installed with the one-command install, it is in the source checkout:
#   bash ~/.local/share/nibble/src/uninstall.sh

set -e

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
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

for svc in nibble-listener nibble-resume nibble-reset nibble-cleanup nibble-web \
           nibble-usage nibble-rc nibble-cron; do
    if systemctl --user is-active --quiet "$svc.service" 2>/dev/null; then
        systemctl --user stop "$svc.service"
        ok "Stopped $svc.service"
    fi
done
for unit in nibble-usage.timer nibble-cleanup.timer; do
    if systemctl --user is-active --quiet "$unit" 2>/dev/null; then
        systemctl --user stop "$unit"
        ok "Stopped $unit"
    fi
done

# ── 4. Disable and remove systemd services ────────────────────────────────────
step "Removing systemd services"

for svc in nibble-listener nibble-resume nibble-reset nibble-cleanup nibble-web \
           nibble-usage nibble-rc nibble-cron; do
    if [ -f "$SYSTEMD_DIR/$svc.service" ]; then
        systemctl --user disable "$svc.service" 2>/dev/null || true
        rm -f "$SYSTEMD_DIR/$svc.service"
        ok "Removed $svc.service"
    fi
done
for unit in nibble-usage.timer nibble-cleanup.timer; do
    if [ -f "$SYSTEMD_DIR/$unit" ]; then
        systemctl --user disable "$unit" 2>/dev/null || true
        rm -f "$SYSTEMD_DIR/$unit"
        ok "Removed $unit"
    fi
done

if systemctl --user daemon-reload 2>/dev/null; then
    ok "systemd daemon reloaded"
fi

# ── 4b. Remove zellij keybindings (before removing binary) ──────────────────
step "Removing zellij keybindings"

if [ -n "$NIBBLE_BIN" ] && [ -x "$NIBBLE_BIN" ]; then
    "$NIBBLE_BIN" keybinds --uninstall \
        && ok "Removed nibble keybindings from zellij config" \
        || warn "Could not remove them. Delete the '>>> managed by nibble' section from your zellij config (config.kdl, or \$ZELLIJ_CONFIG_FILE)"
else
    warn "nibble binary not found. Delete the '>>> managed by nibble' section from your zellij config (config.kdl, or \$ZELLIJ_CONFIG_FILE)"
fi

# ── 5. Remove binaries ───────────────────────────────────────────────────────
step "Removing binaries"

for bin in nibble nibble-musl browser; do
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

# ── 7. Remove nibble-installed skills ─────────────────────────────────────────
step "Removing nibble-installed skills"

# Everything install.sh ships in skills/, plus legacy factory/fable5 installs.
NIBBLE_SKILLS="engineering git-commit-pr nibble-memory nibble-pr-review \
nibble-no-ai-slop nibble-sdlc omarchy-migration"
STALE_SKILLS="factory-pipeline factory-spec factory-verify factory-qa-gate \
factory-lessons fable5-emulation"

for skills_dir in "$HOME/.claude/skills" "$HOME/.nibble/skills" "$HOME/.pi/agent/skills"; do
    [ -d "$skills_dir" ] || continue
    for skill in $NIBBLE_SKILLS $STALE_SKILLS; do
        if [ -d "$skills_dir/$skill" ]; then
            rm -rf "${skills_dir:?}/$skill"
            ok "Removed $skills_dir/$skill"
        fi
    done
done

# ── 7b. Remove nibble-installed pi/omp extensions ─────────────────────────────
step "Removing nibble pi/omp extensions"

for ext_dir in "$HOME/.nibble/extensions" "$HOME/.pi/agent/extensions" "$HOME/.omp/agent/extensions"; do
    [ -d "$ext_dir" ] || continue
    for ext in nibble-memory.ts tok-speed.ts local-llm-autodetect.ts; do
        if [ -f "$ext_dir/$ext" ]; then
            rm -f "$ext_dir/$ext"
            ok "Removed $ext_dir/$ext"
        fi
    done
done
# ── 8. Remove agent usage timer ──────────────────────────────────────────────
step "Removing agent usage timer"

systemctl --user disable --quiet nibble-agent-usage.timer 2>/dev/null || true
rm -f "$HOME/.config/systemd/user/nibble-agent-usage.service" \
      "$HOME/.config/systemd/user/nibble-agent-usage.timer" \
      "$HOME/.local/bin/nibble-agent-usage" \
      "$HOME"/.local/state/omarchy/agents/usage/{zai,kimi,grok}.json
systemctl --user daemon-reload 2>/dev/null || true

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

# Slash commands install.sh copied from claude-commands/ (all nibble-*.md).
for dest in "$HOME"/.claude/commands/nibble-*.md; do
    [ -f "$dest" ] || continue
    rm -f "$dest" && ok "Removed Claude command /$(basename "$dest" .md)"
done

# ── 9. Remove config and data ────────────────────────────────────────────────
step "Removing config and data"

if [ "$KEEP_CONFIG" = false ]; then
    NIBBLE_DIR="$HOME/.nibble"
    if [ -d "$NIBBLE_DIR" ]; then
        rm -rf "$NIBBLE_DIR"
        ok "Removed $NIBBLE_DIR/"
    fi
    # Slack credentials from `nibble rc setup` (kept outside ~/.nibble so
    # sandboxes cannot read them).
    RC_SECRETS="${XDG_CONFIG_HOME:-$HOME/.config}/nibble/slack.env"
    if [ -f "$RC_SECRETS" ]; then
        rm -f "$RC_SECRETS"
        rmdir "$(dirname "$RC_SECRETS")" 2>/dev/null || true
        ok "Removed $RC_SECRETS"
    fi
    # Its private state (which thread drives which pane).
    RC_STATE="${XDG_STATE_HOME:-$HOME/.local/state}/nibble/rc"
    if [ -d "$RC_STATE" ]; then
        rm -rf "$RC_STATE"
        ok "Removed $RC_STATE/"
    fi
    # `nibble cron`'s per-repo secrets and the daemon's environment file.
    for f in cron-secrets.toml cron.env; do
        CRON_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/nibble/$f"
        if [ -f "$CRON_FILE" ]; then
            rm -f "$CRON_FILE"
            ok "Removed $CRON_FILE"
        fi
    done
    rmdir "${XDG_CONFIG_HOME:-$HOME/.config}/nibble" 2>/dev/null || true
    # Its private state: registered repos, approvals, run reports, daemon log.
    CRON_STATE="${XDG_STATE_HOME:-$HOME/.local/state}/nibble/cron"
    if [ -d "$CRON_STATE" ]; then
        rm -rf "$CRON_STATE"
        ok "Removed $CRON_STATE/"
    fi
    rmdir "${XDG_STATE_HOME:-$HOME/.local/state}/nibble" 2>/dev/null || true
else
    warn "Keeping $HOME/.nibble/, the Slack and cron credentials, and their state (--keep-config)"
fi

# ── 9b. Remove the one-command install's source checkout ─────────────────────
# Only a checkout install.sh cloned itself (it carries the marker) is
# deleted; a clone you work in is never touched, even if NIBBLE_SRC_DIR
# points at it. Deleting the checkout this script runs from is fine: bash
# already has the script open.
# The checkout to remove: the one this script is in when install.sh created
# it (so a custom NIBBLE_SRC_DIR is found without setting it again), else
# NIBBLE_SRC_DIR or its default.
SELF="${BASH_SOURCE[0]}"
while [ -L "$SELF" ]; do
    LINK="$(readlink "$SELF")"
    case "$LINK" in
        /*) SELF="$LINK" ;;
        *) SELF="$(dirname "$SELF")/$LINK" ;;
    esac
done
SELF_DIR="$(cd "$(dirname "$SELF")" && pwd)"
# Whether this nibble came from the one-command install, for the reinstall
# hint at the end; a clone's own install.sh otherwise.
INSTALLED_BY_PIPE=false
if [ -f "$SELF_DIR/.git/nibble-installer-managed" ]; then
    NIBBLE_SRC_DIR="$SELF_DIR"
    INSTALLED_BY_PIPE=true
else
    NIBBLE_SRC_DIR="${NIBBLE_SRC_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/nibble/src}"
fi
if [ -f "$NIBBLE_SRC_DIR/.git/nibble-installer-managed" ]; then
    rm -rf "${NIBBLE_SRC_DIR:?}"
    rmdir "$(dirname "$NIBBLE_SRC_DIR")" 2>/dev/null || true
    ok "Removed the source checkout $NIBBLE_SRC_DIR/"
elif [ -e "$NIBBLE_SRC_DIR" ]; then
    warn "Keeping $NIBBLE_SRC_DIR/ (not created by install.sh)"
fi

# ── 10. Remind about shell aliases ───────────────────────────────────────────
SHELL_RC=""
[ -f "$HOME/.zshrc" ]  && SHELL_RC="$HOME/.zshrc"
[ -f "$HOME/.bashrc" ] && [ -z "$SHELL_RC" ] && SHELL_RC="$HOME/.bashrc"

if [ -n "$SHELL_RC" ]; then
    for agent in claude pi omp; do
        if grep -q "nibble/wrappers/$agent-wrapper" "$SHELL_RC" 2>/dev/null; then
            echo ""
            warn "A shell alias for $agent-wrapper was found in $SHELL_RC."
            warn "Remove it manually and reload your shell."
        fi
    done
fi

echo ""
echo -e "${BOLD}${GREEN}Done!${NC} nibble has been uninstalled."
echo ""
echo "  Reinstall anytime with:"
if [ "$INSTALLED_BY_PIPE" = true ]; then
    echo "    curl -fsSL https://raw.githubusercontent.com/adlrocha/nibble/main/install.sh | bash"
else
    echo "    $(printf '%q' "$SELF_DIR/install.sh")"
fi
