#!/bin/bash
# Interactive setup for Telegram notifications in agent-inbox
#
# What this script does:
#   1. Walks you through creating a Telegram bot via @BotFather
#   2. Prompts for your bot token and chat ID
#   3. Writes ~/.agent-tasks/config.toml
#   4. Sends a test message to confirm everything works

set -e

CONFIG_DIR="$HOME/.agent-tasks"
CONFIG_FILE="$CONFIG_DIR/config.toml"

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo ""
echo -e "${BOLD}=== Nibble — Telegram Notification Setup ===${NC}"
echo ""

# ── Step 1: BotFather instructions ───────────────────────────────────────────
echo -e "${BOLD}Step 1: Create a Telegram bot${NC}"
echo ""
echo "  1. Open Telegram and search for @BotFather"
echo "  2. Send: /newbot"
echo "  3. Choose a name (e.g. 'My Agent Inbox')"
echo "  4. Choose a username ending in 'bot' (e.g. 'myagentinbox_bot')"
echo "  5. BotFather will reply with a token like: 123456789:ABCdefGHIjklMNOpqrSTUvwxYZ"
echo ""

# ── Step 2: Get bot token ─────────────────────────────────────────────────────
echo -e "${BOLD}Step 2: Enter your bot token${NC}"
echo ""

while true; do
    read -r -p "  Bot token: " BOT_TOKEN
    BOT_TOKEN="${BOT_TOKEN// /}" # strip accidental spaces

    if [[ "$BOT_TOKEN" =~ ^[0-9]+:[A-Za-z0-9_-]{35,}$ ]]; then
        break
    else
        echo -e "  ${RED}That doesn't look like a valid bot token. Expected format: 123456:ABC...${NC}"
    fi
done

echo ""

# ── Step 3: Get chat ID ───────────────────────────────────────────────────────
echo -e "${BOLD}Step 3: Get your chat ID${NC}"
echo ""
echo "  1. Send any message to your new bot in Telegram"
echo "  2. Then press Enter below — we'll fetch the chat ID automatically"
echo ""
read -r -p "  Press Enter after you've sent a message to your bot..."
echo ""

# Fetch updates from Telegram to extract chat_id
UPDATES=$(curl -s "https://api.telegram.org/bot${BOT_TOKEN}/getUpdates")

if echo "$UPDATES" | grep -q '"ok":true'; then
    if command -v jq >/dev/null 2>&1; then
        CHAT_ID=$(echo "$UPDATES" | jq -r '.result[-1].message.chat.id // empty' 2>/dev/null)
        USERNAME=$(echo "$UPDATES" | jq -r '.result[-1].message.from.username // empty' 2>/dev/null)
    else
        # Fallback: extract with grep/sed (less reliable but works without jq)
        CHAT_ID=$(echo "$UPDATES" | grep -o '"id":[0-9-]*' | head -1 | sed 's/"id"://')
    fi
fi

if [ -n "$CHAT_ID" ] && [ "$CHAT_ID" != "null" ]; then
    echo -e "  ${GREEN}Detected chat ID: ${CHAT_ID}${NC}"
    if [ -n "$USERNAME" ] && [ "$USERNAME" != "null" ]; then
        echo -e "  ${GREEN}Detected username: @${USERNAME}${NC}"
    fi
    read -r -p "  Use these details? [Y/n]: " CONFIRM
    if [[ "$CONFIRM" =~ ^[Nn] ]]; then
        CHAT_ID=""
        USERNAME=""
    fi
fi

if [ -z "$CHAT_ID" ] || [ "$CHAT_ID" = "null" ]; then
    echo ""
    echo "  Could not auto-detect chat ID. You can find it manually:"
    echo "  Visit: https://api.telegram.org/bot${BOT_TOKEN}/getUpdates"
    echo "  Look for: result[0].message.chat.id"
    echo ""
    read -r -p "  Chat ID: " CHAT_ID
fi

# Prompt for username if not auto-detected (strongly recommended)
if [ -z "$USERNAME" ] || [ "$USERNAME" = "null" ]; then
    echo ""
    echo "  Your Telegram username adds a second layer of security."
    echo "  Only messages from this username will be accepted by the bot."
    read -r -p "  Your Telegram username (without @, or Enter to skip): " USERNAME
fi
# Strip leading @ if user typed it
USERNAME="${USERNAME#@}"

echo ""

# ── Step 4: Write config ──────────────────────────────────────────────────────
echo -e "${BOLD}Step 4: Writing config${NC}"
echo ""

mkdir -p "$CONFIG_DIR"

{
    echo '[telegram]'
    echo 'enabled = true'
    echo "bot_token = \"$BOT_TOKEN\""
    echo "chat_id = \"$CHAT_ID\""
    if [ -n "$USERNAME" ]; then
        echo "allowed_username = \"$USERNAME\""
    fi
} > "$CONFIG_FILE"

echo -e "  ${GREEN}Config written to: $CONFIG_FILE${NC}"
echo ""

# ── Step 5: Send test message ─────────────────────────────────────────────────
echo -e "${BOLD}Step 5: Sending test message${NC}"
echo ""

TEST_RESPONSE=$(curl -s -X POST \
    "https://api.telegram.org/bot${BOT_TOKEN}/sendMessage" \
    -H "Content-Type: application/json" \
    -d "{\"chat_id\": \"${CHAT_ID}\", \"text\": \"<b>Nibble</b> — Telegram notifications are now active! You will be notified here whenever Claude Code or OpenCode finishes a turn.\", \"parse_mode\": \"HTML\"}" \
    2>/dev/null)

if echo "$TEST_RESPONSE" | grep -q '"ok":true'; then
    echo -e "  ${GREEN}Test message sent successfully! Check your Telegram.${NC}"
else
    echo -e "  ${RED}Test message failed. Response:${NC}"
    echo "  $TEST_RESPONSE"
    echo ""
    echo "  Check that:"
    echo "    - Your bot token is correct"
    echo "    - You have sent at least one message to the bot"
    echo "    - Your chat ID is correct"
    exit 1
fi

echo ""
echo -e "${BOLD}Setup complete!${NC}"
echo ""
echo "  Next steps:"
echo "    - Re-run  scripts/setup-claude-hooks.sh  to apply updated hooks"
echo "    - Restart Claude Code for hooks to take effect"
echo "    - OpenCode wrapper will notify automatically on session end"
echo ""
echo "  To disable notifications, set  enabled = false  in:"
echo "    $CONFIG_FILE"
echo ""
