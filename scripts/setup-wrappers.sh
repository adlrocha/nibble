#!/bin/bash
set -e

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"

echo "Setting up nibble wrappers..."
echo ""

# Detect shell
SHELL_NAME=$(basename "$SHELL")
case "$SHELL_NAME" in
    bash)
        RC_FILE="$HOME/.bashrc"
        ;;
    zsh)
        RC_FILE="$HOME/.zshrc"
        ;;
    fish)
        echo "Error: Fish shell not yet supported. Please manually configure wrappers." >&2
        exit 1
        ;;
    *)
        echo "Warning: Unknown shell '$SHELL_NAME'. Defaulting to ~/.bashrc"
        RC_FILE="$HOME/.bashrc"
        ;;
esac

echo "Detected shell: $SHELL_NAME"
echo "Configuration file: $RC_FILE"
echo ""

# Create wrappers directory
WRAPPER_DIR="$HOME/.agent-tasks/wrappers"
mkdir -p "$WRAPPER_DIR"

# Copy wrapper scripts
echo "Installing wrapper scripts..."
for wrapper in "$REPO_DIR/wrappers/"*-wrapper; do
    if [ -f "$wrapper" ] && [ "$wrapper" != "$REPO_DIR/wrappers/TEMPLATE-wrapper" ]; then
        wrapper_name=$(basename "$wrapper")
        cp "$wrapper" "$WRAPPER_DIR/"
        chmod +x "$WRAPPER_DIR/$wrapper_name"
        echo "  ✓ Installed $wrapper_name"
    fi
done

echo ""

# Function to check if agent binary exists
check_agent() {
    local agent_name="$1"
    if command -v "$agent_name" &> /dev/null; then
        return 0
    else
        return 1
    fi
}

# Function to add alias if not already present
add_alias() {
    local agent_name="$1"
    local wrapper_path="$WRAPPER_DIR/${agent_name}-wrapper"

    # Check if alias already exists
    if grep -q "alias $agent_name=" "$RC_FILE" 2>/dev/null; then
        echo "  ⚠ Alias for '$agent_name' already exists in $RC_FILE"
        echo "    Please manually update it to: alias $agent_name='$wrapper_path'"
        return
    fi

    # Add alias
    echo "" >> "$RC_FILE"
    echo "# nibble wrapper for $agent_name" >> "$RC_FILE"
    echo "alias $agent_name='$wrapper_path'" >> "$RC_FILE"
    echo "  ✓ Added alias for '$agent_name'"
}

# Configure available agents
echo "Configuring aliases..."

# Claude Code
if check_agent "claude"; then
    # Back up original binary path
    CLAUDE_ORIGINAL=$(which claude)
    if [ -n "$CLAUDE_ORIGINAL" ]; then
        # Create a symlink or note for the original
        ln -sf "$CLAUDE_ORIGINAL" "$WRAPPER_DIR/claude.original" 2>/dev/null || true
        add_alias "claude"
    fi
else
    echo "  ⚠ 'claude' not found in PATH. Skipping wrapper setup."
    echo "    Install Claude Code and re-run this script to enable tracking."
fi

# OpenCode
if check_agent "opencode"; then
    OPENCODE_ORIGINAL=$(which opencode)
    if [ -n "$OPENCODE_ORIGINAL" ]; then
        ln -sf "$OPENCODE_ORIGINAL" "$WRAPPER_DIR/opencode.original" 2>/dev/null || true
        add_alias "opencode"
    fi
else
    echo "  ⚠ 'opencode' not found in PATH. Skipping wrapper setup."
    echo "    Install OpenCode and re-run this script to enable tracking."
fi

echo ""
echo "============================================"
echo "Setup complete!"
echo "============================================"
echo ""
echo "⚠ IMPORTANT: Reload your shell to activate the wrappers:"
echo ""
echo "  source $RC_FILE"
echo ""
echo "Or open a new terminal window."
echo ""
echo "Wrapped agents:"
if check_agent "claude"; then
    echo "  • claude (Claude Code)"
fi
if check_agent "opencode"; then
    echo "  • opencode (OpenCode)"
fi
echo ""
echo "Test it:"
echo "  1. Run a wrapped command: claude --help"
echo "  2. Check nibble: nibble list --all"
echo ""
echo "To wrap additional agents, see: $REPO_DIR/wrappers/TEMPLATE-wrapper"
echo ""
