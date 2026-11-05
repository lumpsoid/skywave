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
(turn it on in Settings first; the script refuses otherwise). It needs
Developer options > "Don't keep activities" on, and refuses otherwise: on
Android 12+ BACK only moves a root launcher Activity to the back, so HOME
with that option is the one way to destroy MainActivity on every version.
Set it in Developer options: `settings put global always_finish_activities`
over adb changes the setting but not the running system. Turn it off after.
It sends a "pe<time> quick" message, then:
  1. HOME: MainActivity is destroyed, the process stays (same pid) and the
     stay-connected service still runs;
  2. relaunch: same pid, the Activity attaches to the cached engine, the
     message is there, and Rust does not boot again (no new "opening the
     core" for that pid);
  3. HOME, SIGKILL the process, relaunch: a new pid that boots Rust
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


def rows(ns, title):
    """The session list's rows titled `title`: a row merges its texts into one
    label, "<title>, <state>, <workdir>, ...", with the title first."""
    return [n for n in ns
            if n.get("text") == title or (n.get("content-desc") or "").startswith(title + ", ")]


def open_session(title):
    # A cold start shows a spinner first; BACK then would leave the app.
    wait_for(lambda: rows(ns := ui.nodes(), title) or burst.composer(ns) is not None, 20)
    for _ in range(10):
        ns = ui.nodes()
        if burst.composer(ns) is not None:
            return
        hits = rows(ns, title)
        if hits:
            ui.adb("shell", "input", "tap", *map(str, ui.center(hits[0])))
        else:
            ui.adb("shell", "input", "keyevent", "4")
            launch()
        time.sleep(1)
    sys.exit(f"cannot open {title!r}")


def type_in(text):
    """Types `text` into the composer. A phone's keyboard drops what `input
    text` sends before it has come up, so the tap that raises it is waited on."""
    ui.adb("shell", "input", "tap", *map(str, ui.center(burst.composer(ui.nodes()))))
    time.sleep(1)
    ui.adb("shell", "input", "text", text.replace(" ", "%s"))


def kill():
    pid = ui.adb("shell", "pidof", PKG).strip()
    if pid:
        ui.adb("shell", "run-as", PKG, "kill", "-9", pid, check=False)
    return pid


def pid():
    return ui.adb("shell", "pidof", PKG, check=False).strip()


def activity_alive():
    """A MainActivity record in the history that is not DESTROYED. A destroyed
    one keeps its record (with its saved state, to be recreated from), so
    the record's own `state=` line says, not its presence."""
    out = ui.adb("shell", "dumpsys", "activity", "activities", check=False)
    mine = False
    for line in out.splitlines():
        if "Hist  #" in line:
            mine = ACTIVITY in line
        elif mine and line.strip().startswith("state="):
            if not line.strip().startswith("state=DESTROYED"):
                return True
            mine = False
    return False


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


def dont_keep_activities():
    out = ui.adb("shell", "settings", "get", "global", "always_finish_activities", check=False)
    return out.strip() == "1"


def leave():
    """HOME; with "Don't keep activities" on, MainActivity is destroyed."""
    ui.adb("shell", "input", "keyevent", "3")
    return wait_for(lambda: not activity_alive(), 10)


def wait_for(cond, timeout=20):
    t0 = time.time()
    while time.time() - t0 < timeout:
        if cond():
            return True
        time.sleep(0.5)
    return False


def engine(title):
    check(dont_keep_activities(), "Developer options > Don't keep activities is on")
    launch()
    check(service_running(), "Stay connected is on (service running)")
    open_session(title)
    text = f"pe{int(time.time()) % 100000} quick"
    type_in(text)
    ui.adb("shell", "input", "tap", *map(str, ui.center(burst.send_button(ui.nodes()))))
    check(wait_for(lambda: in_timeline(text)), f"{text!r} in the timeline")
    ui.adb("logcat", "-c")
    first = pid()

    check(leave(), "HOME destroyed MainActivity")
    time.sleep(2)
    check(pid() == first, f"process alive after the Activity went (pid {first})")
    check(service_running(), "service still running without an Activity")

    launch()
    check(pid() == first, f"relaunch reused the process (pid {pid()})")
    open_session(title)
    check(wait_for(lambda: in_timeline(text)), "message there after re-attach")
    check(boots(first) == 0, f"no second boot in pid {first} ({boots(first)} seen)")

    check(leave(), "HOME destroyed MainActivity again")
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
        type_in(text)
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
