#!/bin/bash
# Setup Claude Code hooks for nibble integration
# This installs hooks globally in ~/.claude/settings.json

set -e

CLAUDE_SETTINGS_DIR="$HOME/.claude"
CLAUDE_SETTINGS_FILE="$CLAUDE_SETTINGS_DIR/settings.json"

echo "Setting up Claude Code hooks for nibble..."

# Create .claude directory if it doesn't exist
mkdir -p "$CLAUDE_SETTINGS_DIR"

# ── Status sources ─────────────────────────────────────────────────────────────
# Only synchronous, per-turn lifecycle events report status. Notification is
# deliberately NOT a status source: it is an async standalone event whose
# "waiting for your input" idle message can land after the next prompt's
# UserPromptSubmit report, flipping a working agent back to blocked (the
# stuck-sidebar race). Stop already reports the waiting state, and permission
# prompts are reported synchronously by PermissionRequest at the exact
# moment the dialog appears.
#
# All hooks log stderr to ~/.nibble/logs/hooks.log so dropped reports are
# diagnosable instead of silently swallowed.

# ── Task reference derivation ──────────────────────────────────────────────────
# A wrapper (claude-wrapper) or sandbox attach exports AGENT_TASK_ID and the
# hooks use it as-is. Anything launched without one — `claude -p` in a script,
# a non-interactive shell, a `nibble sandbox bash` shell — is still tracked:
# every hook payload carries Claude's own session_id, so a stable task ID is
# derived as claude-<session_id>. nibble self-heals unknown rows
# (ensure_task_or_create: agent type from NIBBLE_AGENT_TYPE, title/cwd from
# the reporting process), so the derived row appears with no wrapper involved.

# SessionStart — registers the task at launch so unwrapped sessions are
# visible before the first prompt (wrapped sessions keep their wrapper UUID).
SESSIONSTART_CMD='INPUT=$(cat); mkdir -p "$HOME/.nibble/logs" 2>/dev/null; exec 2>>"$HOME/.nibble/logs/hooks.log" || true; SID=""; if command -v jq >/dev/null 2>&1; then SID=$(printf "%s" "$INPUT" | jq -r ".sessionId // .session_id // empty"); fi; TID="${AGENT_TASK_ID:-claude-$SID}"; if [ -n "$TID" ]; then export NIBBLE_AGENT_TYPE=claude; nibble report status "$TID" running || true; [ -n "$SID" ] && nibble report session-id "$TID" "$SID" || true; fi'

# UserPromptSubmit — turn started: status → working + capture user message.
USERPROMPT_CMD='INPUT=$(cat); mkdir -p "$HOME/.nibble/logs" 2>/dev/null; exec 2>>"$HOME/.nibble/logs/hooks.log" || true; SID=""; if command -v jq >/dev/null 2>&1; then SID=$(printf "%s" "$INPUT" | jq -r ".sessionId // .session_id // empty"); fi; TID="${AGENT_TASK_ID:-claude-$SID}"; if [ -n "$TID" ]; then export NIBBLE_AGENT_TYPE=claude; nibble report status "$TID" working || true; if command -v jq >/dev/null 2>&1; then MSG=$(printf "%s" "$INPUT" | jq -r ".message // empty"); [ -n "$MSG" ] && nibble memory capture "$TID" "user" "$MSG" || true; fi; fi'

# PostToolUse — agent active mid-turn: status → working + capture tool calls.
POSTTOOL_CMD='INPUT=$(cat); mkdir -p "$HOME/.nibble/logs" 2>/dev/null; exec 2>>"$HOME/.nibble/logs/hooks.log" || true; SID=""; if command -v jq >/dev/null 2>&1; then SID=$(printf "%s" "$INPUT" | jq -r ".sessionId // .session_id // empty"); fi; TID="${AGENT_TASK_ID:-claude-$SID}"; if [ -n "$TID" ]; then export NIBBLE_AGENT_TYPE=claude; nibble report status "$TID" working || true; if command -v jq >/dev/null 2>&1; then TOOL=$(printf "%s" "$INPUT" | jq -r ".tool_name // empty"); TOOL_INPUT=$(printf "%s" "$INPUT" | jq -c ".tool_input // {}" | cut -c1-4096); TOOL_OUTPUT=$(printf "%s" "$INPUT" | jq -r ".tool_output // \"\"" | cut -c1-4096); [ -n "$TOOL" ] && nibble memory capture "$TID" "tool" "" --tool-name "$TOOL" --tool-input "$TOOL_INPUT" --tool-output "$TOOL_OUTPUT" || true; fi; fi'

# PermissionRequest — a permission dialog is pending: status → blocked.
# Synchronous: fires exactly when Claude starts waiting for the decision, and
# the next PostToolUse (working) clears it once approved.
PERMISSIONREQ_CMD='INPUT=$(cat); mkdir -p "$HOME/.nibble/logs" 2>/dev/null; exec 2>>"$HOME/.nibble/logs/hooks.log" || true; SID=""; if command -v jq >/dev/null 2>&1; then SID=$(printf "%s" "$INPUT" | jq -r ".sessionId // .session_id // empty"); fi; TID="${AGENT_TASK_ID:-claude-$SID}"; if [ -n "$TID" ]; then export NIBBLE_AGENT_TYPE=claude; MSG="Permission required"; if command -v jq >/dev/null 2>&1; then TOOL=$(printf "%s" "$INPUT" | jq -r ".tool_name // empty"); [ -n "$TOOL" ] && MSG="Permission required: $TOOL"; fi; nibble report status "$TID" blocked --message "$MSG" || true; fi'

# Stop — turn ended, waiting for input: status → idle, capture session_id +
# last assistant message, trigger async session summarization.
STOP_CMD='INPUT=$(cat); mkdir -p "$HOME/.nibble/logs" 2>/dev/null; exec 2>>"$HOME/.nibble/logs/hooks.log" || true; SID=""; if command -v jq >/dev/null 2>&1; then SID=$(printf "%s" "$INPUT" | jq -r ".sessionId // .session_id // empty"); fi; TID="${AGENT_TASK_ID:-claude-$SID}"; if [ -n "$TID" ]; then export NIBBLE_AGENT_TYPE=claude; [ -n "$SID" ] && nibble report session-id "$TID" "$SID" || true; if command -v jq >/dev/null 2>&1; then MSG=$(printf "%s" "$INPUT" | jq -r ".last_assistant_message // \"(no message)\""); else MSG="(install jq to see last message)"; fi; nibble report status "$TID" idle || true; nibble memory capture "$TID" "assistant" "$MSG" || true; nibble memory summarize "$TID" >/dev/null 2>&1 & fi'

# StopFailure — turn ended on an API error: status → idle so the row doesn't
# linger as working (the 30-min stale demotion would catch it, this is faster).
STOPFAILURE_CMD='INPUT=$(cat); mkdir -p "$HOME/.nibble/logs" 2>/dev/null; exec 2>>"$HOME/.nibble/logs/hooks.log" || true; SID=""; if command -v jq >/dev/null 2>&1; then SID=$(printf "%s" "$INPUT" | jq -r ".sessionId // .session_id // empty"); fi; TID="${AGENT_TASK_ID:-claude-$SID}"; if [ -n "$TID" ]; then export NIBBLE_AGENT_TYPE=claude; nibble report status "$TID" idle || true; fi'

# SessionEnd — marks the task exited so the status sidebar drops it.
SESSIONEND_CMD='INPUT=$(cat); mkdir -p "$HOME/.nibble/logs" 2>/dev/null; exec 2>>"$HOME/.nibble/logs/hooks.log" || true; SID=""; if command -v jq >/dev/null 2>&1; then SID=$(printf "%s" "$INPUT" | jq -r ".sessionId // .session_id // empty"); fi; TID="${AGENT_TASK_ID:-claude-$SID}"; if [ -n "$TID" ]; then export NIBBLE_AGENT_TYPE=claude; nibble report status "$TID" exited || true; fi'

# Session retention — Claude Code purges local transcripts older than
# `cleanupPeriodDays` (default: 30 days). Nibble never deletes session data,
# so pin this to ~100 years to keep every session forever.
CLEANUP_PERIOD_DAYS=36500

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
            --arg sessionstart "$SESSIONSTART_CMD" \
            --arg userprompt "$USERPROMPT_CMD" \
            --arg posttool "$POSTTOOL_CMD" \
            --arg permissionreq "$PERMISSIONREQ_CMD" \
            --arg stop   "$STOP_CMD" \
            --arg stopfailure "$STOPFAILURE_CMD" \
            --arg sessionend "$SESSIONEND_CMD" \
            --argjson cleanup "$CLEANUP_PERIOD_DAYS" \
            '{
              cleanupPeriodDays: $cleanup,
              hooks: {
                SessionStart:      [{hooks: [{type:"command", command:$sessionstart, timeout:5}]}],
                UserPromptSubmit:  [{hooks: [{type:"command", command:$userprompt, timeout:5}]}],
                PostToolUse:       [{hooks: [{type:"command", command:$posttool, timeout:5}]}],
                PermissionRequest: [{hooks: [{type:"command", command:$permissionreq, timeout:5}]}],
                Stop:              [{hooks: [{type:"command", command:$stop, timeout:30}]}],
                StopFailure:       [{hooks: [{type:"command", command:$stopfailure, timeout:5}]}],
                SessionEnd:        [{hooks: [{type:"command", command:$sessionend, timeout:5}]}]
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
        --arg sessionstart "$SESSIONSTART_CMD" \
        --arg userprompt "$USERPROMPT_CMD" \
        --arg posttool "$POSTTOOL_CMD" \
        --arg permissionreq "$PERMISSIONREQ_CMD" \
        --arg stop   "$STOP_CMD" \
        --arg stopfailure "$STOPFAILURE_CMD" \
        --arg sessionend "$SESSIONEND_CMD" \
        --argjson cleanup "$CLEANUP_PERIOD_DAYS" \
        '{
          cleanupPeriodDays: $cleanup,
          hooks: {
            SessionStart:      [{hooks: [{type:"command", command:$sessionstart, timeout:5}]}],
            UserPromptSubmit:  [{hooks: [{type:"command", command:$userprompt, timeout:5}]}],
            PostToolUse:       [{hooks: [{type:"command", command:$posttool, timeout:5}]}],
            PermissionRequest: [{hooks: [{type:"command", command:$permissionreq, timeout:5}]}],
            Stop:              [{hooks: [{type:"command", command:$stop, timeout:30}]}],
            StopFailure:       [{hooks: [{type:"command", command:$stopfailure, timeout:5}]}],
            SessionEnd:        [{hooks: [{type:"command", command:$sessionend, timeout:5}]}]
          }
        }' > "$CLAUDE_SETTINGS_FILE"
fi

echo ""
echo "Claude Code hooks installed for nibble!"
echo ""
echo "  - SessionStart:      registers the task at launch (wrapper UUID or"
echo "                      derived claude-<session_id> for unwrapped sessions)"
echo "  - UserPromptSubmit:  status → working (turn started) + user capture"
echo "  - PostToolUse:       status → working (agent active) + tool capture"
echo "  - PermissionRequest: status → blocked (permission dialog pending)"
echo "  - Stop:              status → idle + session_id + summarize"
echo "  - StopFailure:       status → idle (turn ended on an API error)"
echo "  - SessionEnd:        status → exited"
echo "NOTE: You need to restart Claude Code for hooks to take effect."
