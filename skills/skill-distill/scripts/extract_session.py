#!/usr/bin/env python3
"""Turn a coding-agent session log into a compact, readable reference.

Supported logs (auto-detected):
  claude      Claude Code transcript   ~/.claude/projects/<slug>/<session>.jsonl
  codex       Codex rollout            ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl
  codex-exec  `codex exec --json` stdout
  omp         oh-my-pi / pi session    ~/.omp/agent/sessions/<slug>/*.jsonl

Usage:
  extract_session.py LOG [-o reference.md] [--json reference.json]
  extract_session.py --current claude|codex|omp --cwd DIR [-o ...]   # newest session for DIR

System, developer and harness-injected text (AGENTS.md, CLAUDE.md, <system-reminder>, environment
context) is dropped: it is private to the machine and is not part of what the agent did.
Long tool outputs are clipped in the summary only; the original log is never modified.
"""
import argparse
import glob
import json
import os
import re
import sys
from pathlib import Path

CLIP_CMD = 600      # chars of a command / tool input shown per step
CLIP_OUT = 400      # chars of a tool output shown per step
INJECTED = re.compile(r"<(system-reminder|environment_context|user_instructions|permissions instructions|"
                      r"collaboration_mode|skill|turn_aborted)[^>]*>.*?</\1>", re.DOTALL)
INJECTED_PREFIX = ("# AGENTS.md instructions", "<environment_context>", "<user_instructions>", "<INSTRUCTIONS>",
                   "Caveat: The messages below", "<command-name>", "<local-command-stdout>")


def clip(s, n):
    s = s if isinstance(s, str) else json.dumps(s, ensure_ascii=False)
    s = s.strip()
    return s if len(s) <= n else s[:n] + f" …[+{len(s) - n} chars]"


def clean_user(text):
    text = INJECTED.sub("", text or "").strip()
    return "" if text.startswith(INJECTED_PREFIX) else text


def read_jsonl(path):
    out = []
    with open(path, encoding="utf-8", errors="replace") as session:
        for line in session:
            line = line.strip()
            if line:
                try:
                    out.append(json.loads(line))
                except json.JSONDecodeError:
                    pass
    return out


def detect(rows):
    for o in rows[:50]:
        t = o.get("type")
        if t in ("session_meta", "response_item", "turn_context"):
            return "codex"
        if t in ("thread.started", "item.completed", "item.started", "turn.started"):
            return "codex-exec"
        if t == "session" and "version" in o or t == "model_change":
            return "omp"
        if t in ("user", "assistant") and "message" in o:
            return "claude"
    raise SystemExit("unknown session format")


def text_of(content):
    if isinstance(content, str):
        return content
    parts = []
    for b in content or []:
        if isinstance(b, dict):
            if b.get("type") in ("text", "input_text", "output_text"):
                parts.append(b.get("text", ""))
        elif isinstance(b, str):
            parts.append(b)
    return "\n".join(parts)


# Each parser yields events: ("user", text) ("assistant", text) ("tool", name, input) ("result", output, is_error)

def parse_claude(rows):
    for o in rows:
        if o.get("isMeta") or o.get("isSidechain"):
            continue
        m = o.get("message") or {}
        c = m.get("content")
        if o.get("type") == "user":
            if isinstance(c, str):
                t = clean_user(c)
                if t:
                    yield ("user", t)
                continue
            for b in c or []:
                if b.get("type") == "tool_result":
                    yield ("result", text_of(b.get("content")), bool(b.get("is_error")))
                elif b.get("type") == "text":
                    t = clean_user(b.get("text"))
                    if t:
                        yield ("user", t)
        elif o.get("type") == "assistant":
            for b in c or []:
                if b.get("type") == "text" and b.get("text", "").strip():
                    yield ("assistant", b["text"])
                elif b.get("type") == "tool_use":
                    yield ("tool", b.get("name"), b.get("input"))


def parse_codex(rows):
    for o in rows:
        if o.get("type") != "response_item":
            continue
        p = o.get("payload") or {}
        t = p.get("type")
        if t == "message":
            role = p.get("role")
            txt = text_of(p.get("content"))
            if role == "user":
                txt = clean_user(txt)
                if txt:
                    yield ("user", txt)
            elif role == "assistant" and txt.strip():
                yield ("assistant", txt)
        elif t in ("function_call", "custom_tool_call", "local_shell_call"):
            args = p.get("arguments") or p.get("input") or p.get("action")
            if isinstance(args, str):
                try:
                    args = json.loads(args)
                except json.JSONDecodeError:
                    pass
            yield ("tool", p.get("name") or t, args)
        elif t in ("function_call_output", "custom_tool_call_output", "local_shell_call_output"):
            out = p.get("output")
            yield ("result", text_of(out) if isinstance(out, list) else out, False)


def parse_codex_exec(rows):
    for o in rows:
        if o.get("type") != "item.completed":
            continue
        it = o.get("item") or {}
        t = it.get("type")
        if t == "agent_message":
            yield ("assistant", it.get("text", ""))
        elif t == "command_execution":
            yield ("tool", "shell", {"command": it.get("command")})
            yield ("result", it.get("aggregated_output", ""), it.get("exit_code") not in (0, None))
        elif t == "file_change":
            yield ("tool", "file_change", {"changes": it.get("changes")})
            yield ("result", it.get("status", ""), it.get("status") == "failed")
        elif t in ("mcp_tool_call", "web_search", "todo_list"):
            yield ("tool", t, {k: v for k, v in it.items() if k not in ("id", "type")})


def parse_omp(rows):
    for o in rows:
        if o.get("type") != "message":
            continue
        m = o.get("message") or {}
        role = m.get("role")
        if role == "user":
            t = clean_user(text_of(m.get("content")))
            if t:
                yield ("user", t)
        elif role == "assistant":
            for b in m.get("content") or []:
                if b.get("type") == "text" and b.get("text", "").strip():
                    yield ("assistant", b["text"])
                elif b.get("type") == "toolCall":
                    yield ("tool", b.get("name"), b.get("arguments"))
        elif role == "toolResult":
            yield ("result", text_of(m.get("content")), bool(m.get("isError")))


PARSERS = {"claude": parse_claude, "codex": parse_codex, "codex-exec": parse_codex_exec, "omp": parse_omp}

PATH_KEYS = ("file_path", "path", "filePath", "notebook_path")


def files_from(name, args):
    files = set()
    if isinstance(args, dict):
        for k in PATH_KEYS:
            if isinstance(args.get(k), str) and name and re.search(r"write|edit|patch|create|notebook", name, re.IGNORECASE):
                files.add(args[k])
        for ch in args.get("changes") or []:
            if isinstance(ch, dict) and ch.get("path"):
                files.add(ch["path"])
    blob = args if isinstance(args, str) else json.dumps(args) if args else ""
    for m in re.finditer(r"\*\*\* (?:Add|Update) File: (\S+)", blob):
        files.add(m.group(1))
    return files


def summarize(path, fmt=None):
    rows = read_jsonl(path)
    fmt = fmt or detect(rows)
    events = list(PARSERS[fmt](rows))
    task = next((e[1] for e in events if e[0] == "user"), "")
    finals = [e[1] for e in events if e[0] == "assistant"]
    steps, files, n_tools, n_err = [], set(), 0, 0
    for e in events:
        if e[0] == "user" and e[1] != task:
            steps.append(f"**user:** {clip(e[1], 1500)}")
        elif e[0] == "assistant":
            steps.append(f"**agent:** {clip(e[1], 1200)}")
        elif e[0] == "tool":
            n_tools += 1
            files |= files_from(e[1], e[2])
            arg = e[2]
            if isinstance(arg, dict) and ("command" in arg or "cmd" in arg):
                arg = arg.get("command") or arg.get("cmd")
            steps.append(f"`{e[1]}` {clip(arg, CLIP_CMD)}")
        elif e[0] == "result":
            if e[2]:
                n_err += 1
            if steps:
                steps[-1] += f"\n    → {'ERROR ' if e[2] else ''}{clip(e[1] or '', CLIP_OUT)}"
    return {
        "source": str(path), "format": fmt, "task": task,
        "final": finals[-1] if finals else "",
        "user_turns": sum(1 for e in events if e[0] == "user"),
        "tool_calls": n_tools, "tool_errors": n_err,
        "files_written": sorted(files), "steps": steps,
    }


def to_markdown(s):
    out = [f"# Session reference ({s['format']})", "",
           f"tool calls: {s['tool_calls']} · tool errors: {s['tool_errors']} · user turns: {s['user_turns']}", "",
           "## Task (first user message)", "", s["task"], "",
           "## Final answer", "", s["final"], "",
           "## Files written", ""] + [f"- {f}" for f in s["files_written"]] + ["", "## Steps", ""]
    out += [f"{i + 1}. {st}" for i, st in enumerate(s["steps"])]
    return "\n".join(out) + "\n"


def current(kind, cwd):
    cwd = os.path.realpath(cwd)
    home = Path.home()
    if kind == "claude":
        slug = re.sub(r"[^A-Za-z0-9]", "-", cwd)
        cands = glob.glob(str(home / ".claude/projects" / slug / "*.jsonl"))
    elif kind == "omp":
        cands = glob.glob(str(home / ".omp/agent/sessions/*/*.jsonl"))
        cands = [c for c in cands if _first_cwd(c) == cwd]
    else:
        cands = sorted(glob.glob(str(home / ".codex/sessions/*/*/*/rollout-*.jsonl")), key=os.path.getmtime)[-200:]
        cands = [c for c in cands if _first_cwd(c) == cwd]
    if not cands:
        raise SystemExit(f"no {kind} session found for {cwd}")
    return max(cands, key=os.path.getmtime)


def _first_cwd(path):
    try:
        with open(path) as f:
            for _ in range(20):
                o = json.loads(f.readline() or "{}")
                c = (o.get("payload") or {}).get("cwd") or o.get("cwd")
                if c:
                    return os.path.realpath(c)
    except (OSError, ValueError, TypeError, AttributeError):
        return None
    return None


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("log", nargs="?")
    ap.add_argument("--current", choices=["claude", "codex", "omp"])
    ap.add_argument("--cwd", default=os.getcwd())
    ap.add_argument("--format", choices=list(PARSERS))
    ap.add_argument("-o", "--out")
    ap.add_argument("--json")
    a = ap.parse_args()
    log = a.log or (current(a.current, a.cwd) if a.current else None)
    if not log:
        ap.error("give LOG or --current")
    s = summarize(log, a.format)
    md = to_markdown(s)
    if a.out:
        Path(a.out).write_text(md)
        print(f"wrote {a.out} ({s['tool_calls']} tool calls, {len(s['files_written'])} files) from {log}")
    else:
        sys.stdout.write(md)
    if a.json:
        Path(a.json).write_text(json.dumps(s, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
