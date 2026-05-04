#!/bin/bash
# Setup Claude Code hooks for nibble integration
# This installs hooks globally in ~/.claude/settings.json

set -e

CLAUDE_SETTINGS_DIR="$HOME/.claude"
CLAUDE_SETTINGS_FILE="$CLAUDE_SETTINGS_DIR/settings.json"

echo "Setting up Claude Code hooks for nibble..."

# Create .claude directory if it doesn't exist
mkdir -p "$CLAUDE_SETTINGS_DIR"

# ── Hook command definitions ─────────────────────────────────────────────────
#
# Each hook reads JSON from stdin (Claude sends hook context as JSON).
# Hooks must be fast and never block the agent.  Capture and summarization
# run asynchronously (backgrounded) so Claude is never waiting on us.

# UserPromptSubmit — captures user messages into the memory capture JSONL.
USERPROMPT_CMD='if [ -n "$AGENT_TASK_ID" ]; then export NIBBLE_AGENT_TYPE=claude; INPUT=$(cat); if command -v jq >/dev/null 2>&1; then MSG=$(printf "%s" "$INPUT" | jq -r ".message // empty"); [ -n "$MSG" ] && nibble memory capture "$AGENT_TASK_ID" "user" "$MSG" 2>/dev/null || true; fi; fi'

# PostToolUse — captures tool calls + results into the memory capture JSONL.
POSTTOOL_CMD='if [ -n "$AGENT_TASK_ID" ]; then export NIBBLE_AGENT_TYPE=claude; INPUT=$(cat); if command -v jq >/dev/null 2>&1; then TOOL=$(printf "%s" "$INPUT" | jq -r ".tool_name // empty"); TOOL_INPUT=$(printf "%s" "$INPUT" | jq -c ".tool_input // {}" | cut -c1-4096); TOOL_OUTPUT=$(printf "%s" "$INPUT" | jq -r ".tool_output // \"\"" | cut -c1-4096); [ -n "$TOOL" ] && nibble memory capture "$AGENT_TASK_ID" "tool" "" --tool-name "$TOOL" --tool-input "$TOOL_INPUT" --tool-output "$TOOL_OUTPUT" 2>/dev/null || true; fi; fi'

# Stop — captures session_id, last assistant message, notifies Telegram,
# and triggers async session summarization.
STOP_CMD='if [ -n "$AGENT_TASK_ID" ]; then export NIBBLE_AGENT_TYPE=claude; INPUT=$(cat); if command -v jq >/dev/null 2>&1; then SID=$(printf "%s" "$INPUT" | jq -r ".sessionId // .session_id // empty"); [ -n "$SID" ] && nibble report session-id "$AGENT_TASK_ID" "$SID" 2>/dev/null; MSG=$(printf "%s" "$INPUT" | jq -r ".last_assistant_message // \"(no message)\""); else MSG="(install jq to see last message)"; fi; nibble notify --task-id "$AGENT_TASK_ID" --message "$MSG" 2>/dev/null || true; nibble memory capture "$AGENT_TASK_ID" "assistant" "$MSG" 2>/dev/null || true; nibble memory summarize "$AGENT_TASK_ID" >/dev/null 2>&1 & fi'

# Notification — sends attention-required alerts (permission prompts, etc.).
NOTIFY_CMD='if [ -n "$AGENT_TASK_ID" ]; then export NIBBLE_AGENT_TYPE=claude; INPUT=$(cat); if command -v jq >/dev/null 2>&1; then TOOL=$(printf "%s" "$INPUT" | jq -r ".tool_name // empty"); TOOL_INPUT=$(printf "%s" "$INPUT" | jq -c ".tool_input // empty"); BASE=$(printf "%s" "$INPUT" | jq -r ".message // \"Permission required\""); if [ -n "$TOOL" ]; then MSG="$BASE\nTool: $TOOL"; if [ -n "$TOOL_INPUT" ] && [ "$TOOL_INPUT" != "null" ]; then SHORT=$(printf "%s" "$TOOL_INPUT" | jq -r "to_entries | map(.key + \": \" + (.value | tostring)) | join(\", \")" 2>/dev/null | cut -c1-120); MSG="$MSG\n$SHORT"; fi; else MSG="$BASE"; fi; else MSG="Permission required (install jq for details)"; fi; nibble notify --task-id "$AGENT_TASK_ID" --message "$MSG" --attention 2>/dev/null; fi'

# Check if settings.json exists
if [ -f "$CLAUDE_SETTINGS_FILE" ]; then
    echo "Found existing settings at $CLAUDE_SETTINGS_FILE"

    # Backup existing settings
    BACKUP_FILE="$CLAUDE_SETTINGS_FILE.backup.$(date +%Y%m%d_%H%M%S)"
    cp "$CLAUDE_SETTINGS_FILE" "$BACKUP_FILE"
    echo "Backed up existing settings to: $BACKUP_FILE"

    # Try to merge hooks into existing settings using jq if available
    if command -v jq &> /dev/null; then
        # Remove any existing nibble hooks first so we always write the
        # latest version (avoids stale hook commands after an upgrade).
        if grep -q "AGENT_TASK_ID" "$CLAUDE_SETTINGS_FILE" 2>/dev/null; then
            jq 'del(.hooks)' "$CLAUDE_SETTINGS_FILE" > "$CLAUDE_SETTINGS_FILE.tmp" \
                && mv "$CLAUDE_SETTINGS_FILE.tmp" "$CLAUDE_SETTINGS_FILE"
            echo "Removed stale hooks — will write latest version"
        fi
        echo "Merging hooks into existing settings..."

        HOOKS_JSON=$(jq -n \
            --arg userprompt "$USERPROMPT_CMD" \
            --arg posttool "$POSTTOOL_CMD" \
            --arg stop   "$STOP_CMD" \
            --arg notify "$NOTIFY_CMD" \
            '{
              hooks: {
                UserPromptSubmit: [{hooks: [{type:"command", command:$userprompt, timeout:5}]}],
                PostToolUse:      [{hooks: [{type:"command", command:$posttool, timeout:5}]}],
                Stop:             [{hooks: [{type:"command", command:$stop, timeout:30}]}],
                Notification: [{
                  matcher: "permission_prompt",
                  hooks:   [{type:"command", command:$notify, timeout:10}]
                }]
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
        --arg userprompt "$USERPROMPT_CMD" \
        --arg posttool "$POSTTOOL_CMD" \
        --arg stop   "$STOP_CMD" \
        --arg notify "$NOTIFY_CMD" \
        '{
          hooks: {
            UserPromptSubmit: [{hooks: [{type:"command", command:$userprompt, timeout:5}]}],
            PostToolUse:      [{hooks: [{type:"command", command:$posttool, timeout:5}]}],
            Stop:             [{hooks: [{type:"command", command:$stop, timeout:30}]}],
            Notification: [{
              matcher: "permission_prompt",
              hooks:   [{type:"command", command:$notify, timeout:10}]
            }]
          }
        }' > "$CLAUDE_SETTINGS_FILE"
fi

echo ""
echo "Claude Code hooks installed for nibble!"
echo ""
echo "Hooks configured:"
echo "  - UserPromptSubmit: captures user messages to memory capture JSONL"
echo "  - PostToolUse:      captures tool calls + results to memory capture JSONL"
echo "  - Stop:             captures session_id + Telegram notification + async summarization"
echo "  - Notification:     Telegram 🚨 alert when Claude needs a permission decision"
echo ""
echo "NOTE: You need to restart Claude Code for hooks to take effect."
