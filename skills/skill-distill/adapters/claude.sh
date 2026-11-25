#!/usr/bin/env bash
# Claude Code. For an open model behind an Anthropic-compatible endpoint set ANTHROPIC_BASE_URL /
# ANTHROPIC_AUTH_TOKEN in the environment before the trial starts; MODEL goes to --model.
set -uo pipefail
# STUDENT_ARGS is whitespace-separated; never expand globs from the workspace.
set -f
cd "$WORKSPACE" || exit 2
# shellcheck disable=SC2086
claude -p --dangerously-skip-permissions --output-format stream-json --verbose ${MODEL:+--model "$MODEL"} \
    ${STUDENT_ARGS:-} "$(cat "$PROMPT_FILE")" \
  | tee "$TRACE_DIR/session.jsonl" \
  | python3 -c 'import sys,json
for l in sys.stdin:
    try: o=json.loads(l)
    except json.JSONDecodeError: continue
    for b in (o.get("message") or {}).get("content") or []:
        if isinstance(b,dict) and b.get("type")=="text": print("agent:",b["text"][:400],flush=True)
        elif isinstance(b,dict) and b.get("type")=="tool_use": print("tool:",b.get("name"),json.dumps(b.get("input"))[:200],flush=True)'
exit "${PIPESTATUS[0]}"
