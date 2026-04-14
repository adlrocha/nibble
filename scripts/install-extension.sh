#!/bin/bash
set -e

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"

echo "Installing Agent Inbox Browser Extension..."
echo ""

# Detect browser
BROWSER=""
if command -v google-chrome &> /dev/null || command -v chromium &> /dev/null; then
    BROWSER="chrome"
elif command -v firefox &> /dev/null; then
    BROWSER="firefox"
else
    echo "⚠ Warning: Could not detect Chrome or Firefox"
    echo "  You'll need to manually install the extension"
    BROWSER="unknown"
fi

echo "Detected browser: $BROWSER"
echo ""

# Build agent-bridge binary
echo "Building agent-bridge binary..."
cargo build --release --manifest-path "$REPO_DIR/Cargo.toml" --bin agent-bridge

# Install agent-bridge
if [ -w "/usr/local/bin" ]; then
    cp "$REPO_DIR/target/release/agent-bridge" /usr/local/bin/
    echo "✓ Installed agent-bridge to /usr/local/bin/"
else
    echo "Installing agent-bridge requires sudo..."
    sudo cp "$REPO_DIR/target/release/agent-bridge" /usr/local/bin/
    echo "✓ Installed agent-bridge to /usr/local/bin/ (with sudo)"
fi

# Verify it's in PATH
AGENT_BRIDGE_PATH=$(which agent-bridge 2>/dev/null || echo "/usr/local/bin/agent-bridge")
echo "  agent-bridge location: $AGENT_BRIDGE_PATH"

# Update manifest with actual path
sed "s|/usr/local/bin/agent-bridge|$AGENT_BRIDGE_PATH|g" \
    "$REPO_DIR/extension/com.agent_tasks.bridge.json" > /tmp/com.agent_tasks.bridge.json

# Install native messaging manifest
if [ "$BROWSER" == "chrome" ]; then
    MANIFEST_DIR="$HOME/.config/google-chrome/NativeMessagingHosts"
    mkdir -p "$MANIFEST_DIR"
    cp /tmp/com.agent_tasks.bridge.json "$MANIFEST_DIR/"
    echo "✓ Installed native messaging manifest for Chrome"
    echo "  $MANIFEST_DIR/com.agent_tasks.bridge.json"
elif [ "$BROWSER" == "firefox" ]; then
    MANIFEST_DIR="$HOME/.mozilla/native-messaging-hosts"
    mkdir -p "$MANIFEST_DIR"
    cp /tmp/com.agent_tasks.bridge.json "$MANIFEST_DIR/"
    echo "✓ Installed native messaging manifest for Firefox"
    echo "  $MANIFEST_DIR/com.agent_tasks.bridge.json"
fi

echo ""
echo "============================================"
echo "Extension Installation Complete!"
echo "============================================"
echo ""
echo "Next steps:"
echo ""
echo "1. Open Chrome/Chromium:"
echo "   - Navigate to: chrome://extensions"
echo "   - Enable 'Developer mode' (toggle in top-right)"
echo "   - Click 'Load unpacked'"
echo "   - Select directory: $REPO_DIR/extension"
echo ""
echo "2. Copy the Extension ID:"
echo "   - After loading, you'll see an ID like: abcdefghijklmnopqrstuvwxyz123456"
echo "   - Copy it"
echo ""
echo "3. Update the manifest:"
echo "   - Edit: $MANIFEST_DIR/com.agent_tasks.bridge.json"
echo "   - Replace 'EXTENSION_ID_PLACEHOLDER' with your actual extension ID"
echo ""
echo "4. Reload the extension:"
echo "   - Go back to chrome://extensions"
echo "   - Click the reload button on Agent Inbox extension"
echo ""
echo "5. Test it:"
echo "   - Open https://claude.ai and start a conversation"
echo "   - Run: nibble list --all"
echo "   - Your conversation should appear!"
echo ""
echo "For Firefox:"
echo "   - Navigate to: about:debugging#/runtime/this-firefox"
echo "   - Click 'Load Temporary Add-on'"
echo "   - Select: $REPO_DIR/extension/manifest.json"
echo ""
echo "Troubleshooting:"
echo "   - Check extension console: chrome://extensions -> Agent Inbox -> 'background page'"
echo "   - Check native messaging: Look for 'agent-bridge started' message"
echo "   - Verify database: ls -la ~/.nibble/tasks.db"
echo ""
