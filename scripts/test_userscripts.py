#!/usr/bin/env python3
"""Tests the script manager in the real browser.

    python scripts/test_userscripts.py OUT_FOLDER [--exe EXE] [--real]

By default it runs the MIT scripts in samples/test/scripts on local pages and
prints PASS or FAIL for each. The exit code is 1 if any failed.

--real also runs the third party scripts in samples/userscripts, one per
empty profile, on their real sites. That folder is not in git. Most of those
sites want a login before their media shows, so "ran" there means the script
installed, started and threw no error, not that it found anything to download.
The MediaFire script runs on a local stand-in for a file page with the wall,
unless MEDIAFIRE_PAGE gives a real one. Its text is not changed.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LOCAL_SCRIPTS = os.path.join(ROOT, "samples", "test", "scripts")
REAL_SCRIPTS = os.path.join(ROOT, "samples", "userscripts")
BASE = "http://127.0.0.1:8099/test/"

REAL_PAGES = {
    "bypass-link-redirects": "https://www.stremio.com/warning#https%3A%2F%2Fexample.com%2F",
    "deviantart-download-helper": "https://www.deviantart.com/",
    "instagram-download-button": "https://www.instagram.com/instagram/",
    "media-link-extractor": "https://files.fm/",
    "mediafire-no-adblock-message": os.environ.get("MEDIAFIRE_PAGE", BASE + "wall.html"),
    "mediaset-direct-links": "https://mediasetinfinity.mediaset.it/",
    "telegram-media-downloader": "https://web.telegram.org/k/",
    "telegram-media-downloader-improved": "https://web.telegram.org/k/",
    "x-media-downloader-zip": "https://x.com/",
    "x-one-click-media-downloader": "https://x.com/",
}


def probe(exe, script, page, folder, wait, *extra):
    """Runs one script on one page and returns its report, or None."""
    shutil.rmtree(folder, ignore_errors=True)
    scripts = os.path.join(folder, "scripts")
    os.makedirs(scripts)
    shutil.copy(script, scripts)
    report = os.path.join(folder, "report.json")
    env = dict(os.environ, OSPREY_HOME=os.path.join(folder, "home"))
    args = [exe, "--probe-page", page, "--probe-wait", str(wait),
            "--probe-script-dir", scripts, "--probe-out", report] + list(extra)
    try:
        subprocess.run(args, env=env, timeout=120, capture_output=True)
    except subprocess.TimeoutExpired:
        return None
    if not os.path.exists(report):
        return None
    with open(report, encoding="utf-8") as f:
        return json.load(f)


def read(folder, name):
    try:
        with open(os.path.join(folder, name), encoding="utf-8") as f:
            return f.read()
    except OSError:
        return None


def ran_clean(r):
    return bool(r["scripts"]["ran"]) and not any(x["error"] for x in r["scripts"]["ran"])


def check_redirect(r, folder):
    return [("arrived", r["final_url"].endswith("/test/arrived.html"))]


def check_page_save(r, folder):
    return [
        ("ran", ran_clean(r)),
        ("anchor saved", read(folder, "page-anchor.txt") == "saved by an anchor"),
        ("picker saved", read(folder, "page-picker.txt") == "saved by a picker"),
        ("GM_info in the page", r["title"].endswith("finished")),
    ]


def check_wall(r, folder):
    return [("ran", ran_clean(r)), ("wall removed", r["title"] == "wall removed")]


def check_gm(r, folder):
    s = r["scripts"]
    values = next(iter(s["values"].values()), {})
    got = values.get("result", {})
    sandbox = got.get("sandbox", {})
    notes = [n["note"] for n in s["notes"]]
    return [
        ("ran", ran_clean(r)),
        ("finished", got.get("finished") is True),
        ("sandbox keeps the library from the page",
         sandbox.get("lib_seen_by_script") == "object" and sandbox.get("page_cannot_see_lib") == "undefined"),
        ("script globals stay private", sandbox.get("script_global_private") is True),
        ("page values readable", sandbox.get("page_global_readable") == "from the page"),
        ("values", got.get("values", {}).get("after_delete") == "gone"),
        ("style on a strict page", got.get("style") == "37px"),
        ("xhr text", got.get("xhr_text", {}).get("text") == "hello"),
        ("xhr json", got.get("xhr_json") == {"ok": True, "n": 5}),
        ("xhr blob", got.get("xhr_blob", {}).get("blob") is True),
        ("xhr 3 MB", got.get("xhr_big") == 3072000),
        ("xhr promise", got.get("xhr_promise") == 200),
        ("xhr 404 is a load", got.get("xhr_404") == {"on": "load", "status": 404}),
        ("xhr timeout", got.get("xhr_timeout") == "timeout"),
        ("xhr abort", got.get("xhr_abort") == "abort"),
        ("connect denied", got.get("connect_denied", {}).get("on") == "error"),
        ("connect allowed", got.get("connect_allowed") == 200),
        ("connect remembered", got.get("connect_remembered") == 200),
        ("asked exactly twice", sum("asked about" in n for n in notes) == 2),
        ("GM_download", read(folder, "gm-download.txt") == "hello"),
        ("GM_download of a blob", read(folder, "gm-blob.txt") == "from a blob"),
        ("a.download with a blob", read(folder, "anchor-blob.txt") == "from an anchor"),
        ("showSaveFilePicker", read(folder, "picker.txt") == "hello from the picker"),
        ("menu command listed", any(m["caption"] == "Test menu command" for m in s["menus"])),
        ("GM_openInTab", values.get("opened_tab_ran") is True),
    ]


LOCAL = [
    ("start-redirect", "warning.html#" + BASE.replace(":", "%3A").replace("/", "%2F") + "arrived.html",
     check_redirect, ()),
    ("page-save", "page-save.html", check_page_save, ()),
    ("wall-remover", "wall.html", check_wall, ()),
    ("gm-test", "gm-test.html", check_gm, ("--probe-answers", "deny,always")),
]


def run_local(exe, out):
    failed = 0
    for name, page, check, extra in LOCAL:
        folder = os.path.join(out, name)
        wait = 25 if name == "gm-test" else 8
        script = os.path.join(LOCAL_SCRIPTS, name + ".user.js")
        r = probe(exe, script, BASE + page, folder, wait, *extra)
        results = check(r, folder) if r else [("a report was written", False)]
        bad = [what for what, ok in results if not ok]
        print(("PASS " if not bad else "FAIL ") + name + " (%d checks)" % len(results))
        for what in bad:
            print("     failed: " + what)
        failed += bool(bad)
    return failed


def run_real(exe, out):
    if not os.path.isdir(REAL_SCRIPTS):
        print("samples/userscripts is not here, skipped")
        return
    for name, page in REAL_PAGES.items():
        folder = os.path.join(out, "real", name)
        extra = ("--probe-extra-match", "http://127.0.0.1/*") if "127.0.0.1" in page else ()
        r = probe(exe, os.path.join(REAL_SCRIPTS, name + ".user.js"), page, folder, 20, *extra)
        if not r:
            print("FAIL %s: no report" % name)
            continue
        s = r["scripts"]
        print(json.dumps({
            "name": name,
            "installed": str(r["script_installs"][0]["result"]).startswith("Ok"),
            "requires": s["installed"][0]["requires"] if s["installed"] else None,
            "ran": len(s["ran"]),
            "errors": sorted({x["error"][:200] for x in s["ran"] if x["error"]}),
            "menus": [m["caption"] for m in s["menus"]],
            "final_url": r["final_url"][:70],
            "title": r["title"][:50],
        }), flush=True)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("out")
    p.add_argument("--exe", default=os.path.join(ROOT, "target", "debug", "bm-osprey.exe"))
    p.add_argument("--real", action="store_true")
    a = p.parse_args()
    os.makedirs(a.out, exist_ok=True)
    server = subprocess.Popen([sys.executable, os.path.join(ROOT, "samples", "test", "serve_gm_test.py")])
    time.sleep(1)
    try:
        failed = run_local(a.exe, a.out)
        if a.real:
            run_real(a.exe, a.out)
    finally:
        server.terminate()
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
