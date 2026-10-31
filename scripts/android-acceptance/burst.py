#!/usr/bin/env python3
"""Send N numbered messages from the open session screen and time the UI.

    burst.py PREFIX N [--gap S]

For each message: type into the composer, tap Send, then time how long until
the message and a "Queued"/"Sending…" label (or the plain message) show.
Prints per-message UI latency; the network should never be in that path.
Run log-check.py afterwards to check delivery (exactly once, in order).
"""

import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ui  # noqa: E402


def composer(ns):
    for n in ns:
        if n.get("class") == "android.widget.EditText":
            return n
    return None


def send_button(ns):
    for n in ns:
        if n.get("content-desc") == "Send" or n.get("text") == "Send":
            return n
    return None


def main():
    prefix, count = sys.argv[1], int(sys.argv[2])
    gap = float(sys.argv[sys.argv.index("--gap") + 1]) if "--gap" in sys.argv else 0.0
    lat = []
    for i in range(1, count + 1):
        text = f"{prefix} {i} quick"
        ns = ui.nodes()
        c = composer(ns)
        if c is None:
            sys.exit("no composer on screen")
        ui.adb("shell", "input", "tap", *map(str, ui.center(c)))
        ui.adb("shell", "input", "text", text.replace(" ", "%s"))
        s = send_button(ui.nodes())
        t0 = time.time()
        ui.adb("shell", "input", "tap", *map(str, ui.center(s)))
        while True:
            labels = [ui.label(n) for n in ui.nodes()]
            if text in labels:
                dt = time.time() - t0
                break
            if time.time() - t0 > 20:
                dt = float("inf")
                break
        c = composer(ui.nodes())
        cleared = c is not None and text not in (c.get("text") or "")
        lat.append(dt)
        print(f"{text!r}: shown after {dt:.2f}s (incl. ~1 uiautomator dump), composer cleared={cleared}", flush=True)
        time.sleep(gap)
    print(f"max {max(lat):.2f}s  mean {sum(lat) / len(lat):.2f}s")


if __name__ == "__main__":
    main()
