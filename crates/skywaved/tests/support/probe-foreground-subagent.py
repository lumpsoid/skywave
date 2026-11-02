#!/usr/bin/env python3
"""Plays with a foreground (synchronous) subagent the way a user would, and
records what the CLI says (docs/agents/claude-code.md §3.12).

usage: probe-foreground-subagent.py <scenario> <out.ndjson>

The agent works in a small recipe box: a folder of recipes. Asks are worded
the way a person would type them; the agent picks its own tools.

  wait        ask for a helper's answer and wait for it
  background  ask for a longer chore, then press "Ctrl+B" once the helper is
              busy (`background_tasks` with the Agent call's tool_use_id)
  stop        ask for the same chore, then stop the helper once it is busy
              (`stop_task` with its task id)

It writes what skywaved writes (`initialize` with `hooks: null`, one `user`
message), says yes to every permission prompt, and keeps stdin open until
45 s pass with no output and no turn open, in the fixture format of
tests/fixtures/claude-code/README.md. Scrub the output with
`record-claude.sh --scrub` before adding it as a fixture.
"""
import json
import pathlib
import subprocess
import sys
import tempfile
import threading
import time
import uuid

SCENARIOS = ("wait", "background", "stop")

RECIPES = {
    "pancakes.md": "# Pancakes\n\nflour, milk, eggs, butter, a pinch of salt\n\n"
                   "Whisk, rest 10 minutes, fry in butter.\n",
    "garlic-bread.md": "# Garlic bread\n\nbaguette, butter, garlic, parsley\n\n"
                       "Mash garlic into soft butter, spread, bake 10 minutes.\n",
    "tomato-soup.md": "# Tomato soup\n\ntomatoes, onion, garlic, olive oil, basil\n\n"
                      "Soften onion and garlic, add tomatoes, simmer, blend.\n",
    "lemon-cake.md": "# Lemon cake\n\nflour, sugar, eggs, butter, lemons\n\n"
                     "Cream butter and sugar, add eggs, zest, flour; bake 40 minutes.\n",
    "shakshuka.md": "# Shakshuka\n\neggs, tomatoes, peppers, onion, garlic, cumin\n\n"
                    "Cook the sauce, make wells, crack the eggs in, cover.\n",
}

ASKS = {
    "wait": "Could you get a helper agent to look through my recipes and "
            "tell me which ones use garlic? I'll wait for its answer.",
    "background": "Could you ask a helper agent to read my recipes one at a "
                  "time and write a two-line tasting note for each? I'll wait "
                  "for it.",
}
ASKS["stop"] = ASKS["background"]

scenario, out_path = sys.argv[1], sys.argv[2]
if scenario not in SCENARIOS:
    sys.exit(f"unknown scenario {scenario}; one of {', '.join(SCENARIOS)}")

work = pathlib.Path(tempfile.mkdtemp(prefix="skywave-recipes-"))
(work / "recipes").mkdir()
for name, text in RECIPES.items():
    (work / "recipes" / name).write_text(text)

argv = ["claude", "-p", "--input-format", "stream-json", "--output-format",
        "stream-json", "--verbose", "--permission-prompt-tool", "stdio",
        "--model", "haiku", "--setting-sources", "project",
        "--strict-mcp-config", "--session-id", str(uuid.uuid4())]
out = open(out_path, "w")
lock = threading.Lock()
t0 = time.time()


def rec(obj):
    with lock:
        out.write(json.dumps(obj) + "\n")
        out.flush()


def note(text):
    rec({"dir": "note", "text": f"t+{int(time.time() - t0)}s {text}"})


note("argv: " + " ".join(argv))
note(f"scenario: {scenario}; cwd holds recipes/ with {len(RECIPES)} files")
p = subprocess.Popen(argv, cwd=work, stdin=subprocess.PIPE,
                     stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                     text=True, bufsize=1)


def send(msg):
    rec({"dir": "in", "msg": msg})
    p.stdin.write(json.dumps(msg) + "\n")
    p.stdin.flush()


requests = 0


def control(request):
    global requests
    requests += 1
    send({"type": "control_request", "request_id": f"req_{requests}_probe",
          "request": request})


state = {"task": None, "call": None, "turn_open": False, "helper_steps": 0,
         "last_out": time.time(), "acted": False}


def reader():
    for line in p.stdout:
        state["last_out"] = time.time()
        try:
            msg = json.loads(line)
        except ValueError:
            note("non-JSON out: " + line.strip())
            continue
        rec({"dir": "out", "msg": msg})
        kind, subtype = msg.get("type"), msg.get("subtype")
        if kind == "system" and subtype == "init":
            state["turn_open"] = True
        elif kind == "result":
            state["turn_open"] = False
            note("result")
        elif (kind == "system" and subtype == "task_started"
              and msg.get("task_type") == "local_agent"
              and state["task"] is None):
            state["task"], state["call"] = msg["task_id"], msg.get("tool_use_id")
            note(f"helper started: task {state['task']}, "
                 f"is_backgrounded={msg.get('is_backgrounded')}")
        elif kind == "assistant" and msg.get("parent_tool_use_id"):
            blocks = msg.get("message", {}).get("content") or []
            if any(b.get("type") == "tool_use" for b in blocks):
                state["helper_steps"] += 1
        elif kind == "control_request":
            request = msg["request"]
            answer = {}
            if request.get("subtype") == "can_use_tool":
                answer = {"behavior": "allow",
                          "updatedInput": request.get("input", {})}
            send({"type": "control_response", "response": {
                "subtype": "success", "request_id": msg["request_id"],
                "response": answer}})
    note("stdout closed")


def stderr():
    for line in p.stderr:
        note("stderr: " + line.rstrip())


threading.Thread(target=reader, daemon=True).start()
threading.Thread(target=stderr, daemon=True).start()
control({"subtype": "initialize", "hooks": None})
send({"type": "user", "message": {"role": "user", "content": ASKS[scenario]},
      "parent_tool_use_id": None, "session_id": ""})

# In `background` and `stop`, act once the helper has taken two steps: it is
# busy, and the main turn is still waiting on it.
deadline = t0 + 300
while time.time() < deadline and p.poll() is None:
    time.sleep(0.3)
    busy = state["task"] and state["helper_steps"] >= 2
    if scenario != "wait" and not state["acted"] and busy:
        state["acted"] = True
        if scenario == "background":
            note("pressing Ctrl+B on the helper")
            control({"subtype": "background_tasks",
                     "tool_use_id": state["call"]})
        else:
            note("stopping the helper")
            control({"subtype": "stop_task", "task_id": state["task"]})
    quiet = time.time() - state["last_out"]
    if not state["turn_open"] and quiet > 45:
        note("45s quiet with no turn open")
        break

note("closing stdin")
p.stdin.close()
try:
    code = p.wait(timeout=30)
except subprocess.TimeoutExpired:
    p.kill()
    code = p.wait()
note(f"exit {code}")
