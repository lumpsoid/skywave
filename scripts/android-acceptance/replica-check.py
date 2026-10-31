#!/usr/bin/env python3
"""Compare the phone's local replica with the server log: no gaps, no dups.

    replica-check.py SERVER_DB [OUT_DIR]

Pulls files/skywave/core.db (+ -wal, -shm) from the debug app with run-as
into OUT_DIR (default /tmp/skywave-acceptance/phone), then compares the
event seqs held on the phone with the server's events table.
Env: ANDROID_SERIAL (default emulator-5600).
"""

import os
import sqlite3
import subprocess
import sys

SERIAL = os.environ.get("ANDROID_SERIAL", "emulator-5600")


def pull(out):
    os.makedirs(out, exist_ok=True)
    for name in ("core.db", "core.db-wal", "core.db-shm"):
        data = subprocess.run(
            ["adb", "-s", SERIAL, "exec-out", "run-as", "dev.skywave.android", "cat",
             f"files/skywave/{name}"], capture_output=True).stdout
        with open(os.path.join(out, name), "wb") as f:
            f.write(data)
    return os.path.join(out, "core.db")


def seqs(con, table):
    cols = [r[1] for r in con.execute(f"PRAGMA table_info({table})")]
    col = "seq" if "seq" in cols else None
    if col is None:
        return None
    return [r[0] for r in con.execute(f"SELECT {col} FROM {table} ORDER BY {col}")]


def main():
    server_db = sys.argv[1]
    out = sys.argv[2] if len(sys.argv) > 2 else "/tmp/skywave-acceptance/phone"
    phone = sqlite3.connect(pull(out))
    tables = [r[0] for r in phone.execute("SELECT name FROM sqlite_master WHERE type='table'")]
    print("phone tables:", ", ".join(tables))
    server = sqlite3.connect(f"file:{server_db}?mode=ro", uri=True)
    srv = [r[0] for r in server.execute("SELECT seq FROM events ORDER BY seq")]
    for t in ("events",):
        s = seqs(phone, t) or [0]
        dups = len(s) - len(set(s))
        gaps = sorted(set(range(1, max(s) + 1)) - set(s))
        missing = sorted(set(srv) - set(s))
        print(f"{t}: {len(s)} rows, max seq {max(s)}, duplicates {dups}, gaps {gaps[:10]}"
              f"{'…' if len(gaps) > 10 else ''}; server max {srv[-1]}, server seqs missing on phone {len(missing)}")


if __name__ == "__main__":
    main()
