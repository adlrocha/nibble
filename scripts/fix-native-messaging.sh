#!/bin/bash
# Fix native messaging configuration

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"

echo "=== Agent Inbox Native Messaging Fix ==="
echo ""

# Find Brave extension directory
BRAVE_DIR="$HOME/.config/BraveSoftware/Brave-Browser/NativeMessagingHosts"
CHROME_DIR="$HOME/.config/google-chrome/NativeMessagingHosts"
CHROMIUM_DIR="$HOME/.config/chromium/NativeMessagingHosts"

# Determine which directory to use
if [ -d "$BRAVE_DIR" ]; then
  NATIVE_DIR="$BRAVE_DIR"
  echo "Using Brave directory: $NATIVE_DIR"
elif [ -d "$CHROME_DIR" ]; then
  NATIVE_DIR="$CHROME_DIR"
  echo "Using Chrome directory: $NATIVE_DIR"
elif [ -d "$CHROMIUM_DIR" ]; then
  NATIVE_DIR="$CHROMIUM_DIR"
  echo "Using Chromium directory: $NATIVE_DIR"
else
  echo "ERROR: No browser native messaging directory found!"
  exit 1
fi

echo ""
echo "Step 1: Get Extension ID"
echo "------------------------"
echo "Go to brave://extensions and enable 'Developer mode'"
echo "Find 'Agent Inbox Tracker' and copy the ID field"
echo ""
read -p "Paste extension ID here: " EXT_ID

if [ -z "$EXT_ID" ]; then
  echo "ERROR: No extension ID provided"
  exit 1
fi

echo ""
echo "Step 2: Create/Update Native Messaging Manifest"
echo "-----------------------------------------------"

AGENT_BRIDGE_PATH="$HOME/.local/bin/agent-bridge"
MANIFEST_FILE="$NATIVE_DIR/com.agent_tasks.bridge.json"

cat > "$MANIFEST_FILE" << EOF
{
  "name": "com.agent_tasks.bridge",
  "description": "Native messaging host for Agent Inbox extension",
  "path": "$AGENT_BRIDGE_PATH",
  "type": "stdio",
  "allowed_origins": [
    "chrome-extension://$EXT_ID/"
  ]
}
EOF

echo "Created: $MANIFEST_FILE"
cat "$MANIFEST_FILE"

echo ""
echo "Step 3: Verify agent-bridge Binary"
echo "-----------------------------------"

if [ ! -f "$AGENT_BRIDGE_PATH" ]; then
  echo "WARNING: $AGENT_BRIDGE_PATH not found!"
  echo "Installing..."

  if [ -f "$REPO_DIR/target/release/agent-bridge" ]; then
    cp "$REPO_DIR/target/release/agent-bridge" "$AGENT_BRIDGE_PATH"
    chmod +x "$AGENT_BRIDGE_PATH"
    echo "✓ Installed agent-bridge to $AGENT_BRIDGE_PATH"
  else
    echo "ERROR: Build agent-bridge first with: cargo build --release"
    exit 1
  fi
else
  echo "✓ agent-bridge found at $AGENT_BRIDGE_PATH"
fi

echo ""
echo "Step 4: Test Native Messaging"
echo "-----------------------------"
echo "Testing if agent-bridge can receive messages..."

# Test by echoing a test message
echo '{"type":"test","message":"hello"}' | "$AGENT_BRIDGE_PATH" 2>&1 | head -n 5

echo ""
echo "=== Setup Complete ==="
echo ""
echo "Next steps:"
echo "1. Reload extension: brave://extensions → Agent Inbox Tracker → Reload"
echo "2. Check background console for 'Connected to native host' message"
echo "3. Test by starting a conversation in Claude.ai or Gemini"
echo "4. Verify with: nibble list --all"
echo ""
