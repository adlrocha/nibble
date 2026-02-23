#!/bin/bash
# Setup Claude Code hooks for agent-inbox integration
# This installs hooks globally in ~/.claude/settings.json

set -e

CLAUDE_SETTINGS_DIR="$HOME/.claude"
CLAUDE_SETTINGS_FILE="$CLAUDE_SETTINGS_DIR/settings.json"

echo "Setting up Claude Code hooks for agent-inbox..."

# Create .claude directory if it doesn't exist
mkdir -p "$CLAUDE_SETTINGS_DIR"

# Check if settings.json exists
if [ -f "$CLAUDE_SETTINGS_FILE" ]; then
    echo "Found existing settings at $CLAUDE_SETTINGS_FILE"

    # Check if hooks already exist
    if grep -q "AGENT_TASK_ID" "$CLAUDE_SETTINGS_FILE" 2>/dev/null; then
        echo "Hooks already configured. Skipping..."
        echo ""
        echo "To update hooks manually, edit: $CLAUDE_SETTINGS_FILE"
        exit 0
    fi

    # Backup existing settings
    BACKUP_FILE="$CLAUDE_SETTINGS_FILE.backup.$(date +%Y%m%d_%H%M%S)"
    cp "$CLAUDE_SETTINGS_FILE" "$BACKUP_FILE"
    echo "Backed up existing settings to: $BACKUP_FILE"

    # Try to merge hooks into existing settings using jq if available
    if command -v jq &> /dev/null; then
        echo "Merging hooks into existing settings..."

        HOOKS_JSON='{
          "hooks": {
            "UserPromptSubmit": [
              {
                "hooks": [
                  {
                    "type": "command",
                    "command": "if [ -n \"$AGENT_TASK_ID\" ]; then agent-inbox report running \"$AGENT_TASK_ID\" 2>/dev/null; fi",
                    "timeout": 5
                  }
                ]
              }
            ],
            "Stop": [
              {
                "hooks": [
                  {
                    "type": "command",
                    "command": "if [ -n \"$AGENT_TASK_ID\" ]; then INPUT=$(cat); agent-inbox report complete \"$AGENT_TASK_ID\" 2>/dev/null; notify-send '\''Claude Code'\'' '\''Finished generating'\'' 2>/dev/null; if command -v jq >/dev/null 2>&1; then MSG=$(printf '\''%s'\'' \"$INPUT\" | jq -r '\''.last_assistant_message // \"(no message)\"'\''); else MSG=\"(install jq to see last message)\"; fi; agent-inbox notify --task-id \"$AGENT_TASK_ID\" --message \"$MSG\" 2>/dev/null; fi",
                    "timeout": 15
                  }
                ]
              }
            ],
            "Notification": [
              {
                "matcher": "permission_prompt",
                "hooks": [
                  {
                    "type": "command",
                    "command": "if [ -n \"$AGENT_TASK_ID\" ]; then INPUT=$(cat); if command -v jq >/dev/null 2>&1; then MSG=$(printf '\''%s'\'' \"$INPUT\" | jq -r '\''.message // \"Permission required\"'\''); else MSG=\"Permission required (install jq for details)\"; fi; agent-inbox notify --task-id \"$AGENT_TASK_ID\" --message \"$MSG\" --attention 2>/dev/null; fi",
                    "timeout": 10
                  }
                ]
              }
            ],
            "SessionEnd": [
              {
                "hooks": [
                  {
                    "type": "command",
                    "command": "if [ -n \"$AGENT_TASK_ID\" ]; then agent-inbox report exited \"$AGENT_TASK_ID\" 2>/dev/null; fi",
                    "timeout": 5
                  }
                ]
              }
            ]
          }
        }'

        # Merge using jq
        jq -s '.[0] * .[1]' "$CLAUDE_SETTINGS_FILE" <(echo "$HOOKS_JSON") > "$CLAUDE_SETTINGS_FILE.tmp"
        mv "$CLAUDE_SETTINGS_FILE.tmp" "$CLAUDE_SETTINGS_FILE"
    else
        echo ""
        echo "WARNING: jq not installed. Cannot merge with existing settings."
        echo "Please manually add the hooks to: $CLAUDE_SETTINGS_FILE"
        echo ""
        echo "See README.md for the hooks configuration to add."
        exit 1
    fi
else
    echo "Creating new settings file..."

    cat > "$CLAUDE_SETTINGS_FILE" << 'EOF'
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "if [ -n \"$AGENT_TASK_ID\" ]; then agent-inbox report running \"$AGENT_TASK_ID\" 2>/dev/null; fi",
            "timeout": 5
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "if [ -n \"$AGENT_TASK_ID\" ]; then INPUT=$(cat); agent-inbox report complete \"$AGENT_TASK_ID\" 2>/dev/null; notify-send 'Claude Code' 'Finished generating' 2>/dev/null; if command -v jq >/dev/null 2>&1; then MSG=$(printf '%s' \"$INPUT\" | jq -r '.last_assistant_message // \"(no message)\"'); else MSG=\"(install jq to see last message)\"; fi; agent-inbox notify --task-id \"$AGENT_TASK_ID\" --message \"$MSG\" 2>/dev/null; fi",
            "timeout": 15
          }
        ]
      }
    ],
    "Notification": [
      {
        "matcher": "permission_prompt",
        "hooks": [
          {
            "type": "command",
            "command": "if [ -n \"$AGENT_TASK_ID\" ]; then INPUT=$(cat); if command -v jq >/dev/null 2>&1; then MSG=$(printf '%s' \"$INPUT\" | jq -r '.message // \"Permission required\"'); else MSG=\"Permission required (install jq for details)\"; fi; agent-inbox notify --task-id \"$AGENT_TASK_ID\" --message \"$MSG\" --attention 2>/dev/null; fi",
            "timeout": 10
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "if [ -n \"$AGENT_TASK_ID\" ]; then agent-inbox report exited \"$AGENT_TASK_ID\" 2>/dev/null; fi",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
EOF
fi

echo ""
echo "Claude Code hooks installed successfully!"
echo ""
echo "Hooks configured:"
echo "  - UserPromptSubmit:  marks task as 'running' when you send a message"
echo "  - Stop:              marks task as 'completed' + Telegram notification with last message"
echo "  - Notification:      Telegram 🚨 alert when Claude needs a permission decision"
echo "  - SessionEnd:        marks task as 'exited' when you exit Claude Code"
echo ""
echo "NOTE: You need to restart Claude Code for hooks to take effect."
