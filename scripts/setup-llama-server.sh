#!/bin/bash
# Set up llama-server as a systemd service for local LLM inference.
#
# Configurable via environment variables or flags:
#   LLAMA_MODEL  — path to the .gguf model file
#   LLAMA_USER   — user account the service runs as (default: current user)
#   LLAMA_PORT   — port to bind (default: 6969)
#
# Usage:
#   ./scripts/setup-llama-server.sh                          # install and enable
#   ./scripts/setup-llama-server.sh --stop                   # disable and stop
#   LLAMA_MODEL=/path/to/model.gguf ./scripts/setup-llama-server.sh

set -e

# ── Defaults (override via environment or edit below) ─────────────────────────
LLAMA_MODEL="${LLAMA_MODEL:-/home/adlrocha/workspace/llm-models/Qwen3.5-27B-UD-Q5_K_XL.gguf}"
LLAMA_USER="${LLAMA_USER:-$(whoami)}"
LLAMA_PORT="${LLAMA_PORT:-6969}"
LLAMA_BIN="${LLAMA_BIN:-/usr/bin/llama-server}"

SERVICE_DIR="/etc/systemd/system"
SERVICE_NAME="llama-server"
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
    sudo systemctl stop    "$SERVICE_NAME.service" 2>/dev/null && ok "Stopped" || warn "Was not running"
    sudo systemctl disable "$SERVICE_NAME.service" 2>/dev/null && ok "Disabled" || warn "Was not enabled"
    exit 0
fi

# ── Prerequisites ─────────────────────────────────────────────────────────────
if ! command -v llama-server >/dev/null 2>&1 && [ ! -x "$LLAMA_BIN" ]; then
    die "llama-server not found. Install llama.cpp first: https://github.com/ggml-org/llama.cpp"
fi

if [ ! -f "$LLAMA_MODEL" ]; then
    die "Model not found at $LLAMA_MODEL. Set LLAMA_MODEL to the correct path."
fi

# ── Create service ────────────────────────────────────────────────────────────
echo -e "${BOLD}Installing llama-server service…${NC}"
echo "  Model: $LLAMA_MODEL"
echo "  User:  $LLAMA_USER"
echo "  Port:  $LLAMA_PORT"

# Escape single quotes in the chat-template-kwargs JSON for the ExecStart line
CHAT_TEMPLATE_KWARGS='{"enable_thinking":false}'

sudo tee "$SERVICE_FILE" > /dev/null << EOF
[Unit]
Description=llama.cpp Server (Vulkan)
After=network.target

[Service]
Type=simple
User=$LLAMA_USER
ExecStart=$LLAMA_BIN \
    -m $LLAMA_MODEL \
    --host 0.0.0.0 \
    --port $LLAMA_PORT \
    -ngl 99 \
    -c 65536 \
    -n 4096 \
    -fa on \
    -ctk q8_0 \
    -ctv q8_0 \
    -b 512 \
    -ub 512 \
    --temp 0.6 \
    --top-p 0.95 \
    --top-k 20 \
    --min-p 0.0 \
    --repeat-penalty 1.0 \
    --chat-template-kwargs '$CHAT_TEMPLATE_KWARGS'
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

ok "Created $SERVICE_FILE"

# ── Enable and start ──────────────────────────────────────────────────────────
sudo systemctl daemon-reload
sudo systemctl enable "$SERVICE_NAME.service"
ok "Enabled $SERVICE_NAME (starts on boot)"

if sudo systemctl is-active --quiet "$SERVICE_NAME.service"; then
    warn "$SERVICE_NAME is already running — restarting to pick up changes"
    sudo systemctl restart "$SERVICE_NAME.service"
else
    sudo systemctl start "$SERVICE_NAME.service"
fi
ok "Started $SERVICE_NAME"

echo ""
echo -e "${BOLD}${GREEN}Done!${NC}"
echo ""
echo "  Status:    sudo systemctl status $SERVICE_NAME"
echo "  Logs:      sudo journalctl -u $SERVICE_NAME -f"
echo "  Stop:      ./scripts/setup-llama-server.sh --stop"
echo ""
