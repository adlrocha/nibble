# Harness adapters

Each adapter runs one headless agent session and exits. `distill.py trial` calls it inside a tmux window,
so you can watch it live with `tmux attach -t <session>`.

Environment passed in:

| var | meaning |
|---|---|
| `WORKSPACE` | fresh copy of the starting state; the agent's cwd |
| `PROMPT_FILE` | the original task prompt plus the line that points at the skill |
| `MODEL` | model id in the harness's own syntax |
| `TRACE_DIR` | where the adapter must leave `session.jsonl` (any format `extract_session.py` reads) |
| `STUDENT_ARGS` | extra flags, appended verbatim (provider config, thinking level, ...) |

Contract: write the harness's session log to `$TRACE_DIR/session.jsonl`, stream human-readable output to
stdout, and exit with the harness's exit code. Turn off the user's global context files and skills where
the harness allows it, so the only instructions the student gets are the prompt and the skill under test.

`STUDENT_ARGS` is split on whitespace; wildcard characters are passed literally rather than expanded.

Adding a harness: copy `omp.sh`, change the command, and check that `extract_session.py` reads the log
(add a parser there if it doesn't).
