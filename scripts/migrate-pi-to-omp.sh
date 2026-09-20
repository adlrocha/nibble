#!/bin/bash
# migrate-pi-to-omp.sh — migrate pi coding agent config to omp (oh-my-pi).
#
# omp (https://github.com/can1357/oh-my-pi) is a fork of pi with the same
# session format but its own config root (~/.omp/agent instead of ~/.pi/agent).
# This script:
#   1. Installs omp (standalone binary) if not present.
#   2. Copies ~/.pi/agent → ~/.omp/agent (sessions, settings, extensions,
#      skills, themes, custom providers) WITHOUT overwriting anything that
#      already exists in ~/.omp.
#   3. Prints the remaining manual steps (authentication).
#
# Auth is NOT migrated: omp stores credentials in ~/.omp/agent/agent.db and
# ignores pi's auth.json. API keys exported in your shell (ANTHROPIC_API_KEY,
# OPENAI_API_KEY, GEMINI_API_KEY, OPENROUTER_API_KEY, XAI_API_KEY, …) are picked
# up automatically by omp; everything else needs one `omp` → /login.
#
# Idempotent: safe to re-run; existing ~/.omp files are never clobbered.

set -euo pipefail

PI_AGENT="$HOME/.pi/agent"
OMP_AGENT="$HOME/.omp/agent"

echo "── pi → omp migration ─────────────────────────────"

# ── 1. Install omp if missing ────────────────────────────────────────────────
if command -v omp >/dev/null 2>&1; then
    echo "✓ omp already installed: $(command -v omp)"
else
    echo "Installing omp (standalone binary)…"
    curl -fsSL https://omp.sh/install | sh
    # The installer drops omp in ~/.local/bin — make sure it's on PATH for this run.
    export PATH="$HOME/.local/bin:$PATH"
    command -v omp >/dev/null 2>&1 || {
        echo "✗ omp install failed — see output above." >&2
        exit 1
    }
    echo "✓ omp installed: $(command -v omp)"
fi

# ── 2. Copy config ───────────────────────────────────────────────────────────
if [ ! -e "$PI_AGENT" ]; then
    echo "⚠ No pi config found at $PI_AGENT — nothing to migrate."
    exit 0
fi

mkdir -p "$OMP_AGENT"

# Files/dirs that must NOT be copied: credentials (incompatible format),
# pi-specific caches/state, and omp's own runtime files.
EXCLUDES=(
    auth.json            # omp ignores this; uses agent.db — re-login instead
    models-store.json    # pi's model catalog cache; omp uses models.db
    bin                  # pi-managed binaries
    npm                  # pi-managed npm packages cache
    tmp                  # scratch
    extensions.backup.*  # old extension backups
    .gitignore           # belongs to the pi dotfiles repo
    agent.db*            # omp runtime state (if re-running after omp ran)
    models.db*
    config.yml
    models.yml
    last-changelog-version
    custom-session-files
)

copy_with_cp() {
    # cp -a preserves symlinks (e.g. skills → ~/.claude/skills); -n never clobbers.
    (cd "$PI_AGENT" && find . -mindepth 1 -maxdepth 1 | while read -r item; do
        base=$(basename "$item")
        for pat in "${EXCLUDES[@]}"; do
            # shellcheck disable=SC2254
            case "$base" in $pat) echo "  skip  $base"; continue 2 ;; esac
        done
        cp -an "$item" "$OMP_AGENT/" 2>/dev/null \
            && echo "  copy  $base" \
            || echo "  keep  $base (already in ~/.omp)"
    done)
}

if command -v rsync >/dev/null 2>&1; then
    RSYNC_EXCLUDES=()
    for pat in "${EXCLUDES[@]}"; do
        RSYNC_EXCLUDES+=(--exclude="$pat")
    done
    # -a preserves symlinks/perms; --ignore-existing never clobbers.
    rsync -a --ignore-existing "${RSYNC_EXCLUDES[@]}" "$PI_AGENT/" "$OMP_AGENT/"
    echo "✓ Config copied (rsync, existing ~/.omp files kept)"
else
    copy_with_cp
    echo "✓ Config copied (cp, existing ~/.omp files kept)"
fi

# sessions/ may be large — report what came over.
if [ -d "$OMP_AGENT/sessions" ]; then
    n=$(find "$OMP_AGENT/sessions" -name '*.jsonl' | wc -l)
    echo "✓ Sessions available to omp: $n transcript(s)"
fi

# ── 3. Next steps ────────────────────────────────────────────────────────────
cat << 'EOF'

── Done. Remaining steps ──────────────────────────────

1. Auth: omp does not read pi's auth.json. Either:
   - export API keys in your shell (ANTHROPIC_API_KEY, OPENAI_API_KEY,
     GEMINI_API_KEY, OPENROUTER_API_KEY, XAI_API_KEY, …) — omp picks them
     up automatically, or
   - run `omp` once and use /login for OAuth providers (e.g. Copilot).

2. Verify:  omp --version && omp -p "say hi"

3. Nibble sandboxes: with this repo's omp support, `nibble sandbox
   attach <repo> --pi` now runs omp (config knob: [pi] implementation in
   ~/.nibble/config.toml, or force with --omp). Sandboxes mount both
   ~/.pi and ~/.omp, so old pi sessions stay resumable.

Nothing was deleted — your pi config at ~/.pi is untouched.
EOF
