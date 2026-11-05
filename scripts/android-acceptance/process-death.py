#!/usr/bin/env python3
"""Kill the app and see what survived.

    process-death.py SESSION_TITLE DELAY_MS...
    process-death.py --engine SESSION_TITLE

Delay mode: for each delay, open SESSION_TITLE, type "pd<delay> quick", tap
Send, wait DELAY_MS, SIGKILL the app process (run-as, debug build), relaunch,
reopen the session and report whether the message shows in the timeline (and
with what label), sits in the composer as a draft, or is gone. Run with the
link down (run.sh link down) to measure local durability alone; bring it up
and run log-check.py to see what reached the server.

Engine mode (--engine): checks the app-owned engine with Stay connected on
(turn it on in Settings first; the script refuses otherwise). It sends a
"pe<time> quick" message, then:
  1. BACK out until MainActivity is destroyed: the process stays (same pid)
     and the stay-connected service still runs;
  2. relaunch: same pid, the Activity attaches to the cached engine, the
     message is there, and Rust does not boot again (no new "opening the
     core" for that pid);
  3. BACK out, SIGKILL the process, relaunch: a new pid that boots Rust
     exactly once, cold-starts from the replica with the message intact, and
     brings the service back.
Exits non-zero on the first failed check. Needs a debug build (run-as).
"""

import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import burst  # noqa: E402
import ui  # noqa: E402

PKG = "dev.skywave.gui"
ACTIVITY = f"{PKG}/.MainActivity"
SERVICE = f"{PKG}/.StayConnectedService"
BOOT_LINE = "opening the core"


def launch():
    ui.adb("shell", "am", "start", "-W", "-n", ACTIVITY)
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


def pid():
    return ui.adb("shell", "pidof", PKG, check=False).strip()


def activity_alive():
    out = ui.adb("shell", "dumpsys", "activity", "activities", check=False)
    return any(ACTIVITY in line and "ActivityRecord" in line for line in out.splitlines())


def service_running():
    out = ui.adb("shell", "dumpsys", "activity", "services", PKG, check=False)
    return SERVICE in out


def boots(of_pid):
    """How many times Rust opened the core in process `of_pid` since the log was cleared."""
    out = ui.adb("logcat", "-d", f"--pid={of_pid}", "-s", "skywave-gui", check=False)
    return sum(BOOT_LINE in line for line in out.splitlines())


def in_timeline(text):
    return any(ui.label(n) == text for n in ui.nodes())


def check(ok, what):
    print(f"{'ok  ' if ok else 'FAIL'} {what}", flush=True)
    if not ok:
        sys.exit(1)


def back_out():
    """BACK until MainActivity is destroyed (Dart pops its routes first)."""
    for _ in range(8):
        if not activity_alive():
            return True
        ui.adb("shell", "input", "keyevent", "4")
        time.sleep(1)
    return not activity_alive()


def wait_for(cond, timeout=20):
    t0 = time.time()
    while time.time() - t0 < timeout:
        if cond():
            return True
        time.sleep(0.5)
    return False


def engine(title):
    launch()
    check(service_running(), "Stay connected is on (service running)")
    open_session(title)
    text = f"pe{int(time.time()) % 100000} quick"
    c = burst.composer(ui.nodes())
    ui.adb("shell", "input", "tap", *map(str, ui.center(c)))
    ui.adb("shell", "input", "text", text.replace(" ", "%s"))
    ui.adb("shell", "input", "tap", *map(str, ui.center(burst.send_button(ui.nodes()))))
    check(wait_for(lambda: in_timeline(text)), f"{text!r} in the timeline")
    ui.adb("logcat", "-c")
    first = pid()

    check(back_out(), "BACK destroyed MainActivity")
    time.sleep(2)
    check(pid() == first, f"process alive after the Activity went (pid {first})")
    check(service_running(), "service still running without an Activity")

    launch()
    check(pid() == first, f"relaunch reused the process (pid {pid()})")
    open_session(title)
    check(wait_for(lambda: in_timeline(text)), "message there after re-attach")
    check(boots(first) == 0, f"no second boot in pid {first} ({boots(first)} seen)")

    check(back_out(), "BACK destroyed MainActivity again")
    kill()
    check(wait_for(lambda: pid() != first, 10), f"pid {first} killed")
    launch()
    second = pid()
    check(second not in ("", first), f"relaunch started a new process (pid {second})")
    open_session(title)
    check(wait_for(lambda: in_timeline(text)), "message intact after cold start")
    check(boots(second) == 1, f"one boot in pid {second} ({boots(second)} seen)")
    check(wait_for(service_running), "service back after cold start")


def main():
    if sys.argv[1:2] == ["--engine"]:
        engine(sys.argv[2])
        return
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
