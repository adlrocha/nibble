---
name: skill-distill
description: Turn a good coding-agent session (Claude Code, Codex, omp) into a skill that lets a chosen open-weight model in a chosen harness reproduce the same task, by looping: write the skill, run the student in tmux on a fresh copy of the starting state, compare its result with the reference, fix the skill, run again, until the student matches and the user accepts it. Use when the user says "distill this session", "make a skill so DeepSeek/GLM/Qwen can do what you just did", "teach <model> in <harness> to reproduce this", or "skill-distill". Not for writing a general-purpose skill from scratch with no reference session, and not for fine-tuning.
---

# skill-distill

You are the **teacher**. A session just went well: the reference. The user names a **student**: an agent
harness plus a model (for example omp with DeepSeek-V4.1-Flash). Your job is to produce a skill that lets the
student do the same task from the same starting state, with the same prompt, and get a result as good.

You do not stop at a draft. You test the skill on the student, look at what it actually did, fix the skill
where the student went wrong, and test again. The **user** makes the final pass/fail call. Calling this
skill on a session already tells you the user considers that session good.

Everything deterministic is in `scripts/distill.py` (paths below are relative to this skill's folder).
You do the parts that need judgement: what the skill says, and whether a trial matches.

## 0. Settle the inputs

Get these from the conversation. Ask only for what is missing and cannot be worked out:

| input | default |
|---|---|
| student harness + model | must be named by the user. Check that it answers before going further (step 1) |
| reference session | the current session: `scripts/extract_session.py --current claude --cwd <dir>` (or `codex`, `omp`) |
| task prompt | the first user message of the reference, verbatim. Do not improve it: the student gets the same prompt |
| starting state | the git commit the reference started from (`git rev-list -1 --before=<session start> HEAD`), or `empty` |
| where the reference's result lives | the reference's working directory |

If the reference started from uncommitted changes, commit them to a throwaway branch first so the start can be
reproduced. If the task needs machines, services or data outside the repo (a GPU box, an API), write down how
each trial gets a clean copy, and pass that as `--setup`. A trial that starts in a dirty state cannot be judged.

## 1. Check that the student works at all

Run a 1-minute task through the adapter before anything else, e.g. for omp:
`omp -p --no-session --model <provider/id> "Create hello.txt containing hi, then reply DONE."`.
Wrong model id, missing key, or a server that is down will otherwise look like a skill failure.

## 2. Create the run

```
python3 scripts/distill.py init runs/<task> \
  --prompt prompt.md --student omp:<provider/model> --base <repo>@<commit>|empty \
  --reference <session.jsonl> --reference-workspace <dir> [--setup "npm ci"] [--timeout-min 90]
```

`runs/` holds private material (raw session logs, your machine's paths). Never commit it or publish it.

Read `runs/<task>/reference/reference.md` all the way through, and look at the reference result itself:
open it, run it, read the important files. The summary is a map, not the result.

## 3. Write down what "as good" means

Write `runs/<task>/acceptance.md`: 4–10 observable things that made the reference good, each one a thing
you can check in a trial. For example: "the game starts with no console errors", "walking to a tent and
pressing E feeds a person and the street count drops", "README says how to run it", "tok/s at C1 is within
5% of the reference". Prefer outcomes over method, and say how each is checked.

Then write `runs/<task>/checks.sh`, which checks as much of that list as a script can. It gets `$WORKSPACE`
and `$OUT`. Put screenshots and logs in `$OUT`, and exit 0 only when every scripted check passes.
For web apps, call `scripts/capture_web.py`. Run it on the reference first:

```
python3 scripts/distill.py evidence runs/<task> --reference
```

**The checks must pass on the reference.** If they don't, the checks are wrong. Fix them before you judge any trial.

## 4. Write the first version of the student skill

Edit `runs/<task>/skill/SKILL.md` (it starts from `templates/student-skill/SKILL.md.tmpl`). The student is
weaker than you, often has a shorter usable context, and follows text literally. Write for that reader:

- **Order matters.** Number the steps in the order the reference did the work that mattered (skip its
  dead ends). Each step says what to do, the exact command or file, and what the student should see if it
  worked. Add "if you see X instead, do Y" where the reference hit trouble.
- **Pass on the decisions, with reasons.** These are the choices the reference made that shaped the result:
  structure, libraries and versions, file layout, the mechanics or numbers that matter. Give the reason in
  one clause, so the student can adapt when its situation is slightly different.
- **List the traps.** Every dead end the reference hit, and every one you see the student hit, becomes a
  "don't do X, because Y" line.
- **Verification goes in the skill.** End with the exact checks the student must run before it says it is
  done, and what passing looks like. Open models often stop early or report success without checking.
- **Harness notes.** Add what this harness and model get wrong: long-running servers that block the tool
  call (start them in the background, kill them after), tools the model misuses, output it truncates.
- **Scripts beat prose.** If the student keeps fumbling a mechanical step (a build command, a check, a
  data conversion), write a script into `skill/scripts/` and tell the student to run it.
- **Do not hand over the answer.** The skill must not contain the reference's finished output, or a script
  that writes it. Short snippets (≤ ~40 lines) are fine for the few parts the student cannot get right
  otherwise; say in the ledger note why each one was needed. The point is a skill that makes the student
  able to do the task, not an installer.
- **Keep SKILL.md short**: under ~250 lines. Move long reference material to `skill/references/*.md` and
  link it from the step that needs it.

## 5. Run the student

```
python3 scripts/distill.py trial runs/<task> -k 3        # iteration N, 3 trials in tmux
python3 scripts/distill.py status runs/<task>            # tmux attach -t distill-<task> to watch
python3 scripts/distill.py wait runs/<task> --timeout 540 # repeat until all trials exit
```

Each trial gets a fresh copy of the starting state, a frozen copy of the skill in `.skill/<name>/`, and the
original prompt plus one line pointing at the skill. Nothing else. Use 3 trials per iteration: open models
are noisy, and one lucky run proves nothing. Keep working while trials run (prepare the next checks,
re-read the reference); check back every few minutes. Don't sit idle, and don't stop trials early unless
they are clearly stuck (same error repeating for 10+ minutes); if you do, say so in the verdict.

## 6. Judge each trial

```
python3 scripts/distill.py evidence runs/<task>
```

For each trial, read `evidence/trace.md` (what the student did), `evidence/checks.log`, and the screenshots,
and open the result yourself when the checks can't capture it. Compare it item by item with `acceptance.md`
and with the reference. Then find the **first divergence**: the earliest point where the student's path went
wrong in a way that explains the gap. Fixing a later symptom usually fails. Record it:

```
python3 scripts/distill.py verdict runs/<task> --iter N --trial K \
  --verdict match|close|miss|broken --score 0-10 \
  --divergence "step 4: started vite in the foreground, tool call hung for 20 min" \
  --note "what it got right / wrong" --fix "what you will change"
```

`match` means you would show this to the user as equal to the reference. Be strict: if you'd hesitate, it's `close`.

How to judge well (learned on real runs):

- **Judge craft at full resolution, side by side.** Feature parity is not "as good". In the pilot, all
  three trials had every feature and matched at thumbnail size; I scored them 8.5–9 and the user rejected
  them at once ("not even close in quality"). Up close the student worlds were sparse and plain. Capture both at
  2× scale, look at the same region of each, and list concrete craft gaps (detail per object, density,
  typography, motion) before you score.
- **The student often cannot see.** Give it a number it can check for each visual property you care about.
  In the pilot, "share of screen cells with visible detail" separated the reference (56%) from rejected
  trials (33–40%); the student skill got a script that prints it with a map of the empty regions.
- **The student's final message is not evidence.** Students routinely say "verified, all tests pass".
  Run the result yourself: the checks, then drive it by hand the way a user would (walk up, press the key,
  click the button) and look at the frame.
- **Probe the state, not only the first screen.** If the task has a core loop, check it at three points:
  start, middle and end. For apps with saved state, build a mid-way and a finished state with the student's own
  code, inject it before the page's scripts run (Playwright `add_init_script`; setting it and reloading
  fails because many pages save over it on unload), and check the UI and world show it.
- **When a check fails, suspect the check first.** Rerun it on the reference. In the pilot, three of four
  "failures" were probe bugs: a guessed storage key, Escape opening a pause menu, a dismiss click that missed.
- Compare like with like: whatever you do to a trial (live drive, injected state), do to the reference
  too, and put both in the report.

## 7. Fix the skill, then run again

Choose the fix from the kind of failure:

| what happened | fix |
|---|---|
| skipped or never read a step | move it earlier, make it shorter, add a "stop and check" gate |
| misread a step | rewrite it concretely; add an example of the expected result |
| harness/tool misuse (blocking server, wrong tool, giant output) | harness note at the top, with the exact command to use |
| could not do a hard part even when told what to do | give a short snippet, or a script in `skill/scripts/` |
| declared done without checking, or stopped early | make the verification step mandatory, with the exact command and the expected output |
| lost the thread in a long run (context) | cut prose; split into `references/` files read only at the step that needs them |
| the trial's environment was broken | fix `--setup` or `checks.sh`, not the skill; rerun and don't count the trial |

Make the changes for the failures you saw, in one edit, and say what each change targets in the verdict's
`--fix`. If the next iteration scores worse, revert the change that caused it rather than piling more on top.
Then start the next iteration (step 5).

**When to stop looping:**
- all 3 trials of an iteration are `match` → go to step 8;
- 8 iterations, or 3 in a row with no improvement in the best score → stop, go to step 8 anyway, and tell
  the user what the student still cannot do and what you tried. That is a useful result too.

## 8. The user decides

```
python3 scripts/distill.py report runs/<task>   # report.html: reference vs trials, ledger, the skill
```

Show the user the report (publish it as a private page if your environment can, otherwise open the file)
and point them at the best trial's workspace so they can run it themselves. Ask: is this session as good as
the reference? Record the answer:

```
python3 scripts/distill.py accept runs/<task> --iter N --decision pass|fail --note "<their words>"
```

On `fail`, the user's note is the most important evidence you have. Add it to `acceptance.md` and go back to
step 7. On `pass`, the skill is copied to `runs/<task>/final/<name>/`.

## 9. Deliver

Before handing over, add a short "Tested with" line at the end of the final SKILL.md (harness, model,
iterations, how many of the last trials matched), then check the skill contains no secrets, private
hostnames, or personal data copied from the reference. Install it where the student harness looks for skills,
if the user wants that:

| harness | user skills folder |
|---|---|
| omp | `~/.omp/agent/skills/<name>/` |
| pi | `~/.pi/agent/skills/<name>/` or `--skill <dir>` |
| Claude Code | `~/.claude/skills/<name>/` |
| Codex | `~/.agents/skills/<name>/` |

Report: the skill's path, the pass rate per iteration (from `distill.py ledger`), what changed between versions
and why, and anything the student still does worse than the reference.
