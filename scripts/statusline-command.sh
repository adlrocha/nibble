#!/usr/bin/env bash
# Claude Code status line
# Layout: 📁 dir  git branch  🤖 model  ▓░ ctx  ▓░ 5h quota  ▓░ 7d quota

input=$(cat)

# ANSI color codes
RESET="\033[0m"
BOLD="\033[1m"
DIM="\033[2m"
CYAN="\033[36m"
YELLOW="\033[33m"
GREEN="\033[32m"
ORANGE="\033[38;5;208m"
RED="\033[31m"
BLUE="\033[34m"
MAGENTA="\033[35m"
WHITE="\033[37m"
BG_RESET="\033[49m"

# Build a mini bar: filled blocks out of 8
# Usage: make_bar <percentage> <fill_color>
make_bar() {
  local pct="$1"
  local color="$2"
  local total=8
  local filled=$(( (pct * total + 50) / 100 ))
  local empty=$(( total - filled ))

  # Pick color based on percentage (green > 50, orange 20-50, red < 20)
  if [ -z "$color" ]; then
    if [ "$pct" -ge 50 ]; then
      color="$GREEN"
    elif [ "$pct" -ge 20 ]; then
      color="$ORANGE"
    else
      color="$RED"
    fi
  fi

  local bar=""
  for (( i=0; i<filled; i++ )); do bar="${bar}█"; done
  for (( i=0; i<empty; i++ )); do  bar="${bar}░"; done

  printf "%b%s%b" "$color" "$bar" "$RESET"
}

# ── Directory ────────────────────────────────────────────────────────────────
cwd=$(echo "$input" | jq -r '.workspace.current_dir // .cwd // empty')
[ -z "$cwd" ] && cwd=$(pwd)
short_cwd="${cwd/#$HOME/\~}"

# ── Git branch ───────────────────────────────────────────────────────────────
git_part=""
if git -C "$cwd" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  branch=$(git -C "$cwd" symbolic-ref --short HEAD 2>/dev/null \
           || git -C "$cwd" rev-parse --short HEAD 2>/dev/null)
  [ -n "$branch" ] && git_part=" ${YELLOW} ${branch}${RESET}"
fi

# ── Model ────────────────────────────────────────────────────────────────────
model=$(echo "$input" | jq -r '.model.display_name // empty')
model_part=""
[ -n "$model" ] && model_part=" ${DIM}│${RESET} ${MAGENTA}🤖 ${model}${RESET}"

# ── Context window ────────────────────────────────────────────────────────────
ctx_part=""
remaining=$(echo "$input" | jq -r '.context_window.remaining_percentage // empty')
if [ -n "$remaining" ]; then
  ctx_pct=$(printf '%.0f' "$remaining")
  ctx_bar=$(make_bar "$ctx_pct")
  ctx_part=" ${DIM}│${RESET} ${CYAN}ctx${RESET} ${ctx_bar} ${CYAN}${ctx_pct}%${RESET}"
fi

# ── 5-hour quota ─────────────────────────────────────────────────────────────
five_part=""
five_pct=$(echo "$input" | jq -r '.rate_limits.five_hour.used_percentage // empty')
five_resets=$(echo "$input" | jq -r '.rate_limits.five_hour.resets_at // empty')
if [ -n "$five_pct" ]; then
  five_remaining=$(( 100 - $(printf '%.0f' "$five_pct") ))
  five_bar=$(make_bar "$five_remaining")
  reset_str=""
  if [ -n "$five_resets" ]; then
    reset_fmt=$(date -d "@${five_resets}" "+%H:%M" 2>/dev/null \
                || date -r "${five_resets}" "+%H:%M" 2>/dev/null)
    reset_str=" ${DIM}↺${reset_fmt}${RESET}"
  fi
  five_part=" ${DIM}│${RESET} ${BLUE}5h${RESET} ${five_bar} ${BLUE}${five_remaining}%${RESET}${reset_str}"
fi

# ── 7-day quota ──────────────────────────────────────────────────────────────
week_part=""
week_pct=$(echo "$input" | jq -r '.rate_limits.seven_day.used_percentage // empty')
week_resets=$(echo "$input" | jq -r '.rate_limits.seven_day.resets_at // empty')
if [ -n "$week_pct" ]; then
  week_remaining=$(( 100 - $(printf '%.0f' "$week_pct") ))
  week_bar=$(make_bar "$week_remaining")
  reset_str=""
  if [ -n "$week_resets" ]; then
    reset_fmt=$(date -d "@${week_resets}" "+%a %H:%M" 2>/dev/null \
                || date -r "${week_resets}" "+%a %H:%M" 2>/dev/null)
    reset_str=" ${DIM}↺${reset_fmt}${RESET}"
  fi
  week_part=" ${DIM}│${RESET} ${BLUE}7d${RESET} ${week_bar} ${BLUE}${week_remaining}%${RESET}${reset_str}"
fi

# ── Assemble ──────────────────────────────────────────────────────────────────
printf "%b📁 %s%b%b%b%b%b%b" \
  "$CYAN$BOLD" \
  "$short_cwd" \
  "$RESET" \
  "$git_part" \
  "$model_part" \
  "$ctx_part" \
  "$five_part" \
  "$week_part"
