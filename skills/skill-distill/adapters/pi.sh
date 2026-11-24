#!/usr/bin/env bash
# pi coding agent. MODEL like provider/id
set -uo pipefail
# STUDENT_ARGS is whitespace-separated; never expand globs from the workspace.
set -f
mkdir -p "$TRACE_DIR/sessions"
cd "$WORKSPACE" || exit 2
# shellcheck disable=SC2086
pi -p --model "$MODEL" --session-dir "$TRACE_DIR/sessions" --no-skills ${STUDENT_ARGS:-} "$(cat "$PROMPT_FILE")"
rc=$?
f=$(python3 -c 'from pathlib import Path; import sys; p = Path(sys.argv[1]); fs = list(p.glob("*.jsonl")) + list(p.glob("*/*.jsonl")); print(max(fs, key=lambda x: x.stat().st_mtime) if fs else "")' "$TRACE_DIR/sessions")
[ -n "$f" ] && cp "$f" "$TRACE_DIR/session.jsonl"
exit $rc
