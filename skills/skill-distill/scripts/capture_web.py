#!/usr/bin/env python3
"""Start a web app, drive it in headless Chromium, and save what happened.

Use it from checks.sh so the reference and every student trial are captured the same way.

  capture_web.py --dir WORKSPACE --out OUT [--start "npx vite --port {port} --strictPort"] \
                 [--path /] [--actions actions.json] [--size 1280x800]

--start   command that serves the app; {port} is replaced with a free port. Without it the folder is
          served with a static file server (good for a plain index.html).
--actions JSON list of steps, run in order:
            {"wait": 1500}                      milliseconds
            {"key": "KeyW", "hold": 1200}       hold a key (Playwright key names)
            {"press": "KeyE"}                   tap a key
            {"click": [640, 400]}               click at viewport x,y
            {"click_text": "Start"}             click the first visible element with this text
            {"js": "window.game && game.state"} evaluate; result saved in capture.json
            {"shot": "after-walk"}              screenshot OUT/<name>.png
Writes OUT/*.png, OUT/console.log, OUT/capture.json (errors, js results, per-shot "blank" flag).
Exit code 0 if the page loaded with no uncaught errors and the first shot is not blank, else 1.
"""
import argparse
import http.client
import json
import os
import shlex
import signal
import socket
import subprocess
import sys
import time
import urllib.request
from contextlib import suppress
from pathlib import Path

DEFAULT_ACTIONS = [{"wait": 2500}, {"shot": "01-load"}, {"press": "Enter"}, {"click": [640, 420]}, {"wait": 800},
                   {"key": "KeyW", "hold": 1500}, {"key": "KeyD", "hold": 800}, {"shot": "02-moved"},
                   {"press": "KeyE"}, {"wait": 800}, {"shot": "03-interact"}]


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def wait_http(url, secs):
    end = time.time() + secs
    while time.time() < end:
        try:
            with urllib.request.urlopen(url, timeout=3):
                return True
        except (OSError, ValueError, http.client.HTTPException):
            time.sleep(0.5)
    return False


def blank(png):
    from PIL import Image, ImageStat
    with Image.open(png) as image:
        im = image.convert("L").resize((160, 100))
    return ImageStat.Stat(im).stddev[0] < 4.0   # a flat single-colour frame


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dir", required=True); ap.add_argument("--out", required=True)
    ap.add_argument("--start"); ap.add_argument("--path", default="/"); ap.add_argument("--actions")
    ap.add_argument("--size", default="1280x800"); ap.add_argument("--boot-secs", type=int, default=90)
    a = ap.parse_args()
    out = Path(a.out); out.mkdir(parents=True, exist_ok=True)
    port = free_port()
    cmd = (a.start or f"{shlex.quote(sys.executable)} -m http.server {{port}} --bind 127.0.0.1").replace("{port}", str(port))
    with open(out / "server.log", "w") as log:
        srv = subprocess.Popen(cmd, shell=True, cwd=a.dir, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    url = f"http://127.0.0.1:{port}{a.path}"
    res = {"url": url, "server": cmd, "errors": [], "console": [], "js": [], "shots": {}, "ok": False}
    try:
        if not wait_http(url, a.boot_secs):
            res["errors"].append(f"server did not answer at {url} within {a.boot_secs}s")
            return finish(out, res)
        from playwright.sync_api import Error as PlaywrightError
        from playwright.sync_api import sync_playwright
        actions = json.loads(Path(a.actions).read_text()) if a.actions else DEFAULT_ACTIONS
        w, h = map(int, a.size.split("x"))
        with sync_playwright() as p:
            b = p.chromium.launch(args=["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"])
            pg = b.new_page(viewport={"width": w, "height": h})
            pg.on("console", lambda m: res["console"].append(f"[{m.type}] {m.text}"))
            pg.on("pageerror", lambda e: res["errors"].append(str(e)))
            pg.goto(url, wait_until="load", timeout=60000)
            for st in actions:
                try:
                    if "wait" in st: pg.wait_for_timeout(st["wait"])
                    elif "key" in st:
                        pg.keyboard.down(st["key"]); pg.wait_for_timeout(st.get("hold", 500)); pg.keyboard.up(st["key"])
                    elif "press" in st: pg.keyboard.press(st["press"])
                    elif "click" in st: pg.mouse.click(*st["click"])
                    elif "click_text" in st: pg.get_by_text(st["click_text"]).first.click(timeout=3000)
                    elif "js" in st: res["js"].append({"expr": st["js"], "value": pg.evaluate(st["js"])})
                    elif "shot" in st:
                        f = out / f"{st['shot']}.png"; pg.screenshot(path=str(f)); res["shots"][f.name] = {"blank": blank(f)}
                except (PlaywrightError, OSError, ValueError, TypeError, KeyError) as e:
                    res["errors"].append(f"step {st}: {e}".splitlines()[0])
            b.close()
        first = next(iter(res["shots"].values()), {"blank": True})
        page_errors = [e for e in res["errors"] if not e.startswith("step ")]
        res["ok"] = not page_errors and not first["blank"]
    finally:
        with suppress(ProcessLookupError):
            os.killpg(srv.pid, signal.SIGTERM)
    return finish(out, res)


def finish(out, res):
    (out / "console.log").write_text("\n".join(res["console"]))
    (out / "capture.json").write_text(json.dumps(res, indent=2, default=str))
    print(json.dumps({k: res[k] for k in ("url", "ok", "errors", "shots")}, indent=1, default=str)[:3000])
    return 0 if res["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
