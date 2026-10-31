#!/usr/bin/env python3
"""Drive the Skywave app over adb for acceptance passes.

    ui.py dump                     # visible nodes: text / content-desc / bounds
    ui.py tap TEXT [--nth N]       # tap the node whose text or desc contains TEXT
    ui.py wait TEXT [--timeout S]  # wait until TEXT shows; prints seconds waited
    ui.py gone TEXT [--timeout S]  # wait until TEXT is gone
    ui.py type TEXT                # type into the focused field
    ui.py shot NAME                # screenshot to $SHOTS/NAME.png
    ui.py has TEXT                 # exit 0 if TEXT shows now
    ui.py watch [TEXT]             # print labels on every change until TEXT shows ($IGNORE hides some)

Env: ANDROID_SERIAL (default emulator-5600), SHOTS (default /tmp/skywave-acceptance/shots).
"""

import os
import re
import subprocess
import sys
import time
import xml.etree.ElementTree as ET

SERIAL = os.environ.get("ANDROID_SERIAL", "emulator-5600")
SHOTS = os.environ.get("SHOTS", "/tmp/skywave-acceptance/shots")


def adb(*args, check=True):
    return subprocess.run(["adb", "-s", SERIAL, *args], check=check,
                          capture_output=True, text=True).stdout


def nodes():
    for _ in range(3):
        out = adb("exec-out", "uiautomator", "dump", "/dev/tty", check=False)
        start = out.find("<?xml")
        end = out.rfind("</hierarchy>")
        if start >= 0 and end >= 0:
            root = ET.fromstring(out[start:end + len("</hierarchy>")])
            return list(root.iter("node"))
        time.sleep(0.5)
    return []


def label(n):
    return " | ".join(x for x in (n.get("text"), n.get("content-desc")) if x)


def center(n):
    x1, y1, x2, y2 = map(int, re.findall(r"\d+", n.get("bounds")))
    return (x1 + x2) // 2, (y1 + y2) // 2


def find(text, ns=None):
    ns = nodes() if ns is None else ns
    return [n for n in ns if text in label(n)]


def main():
    cmd, *rest = sys.argv[1:] or ["dump"]
    timeout = 30.0
    nth = 0
    if "--timeout" in rest:
        i = rest.index("--timeout")
        timeout = float(rest[i + 1])
        del rest[i:i + 2]
    if "--nth" in rest:
        i = rest.index("--nth")
        nth = int(rest[i + 1])
        del rest[i:i + 2]
    arg = " ".join(rest)
    if cmd == "dump":
        for n in nodes():
            if label(n):
                flags = "C" if n.get("clickable") == "true" else " "
                flags += "F" if n.get("focused") == "true" else " "
                print(f"{flags} {n.get('bounds'):24} {label(n)}")
    elif cmd == "tap":
        ns = nodes()
        hits = [n for n in ns if arg in (n.get("text"), n.get("content-desc"))] or find(arg, ns)
        if len(hits) <= nth:
            sys.exit(f"no node with {arg!r}")
        x, y = center(hits[nth])
        adb("shell", "input", "tap", str(x), str(y))
    elif cmd in ("wait", "gone"):
        t0 = time.time()
        while time.time() - t0 < timeout:
            present = bool(find(arg))
            if present == (cmd == "wait"):
                print(f"{time.time() - t0:.1f}s")
                return
            time.sleep(0.3)
        sys.exit(f"timeout: {arg!r} {'never showed' if cmd == 'wait' else 'still shown'}")
    elif cmd == "watch":
        # Print the screen's labels each time they change, until ARG shows
        # (empty ARG: until timeout). Labels in $IGNORE (|-separated) are hidden.
        ignore = [s for s in os.environ.get("IGNORE", "").split("|") if s]
        t0 = time.time()
        last = None
        while time.time() - t0 < timeout:
            labels = [label(n) for n in nodes() if label(n)]
            shown = [l for l in labels if not any(s in l for s in ignore)]
            if shown != last:
                print(f"{time.time() - t0:6.1f}s  " + " · ".join(shown), flush=True)
                last = shown
            if arg and any(arg in l for l in labels):
                return
            time.sleep(0.3)
        if arg:
            sys.exit(f"timeout: {arg!r} never showed")
    elif cmd == "has":
        sys.exit(0 if find(arg) else 1)
    elif cmd == "type":
        adb("shell", "input", "text", arg.replace(" ", "%s"))
    elif cmd == "shot":
        os.makedirs(SHOTS, exist_ok=True)
        path = os.path.join(SHOTS, f"{arg}.png")
        with open(path, "wb") as f:
            f.write(subprocess.run(["adb", "-s", SERIAL, "exec-out", "screencap", "-p"],
                                   check=True, capture_output=True).stdout)
        print(path)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
