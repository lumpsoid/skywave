#!/usr/bin/env python3
"""Kill the app right after tapping Send and see what survived.

    process-death.py SESSION_TITLE DELAY_MS...

For each delay: open SESSION_TITLE, type "pd<delay> quick", tap Send, wait
DELAY_MS, SIGKILL the app process (run-as, debug build), relaunch, reopen the
session and report whether the message shows in the timeline (and with what
label), sits in the composer as a draft, or is gone. Run with the link down
(run.sh link down) to measure local durability alone; bring it up and run
log-check.py to see what reached the server.
"""

import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import burst  # noqa: E402
import ui  # noqa: E402

PKG = "dev.skywave.android"


def launch():
    ui.adb("shell", "am", "start", "-W", "-n", f"{PKG}/.MainActivity")
    time.sleep(1.5)


def open_session(title):
    for _ in range(10):
        ns = ui.nodes()
        if burst.composer(ns) is not None:
            return
        hits = [n for n in ns if n.get("text") == title]
        if hits:
            ui.adb("shell", "input", "tap", *map(str, ui.center(hits[0])))
        else:
            ui.adb("shell", "input", "keyevent", "4")
            launch()
        time.sleep(1)
    sys.exit(f"cannot open {title!r}")


def kill():
    pid = ui.adb("shell", "pidof", PKG).strip()
    if pid:
        ui.adb("shell", "run-as", PKG, "kill", "-9", pid, check=False)
    return pid


def main():
    title = sys.argv[1]
    for delay in map(int, sys.argv[2:]):
        launch()
        open_session(title)
        text = f"pd{delay} quick"
        c = burst.composer(ui.nodes())
        ui.adb("shell", "input", "tap", *map(str, ui.center(c)))
        ui.adb("shell", "input", "text", text.replace(" ", "%s"))
        s = burst.send_button(ui.nodes())
        x, y = map(str, ui.center(s))
        # one shell: tap, sleep, kill, so adb round trips stay out of the delay
        pid = ui.adb("shell", "pidof", PKG).strip()
        ui.adb("shell", f"input tap {x} {y}; sleep {delay / 1000:.3f}; run-as {PKG} kill -9 {pid}",
               check=False)
        time.sleep(1)
        launch()
        open_session(title)
        ns = ui.nodes()
        labels = [ui.label(n) for n in ns]
        c = burst.composer(ns)
        draft = c is not None and text in (c.get("text") or "")
        if text in labels:
            i = labels.index(text)
            state = labels[i + 1] if i + 1 < len(labels) else ""
            print(f"{delay:5d} ms: in timeline ({state}); draft={draft}", flush=True)
        else:
            print(f"{delay:5d} ms: NOT in timeline; draft={draft}", flush=True)


if __name__ == "__main__":
    main()
