#!/usr/bin/env bash
# oh-my-pi. MODEL like local/deepseek-v4.1-flash (provider/id from ~/.omp/agent/models.yml)
set -uo pipefail
# STUDENT_ARGS is whitespace-separated; never expand globs from the workspace.
set -f
mkdir -p "$TRACE_DIR/sessions"
# shellcheck disable=SC2086
omp -p --cwd "$WORKSPACE" --model "$MODEL" --session-dir "$TRACE_DIR/sessions" \
    --auto-approve --no-context-files --no-rules --no-skills --no-title ${STUDENT_ARGS:-} \
    "$(cat "$PROMPT_FILE")"
rc=$?
f=$(python3 -c 'from pathlib import Path; import sys; p = Path(sys.argv[1]); fs = list(p.glob("*.jsonl")) + list(p.glob("*/*.jsonl")); print(max(fs, key=lambda x: x.stat().st_mtime) if fs else "")' "$TRACE_DIR/sessions")
[ -n "$f" ] && cp "$f" "$TRACE_DIR/session.jsonl"
exit $rc
