#!/usr/bin/env python3
"""Probes how a stream-json consumer can send a message to a background
subagent (docs/agents/claude-code.md §3.12). Re-run it on every CLI bump: the
`ctl_*` scenarios tell whether `send_task_message` is implemented yet.

usage: probe-subagent-message.py <scenario> <out.ndjson>

  ctl_live      `send_task_message` control request while the subagent runs
  ctl_done      the same, after the subagent finished
  ptu_live      a `user` line with `parent_tool_use_id` = the Agent call,
                while the subagent runs
  relay_live    a main-turn message asking the main agent to `SendMessage`
                the subagent (which works by sleeping, so it often finishes
                first and is resumed by the message)
  relay_midrun  the same, to a subagent busy with ten quick tool calls, so
                the message lands mid-run

It writes what skywaved writes (`initialize` with `hooks: null`, one `user`
message), allows every `can_use_tool`, answers other control requests with
an empty success, and keeps stdin open until 45 s pass with no output and
no turn open, in the fixture format of tests/fixtures/claude-code/README.md.
Every subagent prompt asks it to reply with the code word it was sent, or
NONE, so delivery shows in its answer. Scrub the output with
`record-claude.sh --scrub` before adding it as a fixture.
"""
import json
import subprocess
import sys
import tempfile
import threading
import time
import uuid

SCENARIOS = ("ctl_live", "ctl_done", "ptu_live", "relay_live", "relay_midrun")
WORD = "The code word is ZEBRA."
ANSWER = ("Then reply with exactly the code word somebody sent you while you "
          "worked, or NONE if nobody did.")

scenario, out_path = sys.argv[1], sys.argv[2]
if scenario not in SCENARIOS:
    sys.exit(f"unknown scenario {scenario}; one of {', '.join(SCENARIOS)}")

if scenario == "ctl_done":
    work = "Reply with exactly the word PONG."
elif scenario == "relay_midrun":
    work = ("Run the Bash command `date +%N` ten times, one call at a time, "
            "each as its own tool call. " + ANSWER)
else:
    work = ("Run the Bash command `sleep 25`. Then run the Bash command "
            "`sleep 25` again. " + ANSWER)
prompt = (f"Use the Agent tool (subagent_type general-purpose, in the "
          f"background) with this exact prompt: '{work}' Do not wait for it. "
          f"Reply STARTED.")

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
note(f"scenario: {scenario}")
p = subprocess.Popen(argv, cwd=tempfile.mkdtemp(prefix="skywave-probe-"),
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.PIPE, text=True, bufsize=1)


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


def user(text, parent=None):
    send({"type": "user", "message": {"role": "user", "content": text},
          "parent_tool_use_id": parent, "session_id": ""})


state = {"task": None, "call": None, "turn_open": False,
         "last_out": time.time(), "task_done": False, "acted": False}


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
              and msg.get("task_type") == "local_agent"):
            state["task"], state["call"] = msg["task_id"], msg.get("tool_use_id")
        elif (kind == "system" and subtype == "task_notification"
              and msg.get("task_id") == state["task"]):
            state["task_done"] = True
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


def act():
    task, call = state["task"], state["call"]
    note(f"acting on task {task} call {call}")
    if scenario.startswith("ctl_"):
        control({"subtype": "send_task_message", "task_id": task,
                 "message": WORD})
    elif scenario == "ptu_live":
        user(WORD, parent=call)
    else:
        user(f"Use SendMessage to tell the background agent {task} exactly: "
             f"'{WORD}' Then reply SENT.")


threading.Thread(target=reader, daemon=True).start()
threading.Thread(target=stderr, daemon=True).start()
control({"subtype": "initialize", "hooks": None})
user(prompt)

# Act once the turn that started the subagent ended (for ctl_done, once the
# subagent finished too): the adapter writes a message only between turns.
delay = 0.2 if scenario == "relay_midrun" else 3
deadline = t0 + 300
while time.time() < deadline and p.poll() is None:
    time.sleep(0.5)
    ready = state["task"] and not state["turn_open"]
    if scenario == "ctl_done":
        ready = ready and state["task_done"]
    if not state["acted"] and ready:
        time.sleep(delay)
        state["acted"] = True
        act()
    quiet = time.time() - state["last_out"]
    if state["acted"] and not state["turn_open"] and quiet > 45:
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
