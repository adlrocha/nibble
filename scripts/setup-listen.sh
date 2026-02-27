#!/bin/bash
# Set up the agent-inbox Telegram listener as a systemd user service.
#
# The listener runs `agent-inbox listen` in the background and routes
# Telegram replies back to running agent sessions on your machine.
#
# Usage:
#   ./scripts/setup-listen.sh          # install and enable
#   ./scripts/setup-listen.sh --stop   # disable and stop

set -e

AGENT_INBOX_BIN="${AGENT_INBOX_BIN:-$HOME/.local/bin/agent-inbox}"
SERVICE_DIR="$HOME/.config/systemd/user"
SERVICE_NAME="agent-inbox-listen"
SERVICE_FILE="$SERVICE_DIR/$SERVICE_NAME.service"

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

ok()   { echo -e "  ${GREEN}✓${NC} $1"; }
warn() { echo -e "  ${YELLOW}!${NC} $1"; }
die()  { echo -e "  ${RED}✗${NC} $1" >&2; exit 1; }

# ── Stop / disable ────────────────────────────────────────────────────────────
if [ "${1:-}" = "--stop" ]; then
    echo -e "${BOLD}Stopping and disabling $SERVICE_NAME…${NC}"
    systemctl --user stop    "$SERVICE_NAME.service" 2>/dev/null && ok "Stopped" || warn "Was not running"
    systemctl --user disable "$SERVICE_NAME.service" 2>/dev/null && ok "Disabled" || warn "Was not enabled"
    exit 0
fi

# ── Prerequisites ─────────────────────────────────────────────────────────────
if [ ! -x "$AGENT_INBOX_BIN" ]; then
    die "agent-inbox not found at $AGENT_INBOX_BIN. Run ./install.sh first."
fi

CONFIG_FILE="$HOME/.agent-tasks/config.toml"
if ! grep -q "enabled = true" "$CONFIG_FILE" 2>/dev/null; then
    die "Telegram is not configured. Run ./install.sh --telegram first."
fi

# ── Create service ────────────────────────────────────────────────────────────
mkdir -p "$SERVICE_DIR"

cat > "$SERVICE_FILE" << EOF
[Unit]
Description=agent-inbox Telegram reply listener
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$AGENT_INBOX_BIN listen
Restart=on-failure
RestartSec=10
# Give the network time to come up after login
ExecStartPre=/bin/sleep 3

[Install]
WantedBy=default.target
EOF

ok "Created $SERVICE_FILE"

# ── Enable and start ──────────────────────────────────────────────────────────
systemctl --user daemon-reload
systemctl --user enable  "$SERVICE_NAME.service"
ok "Enabled $SERVICE_NAME (starts on login)"

# Start now if not already running
if systemctl --user is-active --quiet "$SERVICE_NAME.service"; then
    warn "$SERVICE_NAME is already running — restarting to pick up changes"
    systemctl --user restart "$SERVICE_NAME.service"
else
    systemctl --user start "$SERVICE_NAME.service"
fi
ok "Started $SERVICE_NAME"

echo ""
echo -e "${BOLD}${GREEN}Done!${NC}"
echo ""
echo "  Status:  systemctl --user status $SERVICE_NAME"
echo "  Logs:    journalctl --user -u $SERVICE_NAME -f"
echo "  Stop:    ./scripts/setup-listen.sh --stop"
echo ""
