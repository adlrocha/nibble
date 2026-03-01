#!/bin/bash
# Setup Claude Code hooks for agent-inbox integration
# This installs hooks globally in ~/.claude/settings.json

set -e

CLAUDE_SETTINGS_DIR="$HOME/.claude"
CLAUDE_SETTINGS_FILE="$CLAUDE_SETTINGS_DIR/settings.json"

echo "Setting up Claude Code hooks for agent-inbox..."

# Create .claude directory if it doesn't exist
mkdir -p "$CLAUDE_SETTINGS_DIR"

# The Stop hook:
#   1. Reads hook JSON from stdin
#   2. Extracts session_id and stores it on the task (enables --resume from phone)
#   3. Marks task completed
#   4. Sends desktop notification
#   5. Sends Telegram notification with last assistant message
#
# The Notification hook fires on permission_prompt events and sends a 🚨 alert.
#
# The SessionEnd hook marks the task as exited when the user quits Claude Code.

STOP_CMD='if [ -n "$AGENT_TASK_ID" ]; then INPUT=$(cat); if command -v jq >/dev/null 2>&1; then SID=$(printf "%s" "$INPUT" | jq -r ".sessionId // .session_id // empty"); [ -n "$SID" ] && agent-inbox report session-id "$AGENT_TASK_ID" "$SID" 2>/dev/null; MSG=$(printf "%s" "$INPUT" | jq -r ".last_assistant_message // \"(no message)\""); agent-inbox report last-message "$AGENT_TASK_ID" "$MSG" 2>/dev/null || true; else MSG="(install jq to see last message)"; fi; agent-inbox report complete "$AGENT_TASK_ID" 2>/dev/null; notify-send "Claude Code" "Finished generating" 2>/dev/null || true; agent-inbox notify --task-id "$AGENT_TASK_ID" --message "$MSG" 2>/dev/null || true; fi'

NOTIFY_CMD='if [ -n "$AGENT_TASK_ID" ]; then INPUT=$(cat); if command -v jq >/dev/null 2>&1; then TOOL=$(printf "%s" "$INPUT" | jq -r ".tool_name // empty"); TOOL_INPUT=$(printf "%s" "$INPUT" | jq -c ".tool_input // empty"); BASE=$(printf "%s" "$INPUT" | jq -r ".message // \"Permission required\""); if [ -n "$TOOL" ]; then MSG="$BASE\nTool: $TOOL"; if [ -n "$TOOL_INPUT" ] && [ "$TOOL_INPUT" != "null" ]; then SHORT=$(printf "%s" "$TOOL_INPUT" | jq -r "to_entries | map(.key + \": \" + (.value | tostring)) | join(\", \")" 2>/dev/null | cut -c1-120); MSG="$MSG\n$SHORT"; fi; else MSG="$BASE"; fi; else MSG="Permission required (install jq for details)"; fi; agent-inbox notify --task-id "$AGENT_TASK_ID" --message "$MSG" --attention 2>/dev/null; fi'

RUNNING_CMD='if [ -n "$AGENT_TASK_ID" ]; then agent-inbox report running "$AGENT_TASK_ID" 2>/dev/null; fi'

EXITED_CMD='if [ -n "$AGENT_TASK_ID" ]; then agent-inbox report exited "$AGENT_TASK_ID" 2>/dev/null; fi'

# Check if settings.json exists
if [ -f "$CLAUDE_SETTINGS_FILE" ]; then
    echo "Found existing settings at $CLAUDE_SETTINGS_FILE"

    # Backup existing settings
    BACKUP_FILE="$CLAUDE_SETTINGS_FILE.backup.$(date +%Y%m%d_%H%M%S)"
    cp "$CLAUDE_SETTINGS_FILE" "$BACKUP_FILE"
    echo "Backed up existing settings to: $BACKUP_FILE"

    # Try to merge hooks into existing settings using jq if available
    if command -v jq &> /dev/null; then
        # Remove any existing agent-inbox hooks first so we always write the
        # latest version (avoids stale hook commands after an upgrade).
        if grep -q "AGENT_TASK_ID" "$CLAUDE_SETTINGS_FILE" 2>/dev/null; then
            jq 'del(.hooks)' "$CLAUDE_SETTINGS_FILE" > "$CLAUDE_SETTINGS_FILE.tmp" \
                && mv "$CLAUDE_SETTINGS_FILE.tmp" "$CLAUDE_SETTINGS_FILE"
            echo "Removed stale hooks — will write latest version"
        fi
        echo "Merging hooks into existing settings..."

        HOOKS_JSON=$(jq -n \
            --arg running "$RUNNING_CMD" \
            --arg stop    "$STOP_CMD" \
            --arg notify  "$NOTIFY_CMD" \
            --arg exited  "$EXITED_CMD" \
            '{
              hooks: {
                UserPromptSubmit: [{hooks: [{type:"command", command:$running, timeout:5}]}],
                Stop:             [{hooks: [{type:"command", command:$stop,    timeout:120}]}],
                Notification: [{
                  matcher: "permission_prompt",
                  hooks:   [{type:"command", command:$notify, timeout:10}]
                }],
                SessionEnd: [{hooks: [{type:"command", command:$exited, timeout:5}]}]
              }
            }')

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

    if ! command -v jq &> /dev/null; then
        echo "WARNING: jq not installed. Install jq for full hook functionality."
    fi

    jq -n \
        --arg running "$RUNNING_CMD" \
        --arg stop    "$STOP_CMD" \
        --arg notify  "$NOTIFY_CMD" \
        --arg exited  "$EXITED_CMD" \
        '{
          hooks: {
            UserPromptSubmit: [{hooks: [{type:"command", command:$running, timeout:5}]}],
            Stop:             [{hooks: [{type:"command", command:$stop,    timeout:120}]}],
            Notification: [{
              matcher: "permission_prompt",
              hooks:   [{type:"command", command:$notify, timeout:10}]
            }],
            SessionEnd: [{hooks: [{type:"command", command:$exited, timeout:5}]}]
          }
        }' > "$CLAUDE_SETTINGS_FILE"
fi

echo ""
echo "Claude Code hooks installed successfully!"
echo ""
echo "Hooks configured:"
echo "  - UserPromptSubmit:  marks task as 'running' when you send a message"
echo "  - Stop:              captures session_id + marks 'completed' + Telegram notification"
echo "  - Notification:      Telegram 🚨 alert when Claude needs a permission decision"
echo "  - SessionEnd:        marks task as 'exited' when you exit Claude Code"
echo ""
echo "NOTE: You need to restart Claude Code for hooks to take effect."
