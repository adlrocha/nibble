#!/usr/bin/env bash
# Codex CLI. MODEL is passed to -m. For a local OpenAI-compatible server put the provider in STUDENT_ARGS, e.g.
#   STUDENT_ARGS='-c model_provider=local -c model_providers.local.name=local -c model_providers.local.base_url=http://host:8000/v1'
set -uo pipefail
# STUDENT_ARGS is whitespace-separated; never expand globs from the workspace.
set -f
# shellcheck disable=SC2086
codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check --ignore-user-config --ignore-rules \
    -C "$WORKSPACE" ${MODEL:+-m "$MODEL"} ${STUDENT_ARGS:-} "$(cat "$PROMPT_FILE")" \
  | tee "$TRACE_DIR/session.jsonl" \
  | python3 -c 'import sys,json
for l in sys.stdin:
    try: it=json.loads(l).get("item") or {}
    except json.JSONDecodeError: continue
    t=it.get("type")
    if t=="agent_message": print("agent:",it.get("text","")[:400],flush=True)
    elif t=="command_execution" and "exit_code" in it: print("$",str(it.get("command"))[:200],"->",it.get("exit_code"),flush=True)'
exit "${PIPESTATUS[0]}"
