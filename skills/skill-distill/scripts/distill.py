#!/usr/bin/env python3
"""skill-distill run manager: the deterministic half of the teacher/student loop.

The teacher (the agent following SKILL.md) decides what goes in the skill and judges results.
This script does the bookkeeping that must not drift between iterations:

  init      create a run: reference summary, frozen prompt, starting state, empty student skill
  trial     freeze the current skill as iteration N and start K student sessions in tmux
  status    show which trials are still running
  wait      block until an iteration's trials exit (or timeout)
  evidence  collect what each trial produced: trace summary, files, diff, checks.sh output
  verdict   record the teacher's judgement of one trial in the ledger
  ledger    print the ledger
  report    write report.html: reference vs trials, side by side, for the user's pass/fail call
  accept    record the user's decision and copy the accepted skill to final/

Run layout (RUN is any directory, e.g. runs/<task>):
  run.json          settings          prompt.md        the original task prompt, verbatim
  reference/        reference.md, reference.json, evidence/
  skill/            the student skill being edited (SKILL.md + scripts/ ...)
  checks.sh         optional task checks; gets WORKSPACE, OUT; exit 0 = pass
  iters/NN/skill/   frozen copy of the skill used for iteration NN
  iters/NN/tK/      workspace/, trace/, prompt.md, run.sh, exit_code, evidence/, verdict.json
  ledger.jsonl      one line per verdict, and the user's final decision
"""
import argparse
import datetime as dt
import html
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ADAPTERS = HERE.parent / "adapters"
TEMPLATE = HERE.parent / "templates" / "student-skill" / "SKILL.md.tmpl"
EXTRACT = HERE / "extract_session.py"
SKILL_POINTER = ("\n\n---\nA skill for this task is at `.skill/{name}/SKILL.md` in this folder. "
                 "Read all of it before you start, and follow it.\n")


def now():
    return dt.datetime.now().astimezone().isoformat(timespec="seconds")


def sh(cmd, **kw):
    kw.setdefault("timeout", 600)
    return subprocess.run(cmd, shell=isinstance(cmd, str), capture_output=True, text=True,
                          check=kw.pop("check", False), **kw)


def load(run):
    return json.loads((Path(run) / "run.json").read_text())


def save(run, cfg):
    (Path(run) / "run.json").write_text(json.dumps(cfg, indent=2))


def iter_dir(run, n):
    return Path(run) / "iters" / f"{n:02d}"


def last_iter(run):
    d = Path(run) / "iters"
    ns = [int(p.name) for p in d.iterdir() if p.name.isdigit()] if d.exists() else []
    return max(ns) if ns else 0


def make_workspace(cfg, dest):
    """Copy the starting state into dest. base = git repo + ref, or 'empty'."""
    dest = Path(dest)
    base = cfg["base"]
    if base["kind"] == "empty":
        dest.mkdir(parents=True)
        sh(["git", "init", "-q"], cwd=dest)
        sh(["git", "-c", "user.name=skill-distill", "-c", "user.email=skill-distill@localhost",
            "commit", "-q", "--allow-empty", "-m", "start"], cwd=dest)
    else:
        r = sh(["git", "clone", "-q", "--no-hardlinks", base["path"], str(dest)])
        if r.returncode:
            raise SystemExit(f"clone failed: {r.stderr}")
        r = sh(["git", "checkout", "-q", base["ref"]], cwd=dest)
        if r.returncode:
            raise SystemExit(f"checkout {base['ref']} failed: {r.stderr}")
        sh(["git", "remote", "remove", "origin"], cwd=dest)
    if cfg.get("setup"):
        r = sh(cfg["setup"], cwd=dest, timeout=1800)
        if r.returncode:
            raise SystemExit(f"setup failed in {dest}:\n{r.stdout}\n{r.stderr}")


def cmd_init(a):
    run = Path(a.run).resolve()
    run.mkdir(parents=True, exist_ok=True)
    if (run / "run.json").exists() and not a.force:
        raise SystemExit(f"{run}/run.json exists (use --force)")
    adapter, _, model = a.student.partition(":")
    if not (ADAPTERS / f"{adapter}.sh").exists():
        raise SystemExit(f"no adapter {adapter}.sh in {ADAPTERS}")
    if a.base == "empty":
        base = {"kind": "empty"}
    else:
        path, _, ref = a.base.partition("@")
        path = str(Path(path).resolve())
        ref = ref or sh(["git", "rev-parse", "HEAD"], cwd=path).stdout.strip()
        base = {"kind": "git", "path": path, "ref": ref}
    name = a.name or "reproduce-" + run.name
    cfg = {"created": now(), "name": name, "adapter": adapter, "model": model,
           "student_args": a.student_args or "", "env": dict(kv.split("=", 1) for kv in a.env or []),
           "base": base, "setup": a.setup or "", "tmux": a.tmux or f"distill-{run.name}",
           "timeout_min": a.timeout_min, "reference_log": str(Path(a.reference).resolve()) if a.reference else "",
           "reference_workspace": str(Path(a.reference_workspace).resolve()) if a.reference_workspace else ""}
    if Path(a.prompt).resolve() != (run / "prompt.md").resolve():
        shutil.copy(a.prompt, run / "prompt.md")
    (run / "reference").mkdir(exist_ok=True)
    if a.reference:
        r = sh([sys.executable, str(EXTRACT), a.reference, "-o", str(run / "reference/reference.md"),
                "--json", str(run / "reference/reference.json")])
        print(r.stdout.strip() or r.stderr.strip())
    skill = run / "skill"
    if not skill.exists():
        skill.mkdir()
        (skill / "SKILL.md").write_text(TEMPLATE.read_text().replace("{{name}}", name)
                                        .replace("{{model}}", model).replace("{{harness}}", adapter))
    save(run, cfg)
    print(f"run ready: {run}\n  student: {adapter} / {model}\n  skill:   {skill}/SKILL.md (edit this)")


def cmd_trial(a):
    run = Path(a.run).resolve()
    cfg = load(run)
    n = a.iter or last_iter(run) + 1
    it = iter_dir(run, n)
    if it.exists():
        raise SystemExit(f"{it} exists; use a new --iter")
    shutil.copytree(run / "skill", it / "skill")
    session = cfg["tmux"]
    if sh(["tmux", "has-session", "-t", session], timeout=10).returncode:
        sh(["tmux", "new-session", "-d", "-s", session, "-n", "ledger", f"tail -F {run}/ledger.jsonl"], timeout=10)
    prompt = (run / "prompt.md").read_text().rstrip() + SKILL_POINTER.format(name=cfg["name"])
    for k in range(1, a.k + 1):
        t = it / f"t{k}"
        make_workspace(cfg, t / "workspace")
        dest = t / "workspace" / ".skill" / cfg["name"]
        shutil.copytree(it / "skill", dest)
        with open(t / "workspace" / ".git" / "info" / "exclude", "a") as f:
            f.write("\n.skill/\n")
        (t / "trace").mkdir()
        (t / "prompt.md").write_text(prompt)
        env = {"WORKSPACE": t / "workspace", "PROMPT_FILE": t / "prompt.md", "MODEL": cfg["model"],
               "TRACE_DIR": t / "trace", "STUDENT_ARGS": cfg["student_args"], "TRACK_EXEMPT": "1", **cfg["env"]}
        exports = "\n".join(f"export {k}={json.dumps(str(v))}" for k, v in env.items())
        secs = int(cfg["timeout_min"] * 60)
        (t / "run.sh").write_text(f"""#!/usr/bin/env bash
{exports}
date -Iseconds > {t}/started_at
perl -e 'alarm {secs}; exec @ARGV' bash {ADAPTERS}/{cfg['adapter']}.sh 2>&1 | tee {t}/stdout.log
echo ${{PIPESTATUS[0]}} > {t}/exit_code
date -Iseconds > {t}/ended_at
echo "== trial done (exit $(cat {t}/exit_code)) =="
""")
        win = f"i{n:02d}-t{k}"
        r = sh(["tmux", "new-window", "-d", "-t", session, "-n", win, f"bash {t}/run.sh; sleep 86400"], timeout=10)
        if r.returncode:
            raise SystemExit(f"tmux failed: {r.stderr}")
        print(f"started {session}:{win}  ->  {t}")
    print(f"watch: tmux attach -t {session}")


def trial_dirs(run, n):
    return sorted(p for p in iter_dir(run, n).glob("t*") if p.is_dir())


def state(t):
    if (t / "exit_code").exists():
        return f"exit {(t / 'exit_code').read_text().strip()}"
    return "running" if (t / "started_at").exists() else "pending"


def cmd_status(a):
    run = Path(a.run).resolve()
    n = a.iter or last_iter(run)
    for t in trial_dirs(run, n):
        log = t / "stdout.log"
        tail = log.read_text(errors="replace").strip().splitlines()[-1:] if log.exists() else []
        print(f"i{n:02d}/{t.name}: {state(t):10} {tail[0][:120] if tail else ''}")


def cmd_wait(a):
    run = Path(a.run).resolve()
    n = a.iter or last_iter(run)
    deadline = time.time() + a.timeout
    while True:
        ts = trial_dirs(run, n)
        if ts and all((t / "exit_code").exists() for t in ts):
            cmd_status(a)
            return
        if time.time() > deadline:
            cmd_status(a)
            raise SystemExit(2)
        time.sleep(15)


def collect(ws, out, checks, trace=None):
    out.mkdir(parents=True, exist_ok=True)
    files = sh("git ls-files --others --cached --exclude-standard | grep -v '^.skill/' | head -400", cwd=ws).stdout
    (out / "files.txt").write_text(files)
    (out / "diffstat.txt").write_text(sh("git add -A -n . >/dev/null; git diff --stat HEAD; git status --short | head -200",
                                         cwd=ws).stdout)
    if trace and trace.exists() and trace.stat().st_size:
        r = sh([sys.executable, str(EXTRACT), str(trace), "-o", str(out / "trace.md"),
                "--json", str(out / "trace.json")])
        if r.returncode:
            (out / "trace.md").write_text(f"could not parse trace: {r.stderr}")
    result = None
    if checks.exists():
        env = {**os.environ, "WORKSPACE": str(ws), "OUT": str(out)}
        try:
            r = subprocess.run(["bash", str(checks)], cwd=ws, env=env, capture_output=True, text=True, timeout=1800, check=False)
            (out / "checks.log").write_text(r.stdout + "\n--- stderr ---\n" + r.stderr)
            result = r.returncode
        except subprocess.TimeoutExpired:
            (out / "checks.log").write_text("checks.sh timed out after 1800 s")
            result = 124
    (out / "summary.json").write_text(json.dumps({"files": len(files.splitlines()), "checks_exit": result}))
    return result


def cmd_evidence(a):
    run = Path(a.run).resolve()
    cfg = load(run)
    checks = run / "checks.sh"
    if a.reference:
        ws = Path(a.reference if isinstance(a.reference, str) else cfg["reference_workspace"])
        rc = collect(ws, run / "reference" / "evidence", checks)
        print(f"reference: checks exit {rc}  ->  {run}/reference/evidence")
        return
    n = a.iter or last_iter(run)
    for t in trial_dirs(run, n):
        rc = collect(t / "workspace", t / "evidence", checks, t / "trace" / "session.jsonl")
        tj = t / "evidence" / "trace.json"
        s = json.loads(tj.read_text()) if tj.exists() else {}
        print(f"i{n:02d}/{t.name}: {state(t)}  checks exit {rc}  tool calls {s.get('tool_calls', '?')}  "
              f"errors {s.get('tool_errors', '?')}  ->  {t}/evidence")


def ledger_add(run, row):
    with open(Path(run) / "ledger.jsonl", "a") as f:
        f.write(json.dumps({"at": now(), **row}) + "\n")


def cmd_verdict(a):
    run = Path(a.run).resolve()
    t = iter_dir(run, a.iter) / f"t{a.trial}"
    row = {"kind": "verdict", "iter": a.iter, "trial": a.trial, "verdict": a.verdict, "score": a.score,
           "first_divergence": a.divergence or "", "note": a.note or "", "fix": a.fix or ""}
    (t / "verdict.json").write_text(json.dumps(row, indent=2))
    ledger_add(run, row)
    print(f"recorded i{a.iter:02d}/t{a.trial}: {a.verdict} {a.score}")


def rows(run):
    p = Path(run) / "ledger.jsonl"
    return [json.loads(l) for l in p.read_text().splitlines() if l.strip()] if p.exists() else []


def cmd_ledger(a):
    print(f"{'iter':>4} {'trial':>5} {'verdict':8} {'score':>5}  first divergence / note")
    for r in rows(a.run):
        if r["kind"] == "verdict":
            print(f"{r['iter']:>4} {r['trial']:>5} {r['verdict']:8} {r['score']:>5}  {r['first_divergence'] or r['note']}"[:200])
        else:
            print(f"user decision on iter {r.get('iter')}: {r.get('decision')}  {r.get('note', '')}")


def imgs(d):
    return sorted([*Path(d).glob("*.png"), *Path(d).glob("*.jpg")]) if Path(d).exists() else []


def cmd_report(a):
    run = Path(a.run).resolve()
    cfg = load(run)
    n = a.iter or last_iter(run)
    e = html.escape
    ref_ev = run / "reference" / "evidence"

    def panel(title, ev, extra=""):
        pics = "".join(f'<figure><img src="{e(os.path.relpath(p, run))}"><figcaption>{e(p.name)}</figcaption></figure>'
                       for p in imgs(ev))
        checks = (ev / "checks.log").read_text(errors="replace")[-3000:] if (ev / "checks.log").exists() else "(no checks.log)"
        files = (ev / "files.txt").read_text()[:3000] if (ev / "files.txt").exists() else ""
        return (f"<section><h3>{e(title)}</h3>{extra}<div class=pics>{pics or '<p>(no screenshots)</p>'}</div>"
                f"<details><summary>checks.log</summary><pre>{e(checks)}</pre></details>"
                f"<details><summary>files</summary><pre>{e(files)}</pre></details></section>")

    trials = []
    for t in trial_dirs(run, n):
        v = json.loads((t / "verdict.json").read_text()) if (t / "verdict.json").exists() else {}
        extra = (f"<p><b>{e(v.get('verdict', 'not judged'))}</b> {e(str(v.get('score', '')))} "
                 f"{e(v.get('note', ''))}</p><p>workspace: <code>{e(str(t / 'workspace'))}</code></p>")
        trials.append(panel(f"iteration {n}, {t.name} ({state(t)})", t / "evidence", extra))
    led = "".join(f"<tr><td>{r.get('iter')}</td><td>{r.get('trial', '')}</td><td>{e(str(r.get('verdict', r.get('decision'))))}</td>"
                  f"<td>{e(str(r.get('score', '')))}</td><td>{e(r.get('first_divergence', '') or r.get('note', ''))}</td></tr>"
                  for r in rows(run))
    skill = (iter_dir(run, n) / "skill" / "SKILL.md").read_text()
    page = f"""<title>skill-distill report</title><style>
:root{{--bg:#fff;--fg:#111;--muted:#666;--line:#ccc;--code:#f4f4f4}}
@media (prefers-color-scheme:dark){{:root:not([data-theme="light"]){{--bg:#111;--fg:#ddd;--muted:#999;--line:#444;--code:#222;color-scheme:dark}}}}
:root[data-theme="dark"]{{--bg:#111;--fg:#ddd;--muted:#999;--line:#444;--code:#222;color-scheme:dark}}
body{{font:14px/1.5 system-ui,sans-serif;max-width:1200px;margin:0 auto;padding-block:16px;padding-left:max(16px,env(safe-area-inset-left,0px));padding-right:max(16px,env(safe-area-inset-right,0px));background:var(--bg);color:var(--fg)}}
h1{{font-size:20px}} h2{{font-size:16px;border-bottom:1px solid var(--line);margin-top:28px}} h3{{font-size:14px}}
.pics{{display:flex;flex-wrap:wrap;gap:8px}} figure{{margin:0;width:280px;max-width:100%}} img{{width:100%;border:1px solid var(--line)}}
figcaption{{font-size:12px;color:var(--muted)}} pre,code{{background:var(--code)}} pre{{padding:8px;overflow:auto;max-height:360px;white-space:pre-wrap}}
table{{border-collapse:collapse;width:100%}} td,th{{border:1px solid var(--line);padding:4px 6px;text-align:left;vertical-align:top}}
.tbl{{overflow-x:auto}} .cols{{display:grid;grid-template-columns:1fr;gap:12px}} @media(min-width:900px){{.cols{{grid-template-columns:1fr 1fr}}}}
</style>
<h1>skill-distill: {e(cfg['name'])}</h1>
<p>student: <code>{e(cfg['adapter'])} / {e(cfg['model'])}</code> · iteration {n} · generated {now()}</p>
<p><b>Your call:</b> is the student's session as good as the reference? Record it with
<code>distill.py accept {e(str(run))} --iter {n} --decision pass|fail --note "..."</code></p>
<h2>Task</h2><pre>{e((run / 'prompt.md').read_text())}</pre>
<div class=cols>{panel('reference', ref_ev)}<div>{''.join(trials)}</div></div>
<h2>Ledger</h2><div class=tbl><table><tr><th>iter</th><th>trial</th><th>verdict</th><th>score</th><th>note</th></tr>{led}</table></div>
<h2>Skill used in iteration {n}</h2><pre>{e(skill)}</pre>
"""
    out = run / "report.html"
    out.write_text(page)
    print(out)


def cmd_accept(a):
    run = Path(a.run).resolve()
    ledger_add(run, {"kind": "decision", "iter": a.iter, "decision": a.decision, "note": a.note or ""})
    if a.decision == "pass":
        dest = run / "final" / load(run)["name"]
        if dest.exists():
            shutil.rmtree(dest)
        shutil.copytree(iter_dir(run, a.iter) / "skill", dest)
        print(f"accepted; skill copied to {dest}")
    else:
        print("recorded fail; revise skill/ and start the next iteration")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("init")
    p.add_argument("run"); p.add_argument("--prompt", required=True)
    p.add_argument("--student", required=True, help="adapter:model, e.g. omp:local/deepseek-v4.1-flash")
    p.add_argument("--base", required=True, help="'empty' or /path/to/git/repo[@ref] (the state the reference started from)")
    p.add_argument("--reference", help="reference session log")
    p.add_argument("--reference-workspace", help="where the reference's result lives (for evidence)")
    p.add_argument("--name"); p.add_argument("--student-args"); p.add_argument("--env", action="append")
    p.add_argument("--setup", help="shell command run in every fresh workspace before the student starts")
    p.add_argument("--tmux"); p.add_argument("--timeout-min", type=float, default=90)
    p.add_argument("--force", action="store_true")
    p = sub.add_parser("trial"); p.add_argument("run"); p.add_argument("--iter", type=int); p.add_argument("-k", type=int, default=3)
    for c in ("status", "evidence", "report"):
        p = sub.add_parser(c); p.add_argument("run"); p.add_argument("--iter", type=int)
        if c == "evidence":
            p.add_argument("--reference", nargs="?", const=True, default=False,
                           help="collect evidence for the reference workspace instead")
    p = sub.add_parser("wait"); p.add_argument("run"); p.add_argument("--iter", type=int)
    p.add_argument("--timeout", type=int, default=540)
    p = sub.add_parser("verdict"); p.add_argument("run"); p.add_argument("--iter", type=int, required=True)
    p.add_argument("--trial", type=int, required=True)
    p.add_argument("--verdict", required=True, choices=["match", "close", "miss", "broken"])
    p.add_argument("--score", type=float, required=True, help="0-10 against the reference")
    p.add_argument("--divergence"); p.add_argument("--note"); p.add_argument("--fix")
    p = sub.add_parser("ledger"); p.add_argument("run")
    p = sub.add_parser("accept"); p.add_argument("run"); p.add_argument("--iter", type=int, required=True)
    p.add_argument("--decision", required=True, choices=["pass", "fail"]); p.add_argument("--note")
    a = ap.parse_args()
    globals()["cmd_" + a.cmd](a)


if __name__ == "__main__":
    main()
