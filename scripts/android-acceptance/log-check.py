#!/usr/bin/env python3
"""Check the server log for user messages: each exactly once, in send order.

    log-check.py DB [PREFIX]      # list user messages (text starting with PREFIX)
    log-check.py DB PREFIX N      # also assert PREFIX 1..N appear once each, in order

Reads skywaved's SQLite database read-only (works while `serve` runs).
Also prints the approvals table's latest rows.
"""

import json
import re
import sqlite3
import sys


def texts(kind):
    """User message text from an EventKind, whatever its exact nesting."""
    if isinstance(kind, dict):
        if kind.get("type") == "user_message" and isinstance(kind.get("text"), str):
            yield kind["text"]
            return
        for k, v in kind.items():
            if k in ("UserMessage", "user_message") and isinstance(v, dict):
                t = v.get("text") or v.get("content")
                if isinstance(t, str):
                    yield t
            else:
                yield from texts(v)
    elif isinstance(kind, list):
        for v in kind:
            yield from texts(v)


def main():
    db, *rest = sys.argv[1:]
    prefix = rest[0] if rest else ""
    n = int(rest[1]) if len(rest) > 1 else None
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    rows = []
    for seq, sid, at, kind in con.execute("SELECT seq, session_id, at, kind FROM events ORDER BY seq"):
        for t in texts(json.loads(kind)):
            if t.startswith(prefix):
                rows.append((seq, sid[:8], at, t))
    for r in rows:
        print(*r, sep="  ")
    if n is not None:
        seen = [int(m.group(1)) for *_, t in rows if (m := re.match(re.escape(prefix) + r"\s*(\d+)", t))]
        dup = sorted({x for x in seen if seen.count(x) > 1})
        missing = [i for i in range(1, n + 1) if i not in seen]
        ordered = seen == sorted(seen)
        print(f"messages={len(seen)} expected={n} duplicates={dup} missing={missing} in_order={ordered}")
        sys.exit(0 if not dup and not missing and ordered else 1)


if __name__ == "__main__":
    main()
