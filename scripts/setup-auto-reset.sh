#!/bin/bash
# Setup automatic reset of nibble tasks on login/restart
# This creates a systemd user service that clears stale tasks

set -e

NIBBLE_BIN="${NIBBLE_BIN:-$HOME/.local/bin/nibble}"
SERVICE_DIR="$HOME/.config/systemd/user"
SERVICE_FILE="$SERVICE_DIR/nibble-reset.service"

# Check if nibble is installed
if [ ! -x "$NIBBLE_BIN" ]; then
    echo "Error: nibble not found at $NIBBLE_BIN"
    echo "Please install it first or set NIBBLE_BIN environment variable"
    exit 1
fi

# Create systemd user directory
mkdir -p "$SERVICE_DIR"

# Create the service file
cat > "$SERVICE_FILE" << EOF
[Unit]
Description=Reset nibble tasks on login
After=default.target

[Service]
Type=oneshot
ExecStart=$NIBBLE_BIN reset --force
RemainAfterExit=yes

[Install]
WantedBy=default.target
EOF

echo "Created service file: $SERVICE_FILE"

# Reload systemd and enable the service
systemctl --user daemon-reload
systemctl --user enable nibble-reset.service

echo ""
echo "Auto-reset service installed and enabled!"
echo "Tasks will be automatically cleared on each login/restart."
echo ""
echo "To disable: systemctl --user disable nibble-reset.service"
echo "To check status: systemctl --user status nibble-reset.service"
