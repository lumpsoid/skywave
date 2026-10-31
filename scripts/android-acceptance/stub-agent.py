#!/usr/bin/env python3
"""A stand-in for the `claude` CLI, for driving skywaved without billing.

skywaved starts it as
  stub-agent -p --input-format stream-json --output-format stream-json \
    --verbose --permission-prompt-tool stdio [--permission-mode M] \
    [--allowedTools 'Tool(content)']... [--add-dir D]... \
    (--session-id <uuid> | --resume <uuid>) [agent.args...]
in the session workdir, and speaks Claude Code 2.1.310 stream-json on
stdin/stdout (docs/agents/claude-code.md; shapes copied from the T07
fixtures in crates/skywaved/tests/fixtures/claude-code/).

Every turn (a `user` message on stdin):
  system/init, assistant text, then
  1. Edit of <cwd>/demo.txt (multi-line diff); can_use_tool with suggestions
     addRules Edit (session), addRules Edit (localSettings), setMode
     acceptEdits (session)
  2. Bash `cargo test -p sync -- --nocapture`; can_use_tool with addRules
     Bash (session) and addRules Bash (localSettings)
  3. Write of a new file <cwd>/notes-<timestamp>-<n>.md; can_use_tool with no suggestions
  then a final text and `result`.
A message containing "quick" gets a text-only turn instead; one containing
"big N" gets a Bash call (no approval) with about N KiB of output (a blob).

Each can_use_tool waits for skywaved's control_response, then a success
tool_result (allow) or an error "denied" one (deny). A granted rule
(`updatedPermissions`) is remembered; on later calls a matching tool runs
without asking. Rules with destination localSettings/projectSettings are
written to <cwd>/.claude/settings{.local,}.json like the real CLI (only when
cwd is under /tmp/a12), so they survive a restart; session ones come back
from skywaved as --allowedTools / --permission-mode flags.

interrupt: answered at once; mid-stream ends the turn aborted_streaming, at
an approval sends control_cancel_request and ends it aborted_tools.
stdin EOF: exits when idle; at an approval, fails the approval with an
AbortError tool_result and finishes the turn first (as the real CLI does).

Every line read and written is logged to $STUB_LOG (default /tmp/a12/stub.log).
Env: STUB_DELAY (seconds between messages, default 0.5), STUB_LOG (log path),
STUB_SAFE_PREFIX (only under this prefix are files written, default /tmp/a12/).
"""

import json
import os
import queue
import random
import re
import string
import sys
import threading
import time
import uuid
from datetime import datetime, timezone

LOG_PATH = os.environ.get("STUB_LOG", "/tmp/a12/stub.log")
DELAY = float(os.environ.get("STUB_DELAY", "0.5"))
VERSION = "2.1.310"
MODEL = "claude-stub-1"
SAFE_PREFIX = os.environ.get("STUB_SAFE_PREFIX", "/tmp/a12/")
DENY_DEFAULT = "The user denied this action."
TOOLS = ["Task", "Bash", "Glob", "Grep", "Read", "Edit", "MultiEdit", "Write",
         "WebFetch", "WebSearch", "TodoWrite"]

_log_lock = threading.Lock()
_out_lock = threading.Lock()


def log(tag, text):
    stamp = datetime.now().strftime("%H:%M:%S.%f")[:-3]
    with _log_lock:
        with open(LOG_PATH, "a", encoding="utf-8") as f:
            f.write(f"{stamp} [{os.getpid()}] {tag} {text}\n")


def emit(msg):
    line = json.dumps(msg, separators=(",", ":"), ensure_ascii=False)
    with _out_lock:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()
    log("OUT", line)


def b62(n):
    alphabet = string.ascii_letters + string.digits
    return "".join(random.choice(alphabet) for _ in range(n))


def toolu_id():
    return "toolu_01" + b62(22)


def msg_id():
    return "msg_01" + b62(22)


def api_req_id():
    return "req_011C" + b62(20)


def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.") + \
        f"{datetime.now(timezone.utc).microsecond // 1000:03d}Z"


# ---------------------------------------------------------------------------
# argv

def parse_argv(argv):
    opts = {"session_id": None, "resume": False, "allowed": [],
            "mode": "default", "add_dirs": []}
    i = 0
    while i < len(argv):
        a = argv[i]
        nxt = argv[i + 1] if i + 1 < len(argv) else None
        if a == "--session-id":
            opts["session_id"] = nxt; i += 1
        elif a == "--resume":
            opts["session_id"] = nxt; opts["resume"] = True; i += 1
        elif a in ("--allowedTools", "--allowed-tools"):
            opts["allowed"].append(nxt); i += 1
        elif a == "--permission-mode":
            opts["mode"] = nxt; i += 1
        elif a == "--add-dir":
            opts["add_dirs"].append(nxt); i += 1
        elif a in ("--model", "--input-format", "--output-format",
                   "--permission-prompt-tool", "--setting-sources"):
            i += 1
        i += 1
    if not opts["session_id"]:
        opts["session_id"] = str(uuid.uuid4())
    return opts


def parse_rule_string(s):
    """'Bash(cargo test)' -> {'toolName':'Bash','ruleContent':'cargo test'}."""
    if s.endswith(")") and "(" in s:
        tool, content = s.split("(", 1)
        return {"toolName": tool, "ruleContent": content[:-1]}
    return {"toolName": s}


def rule_string(rule):
    c = rule.get("ruleContent")
    return f"{rule['toolName']}({c})" if c is not None else rule["toolName"]


# ---------------------------------------------------------------------------
# stdin reader

EOF = object()


class Input:
    def __init__(self):
        self.q = queue.Queue()
        self.eof = False
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for raw in sys.stdin:
            line = raw.strip()
            if not line:
                continue
            log("IN ", line)
            try:
                self.q.put(json.loads(line))
            except json.JSONDecodeError:
                log("ERR", "non-JSON stdin line ignored")
        log("IN ", "<EOF>")
        self.q.put(EOF)

    def get(self, timeout=None):
        """Next message, EOF, or None on timeout. EOF is sticky."""
        if self.eof:
            if timeout:
                time.sleep(timeout)
            return EOF if timeout is None else None
        try:
            m = self.q.get(timeout=timeout) if timeout is not None else self.q.get()
        except queue.Empty:
            return None
        if m is EOF:
            self.eof = True
        return m


class StreamInterrupt(Exception):
    def __init__(self, request_id):
        self.request_id = request_id


class ToolInterrupt(Exception):
    def __init__(self, request_id, tool_name, tool_use_id, tool_input):
        self.request_id = request_id
        self.tool_name = tool_name
        self.tool_use_id = tool_use_id
        self.tool_input = tool_input


# ---------------------------------------------------------------------------
# the agent

class Stub:
    def __init__(self, argv):
        self.argv = argv
        self.opts = parse_argv(argv)
        self.sid = self.opts["session_id"]
        self.cwd = os.getcwd()
        self.mode = self.opts["mode"]
        self.add_dirs = list(self.opts["add_dirs"])
        self.rules = [parse_rule_string(s) for s in self.opts["allowed"] if s]
        self.rules += self.load_settings_rules()
        self.inp = Input()
        self.queued = []
        self.turn_no = 0
        self.total_cost = 0.0
        self.write_files = self.cwd.startswith(SAFE_PREFIX)
        log("ARG", json.dumps({"argv": argv, "cwd": self.cwd,
                               "session_id": self.sid,
                               "resume": self.opts["resume"],
                               "mode": self.mode, "rules": self.rules,
                               "add_dirs": self.add_dirs}))

    # -- settings files (localSettings / projectSettings) --------------------

    def settings_path(self, destination):
        name = {"localSettings": "settings.local.json",
                "projectSettings": "settings.json"}.get(destination)
        return os.path.join(self.cwd, ".claude", name) if name else None

    def load_settings_rules(self):
        rules = []
        for dest in ("localSettings", "projectSettings"):
            path = self.settings_path(dest)
            try:
                with open(path, encoding="utf-8") as f:
                    allow = json.load(f).get("permissions", {}).get("allow", [])
                rules += [parse_rule_string(s) for s in allow]
            except (OSError, ValueError, AttributeError):
                pass
        return rules

    def persist_rules(self, destination, rules):
        path = self.settings_path(destination)
        if not path or not self.write_files:
            log("RUL", f"not persisting {destination} rules (cwd outside {SAFE_PREFIX})")
            return
        try:
            data = {}
            if os.path.exists(path):
                with open(path, encoding="utf-8") as f:
                    data = json.load(f)
            allow = data.setdefault("permissions", {}).setdefault("allow", [])
            for r in rules:
                s = rule_string(r)
                if s not in allow:
                    allow.append(s)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w", encoding="utf-8") as f:
                json.dump(data, f, indent=2)
                f.write("\n")
            log("RUL", f"wrote {path}")
        except (OSError, ValueError) as e:
            log("ERR", f"cannot persist rules to {path}: {e}")

    def apply_updates(self, updates):
        for u in updates or []:
            log("RUL", "granted updatedPermissions: " + json.dumps(u))
            t = u.get("type")
            if t == "addRules" and u.get("behavior") == "allow":
                self.rules += u.get("rules", [])
                if u.get("destination") in ("localSettings", "projectSettings"):
                    self.persist_rules(u["destination"], u.get("rules", []))
            elif t == "setMode":
                self.mode = u.get("mode", self.mode)
            elif t == "addDirectories":
                self.add_dirs += u.get("directories", [])

    def allowed_by(self, tool, content):
        """The rule/mode that lets `tool` run without asking, or None."""
        if self.mode == "bypassPermissions":
            return "permission mode bypassPermissions"
        if self.mode == "acceptEdits" and tool in ("Edit", "MultiEdit", "Write"):
            return "permission mode acceptEdits"
        for r in self.rules:
            if r.get("toolName") != tool:
                continue
            rc = r.get("ruleContent")
            if rc is None or rc == content:
                return rule_string(r)
            if rc.endswith(":*") and content and content.startswith(rc[:-2]):
                return rule_string(r)
        return None

    # -- I/O helpers ----------------------------------------------------------

    def handle_control_request(self, m, in_turn):
        """Answers a host control request; returns True for an interrupt."""
        rid = m.get("request_id", "")
        sub = (m.get("request") or {}).get("subtype")
        if sub == "initialize":
            emit(self.initialize_response(rid))
        elif sub == "interrupt":
            if not in_turn:
                emit(ack(rid))
            return True
        elif sub == "set_permission_mode":
            self.mode = (m.get("request") or {}).get("mode", self.mode)
            emit({"type": "control_response",
                  "response": {"subtype": "success", "request_id": rid, "response": {}}})
        else:
            emit({"type": "control_response",
                  "response": {"subtype": "error", "request_id": rid,
                               "error": f"Unsupported control request subtype: {sub}"}})
        return False

    def pause(self, seconds=None):
        """Sleeps, handling stdin meanwhile; raises StreamInterrupt."""
        end = time.monotonic() + (DELAY if seconds is None else seconds)
        while True:
            left = end - time.monotonic()
            if left <= 0:
                return
            m = self.inp.get(timeout=left)
            if m is None or m is EOF:
                continue
            t = m.get("type")
            if t == "control_request":
                if self.handle_control_request(m, in_turn=True):
                    raise StreamInterrupt(m.get("request_id", ""))
            elif t == "user":
                log("TRN", "user message during a turn: queued")
                self.queued.append(m)
            elif t == "control_response":
                log("TRN", "ignoring a control_response for no pending request")

    def ask(self, tool, tool_input, tool_use_id, description, suggestions):
        """Sends can_use_tool and waits; returns the response dict, or
        {'behavior':'abort'} on stdin EOF. Raises ToolInterrupt."""
        rid = str(uuid.uuid4())
        req = {"subtype": "can_use_tool", "tool_name": tool, "display_name": tool,
               "input": tool_input, "description": description}
        if suggestions:
            req["permission_suggestions"] = suggestions
        req["tool_use_id"] = tool_use_id
        emit({"type": "control_request", "request_id": rid, "request": req})
        while True:
            m = self.inp.get()
            if m is EOF:
                return {"behavior": "abort"}
            t = m.get("type")
            if t == "control_response":
                resp = m.get("response") or {}
                if resp.get("request_id") == rid:
                    if resp.get("subtype") != "success":
                        return {"behavior": "deny",
                                "message": resp.get("error", "permission error")}
                    return resp.get("response") or {}
                log("TRN", "ignoring a control_response for another request")
            elif t == "control_request":
                if self.handle_control_request(m, in_turn=True):
                    emit({"type": "control_cancel_request", "request_id": rid})
                    raise ToolInterrupt(m.get("request_id", ""), tool, tool_use_id,
                                        tool_input)
            elif t == "user":
                log("TRN", "user message during a turn: queued")
                self.queued.append(m)

    # -- message builders -----------------------------------------------------

    def initialize_response(self, rid):
        return {"type": "control_response", "response": {
            "subtype": "success", "request_id": rid, "response": {
                "commands": [], "agents": [], "output_style": "default",
                "available_output_styles": ["default"],
                "models": [{"value": "default", "displayName": "Stub (default)",
                            "description": "stub-agent, no model"}],
                "account": {"email": "user@example.com",
                            "organization": "<stub-organization>"},
                "pid": os.getpid(), "claude_code_version": VERSION,
                "current_permission_mode": self.mode,
                "session_state": "idle", "capabilities": []}}}

    def system_init(self):
        return {"type": "system", "subtype": "init", "cwd": self.cwd,
                "session_id": self.sid, "tools": TOOLS, "mcp_servers": [],
                "model": MODEL, "permissionMode": self.mode,
                "slash_commands": [], "apiKeySource": "none",
                "claude_code_version": VERSION, "output_style": "default",
                "additional_directories": self.add_dirs,
                "uuid": str(uuid.uuid4())}

    def assistant(self, mid, block):
        return {"type": "assistant", "message": {
            "model": MODEL, "id": mid, "type": "message", "role": "assistant",
            "content": [block], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 2, "cache_creation_input_tokens": 120,
                      "cache_read_input_tokens": 15000, "output_tokens": 24}},
            "parent_tool_use_id": None, "session_id": self.sid,
            "uuid": str(uuid.uuid4()), "timestamp": now_iso(),
            "request_id": api_req_id()}

    def text(self, mid, text):
        emit(self.assistant(mid, {"type": "text", "text": text}))

    def tool_use(self, mid, tid, name, tool_input):
        emit(self.assistant(mid, {"type": "tool_use", "id": tid, "name": name,
                                  "input": tool_input,
                                  "caller": {"type": "direct"}}))

    def tool_result(self, tid, content, structured, is_error=False, meta=None):
        block = {"tool_use_id": tid, "type": "tool_result", "content": content}
        if is_error:
            block["is_error"] = True
        msg = {"type": "user", "message": {"role": "user", "content": [block]},
               "parent_tool_use_id": None, "session_id": self.sid,
               "uuid": str(uuid.uuid4()), "timestamp": now_iso(),
               "tool_use_result": structured}
        if meta:
            msg["tool_result_meta"] = [dict(id=tid, **meta)]
        emit(msg)

    def user_marker(self, text):
        emit({"type": "user", "message": {"role": "user", "content": [
            {"type": "text", "text": text}]},
            "parent_tool_use_id": None, "session_id": self.sid,
            "uuid": str(uuid.uuid4()), "timestamp": now_iso()})

    def result(self, started, ok, text, terminal, stop_reason, denials, api_calls):
        cost = 0.0004 * api_calls if terminal != "aborted_streaming" else 0.0
        self.total_cost += cost
        usage = ({"input_tokens": 2 * api_calls,
                  "cache_creation_input_tokens": 120 * api_calls,
                  "cache_read_input_tokens": 15000 * api_calls,
                  "output_tokens": 60 * api_calls}
                 if cost else
                 {"input_tokens": 0, "cache_creation_input_tokens": 0,
                  "cache_read_input_tokens": 0, "output_tokens": 0})
        model_usage = ({MODEL: {"inputTokens": usage["input_tokens"],
                                "outputTokens": usage["output_tokens"],
                                "cacheReadInputTokens": usage["cache_read_input_tokens"],
                                "cacheCreationInputTokens": usage["cache_creation_input_tokens"],
                                "webSearchRequests": 0, "costUSD": cost}}
                       if cost else {})
        emit({"type": "result",
              "subtype": "success" if ok else "error_during_execution",
              "is_error": not ok,
              "duration_ms": int((time.monotonic() - started) * 1000),
              "duration_api_ms": 400 * api_calls, "num_turns": api_calls,
              "result": text, "stop_reason": stop_reason,
              "terminal_reason": terminal, "session_id": self.sid,
              "total_cost_usd": round(self.total_cost, 8), "usage": usage,
              "modelUsage": model_usage,
              "model_usage_since_last_result": model_usage,
              "permission_denials": denials, "uuid": str(uuid.uuid4())})

    # -- the turn ---------------------------------------------------------------

    def run(self):
        while True:
            m = self.queued.pop(0) if self.queued else self.inp.get()
            if m is EOF:
                log("END", "stdin closed while idle; exiting 0")
                time.sleep(0.2)
                return 0
            t = m.get("type")
            if t == "control_request":
                self.handle_control_request(m, in_turn=False)
            elif t == "control_response":
                log("TRN", "ignoring a late control_response")
            elif t == "user":
                self.turn(m)
                if self.inp.eof and not self.queued:
                    log("END", "stdin closed during the turn; exiting 0 after it")
                    return 0

    def turn(self, m):
        self.turn_no += 1
        content = (m.get("message") or {}).get("content")
        if isinstance(content, list):
            content = " ".join(b.get("text", "") for b in content if isinstance(b, dict))
        prompt = content or ""
        started = time.monotonic()
        self.denials = []
        self.calls = 1
        emit(self.system_init())
        try:
            self.pause()
            if "quick" in prompt.lower():
                self.text(msg_id(), f"Quick reply #{self.turn_no}: you said {prompt!r}.")
                self.pause(0.1)
                self.result(started, True, f"Quick reply #{self.turn_no}.", "completed",
                            "end_turn", [], 1)
                return
            big = re.search(r"\bbig (\d+)\b", prompt.lower())
            if big:
                self.big_output(int(big.group(1)), started)
                return
            self.scripted(prompt, started)
        except StreamInterrupt as e:
            emit(ack(e.request_id))
            self.user_marker("[Request interrupted by user]")
            self.result(started, False, None, "aborted_streaming", None,
                        self.denials, self.calls)
        except ToolInterrupt as e:
            emit(ack(e.request_id))
            self.tool_result(
                e.tool_use_id,
                "The user doesn't want to proceed with this tool use. The tool use "
                "was rejected (eg. if it was a file edit, the new_string was NOT "
                "written to the file). STOP what you are doing and wait for the "
                "user to tell you how to proceed.",
                "User rejected tool use", is_error=True,
                meta={"non_execution_kind": "user-rejected"})
            self.user_marker("[Request interrupted by user for tool use]")
            self.denials.append({"tool_name": e.tool_name, "tool_use_id": e.tool_use_id,
                                 "tool_input": e.tool_input})
            self.result(started, False, None, "aborted_tools", "tool_use",
                        self.denials, self.calls)

    def big_output(self, kib, started):
        """A Bash call (no approval) whose output is about `kib` KiB: a large
        blob for download tests."""
        mid = msg_id()
        self.text(mid, f"Printing about {kib} KiB of log output.")
        tid = toolu_id()
        cmd = f"cat build-{kib}k.log"
        self.tool_use(mid, tid, "Bash", {"command": cmd, "description": "Show the build log"})
        self.pause(0.3)
        line = "{:08d} compiling crate with a reasonably long log line to fill bytes\n"
        stdout = "".join(line.format(i) for i in range(kib * 1024 // 70 + 1))
        self.tool_result(tid, stdout, {"stdout": stdout, "stderr": "", "interrupted": False,
                                       "isImage": False})
        self.pause()
        self.text(msg_id(), "Done printing the log.")
        self.result(started, True, "Done printing the log.", "completed", "end_turn", [], 1)

    def gate(self, mid, name, tool_input, rule_content, description, suggestions):
        """tool_use, then permission (asked or by rule). Returns
        (tool_use_id, allowed, deny_message, meta)."""
        tid = toolu_id()
        self.tool_use(mid, tid, name, tool_input)
        self.pause(0.3)
        by = self.allowed_by(name, rule_content)
        if by:
            log("RUL", f"{name} auto-allowed by {by} (no can_use_tool)")
            self.auto_allowed.append(f"{name} ({by})")
            return tid, True, None, {"permission_decision": {"decision": "accept",
                                                             "source": "config"}}
        resp = self.ask(name, tool_input, tid, description, suggestions)
        self.calls += 1
        behavior = resp.get("behavior")
        if behavior == "allow":
            if resp.get("updatedPermissions"):
                self.apply_updates(resp["updatedPermissions"])
            else:
                log("RUL", f"{name} allowed once (no updatedPermissions)")
            return tid, True, None, {"permission_decision": {"decision": "accept",
                                                             "source": "user_temporary"}}
        if behavior == "abort":
            return tid, False, ("Tool permission request failed: AbortError: Tool "
                                "permission stream closed before response received"), \
                {"non_execution_kind": "permission-rule",
                 "permission_decision": {"decision": "reject", "source": "config"}}
        log("RUL", f"{name} denied: {resp.get('message')}")
        return tid, False, resp.get("message") or DENY_DEFAULT, \
            {"non_execution_kind": "permission-rule",
             "permission_decision": {"decision": "reject", "source": "user_reject"}}

    def denied(self, tid, name, tool_input, message, meta):
        self.denials.append({"tool_name": name, "tool_use_id": tid, "tool_input": tool_input})
        self.tool_result(tid, message, "Error: " + message, is_error=True, meta=meta)

    def scripted(self, prompt, started):
        self.auto_allowed = []
        n = self.turn_no
        demo = os.path.join(self.cwd, "demo.txt")
        old = ("fn retry_delay(attempt: u32) -> Duration {\n"
               "    Duration::from_millis(100 * attempt as u64)\n"
               "}\n")
        new = ("fn retry_delay(attempt: u32) -> Duration {\n"
               "    // Exponential backoff, capped at 30 s.\n"
               "    let ms = 100u64.saturating_mul(1 << attempt.min(8));\n"
               "    Duration::from_millis(ms.min(30_000))\n"
               "}\n")
        self.ensure_demo(demo, old, new)
        state = self.read(demo)
        if state is not None and old not in state and new in state:
            old, new = new, old  # flip back and forth across turns

        mid = msg_id()
        self.text(mid, f"Turn {n}: working on {prompt[:80]!r}. First I'll switch "
                       "the retry delay in demo.txt to exponential backoff.")
        self.pause()

        # 1. Edit -------------------------------------------------------------
        edit_in = {"file_path": demo, "old_string": old, "new_string": new,
                   "replace_all": False}
        rel = os.path.relpath(demo, self.cwd)
        edit_sugg = [
            {"type": "addRules", "rules": [{"toolName": "Edit", "ruleContent": rel}],
             "behavior": "allow", "destination": "session"},
            {"type": "addRules", "rules": [{"toolName": "Edit", "ruleContent": rel}],
             "behavior": "allow", "destination": "localSettings"},
            {"type": "setMode", "mode": "acceptEdits", "destination": "session"},
        ]
        tid, ok, msg, meta = self.gate(mid, "Edit", edit_in, rel, "demo.txt", edit_sugg)
        self.pause(0.2)
        if ok:
            before = self.read(demo) or old
            if self.write_files:
                self.write(demo, before.replace(old, new, 1))
            self.tool_result(tid, f"The file {demo} has been updated successfully.", {
                "filePath": demo, "oldString": old, "newString": new,
                "originalFile": before, "replaceAll": False, "userModified": False,
                "structuredPatch": [patch_hunk(old, new)]}, meta=meta)
        else:
            self.denied(tid, "Edit", edit_in, msg, meta)
        self.pause()

        # 2. Bash -------------------------------------------------------------
        mid = msg_id()
        self.text(mid, "Now running the sync tests to check nothing broke.")
        self.pause(0.3)
        cmd = "cargo test -p sync -- --nocapture"
        bash_in = {"command": cmd, "description": "Run the sync crate tests with output"}
        bash_sugg = [
            {"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": cmd}],
             "behavior": "allow", "destination": "session"},
            {"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "cargo test:*"}],
             "behavior": "allow", "destination": "localSettings"},
        ]
        tid, ok, msg, meta = self.gate(mid, "Bash", bash_in, cmd, bash_in["description"],
                                       bash_sugg)
        if ok:
            self.pause(1.0)  # "running"
            stdout = ("running 4 tests\n"
                      "test backoff::caps_at_thirty_seconds ... ok\n"
                      "test backoff::doubles_each_attempt ... ok\n"
                      "test sync::resumes_after_reconnect ... ok\n"
                      "test sync::drops_stale_cursor ... ok\n\n"
                      "test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; "
                      "0 filtered out; finished in 0.02s\n")
            stderr = ("   Compiling sync v0.1.0 (/tmp/a12/root/sync)\n"
                      "    Finished `test` profile [unoptimized + debuginfo] target(s) in 2.41s\n"
                      "     Running unittests src/lib.rs (target/debug/deps/sync-3f2a9c1b)\n")
            self.tool_result(tid, stdout + stderr, {
                "stdout": stdout, "stderr": stderr, "interrupted": False,
                "isImage": False}, meta=meta)
        else:
            self.denied(tid, "Bash", bash_in, msg, meta)
        self.pause()

        # 3. Write ------------------------------------------------------------
        mid = msg_id()
        self.text(mid, "Writing a short note about the change.")
        self.pause(0.3)
        note = os.path.join(self.cwd, f"notes-{datetime.now().strftime('%Y%m%d-%H%M%S')}-{n}.md")
        body = (f"# Retry backoff (turn {n})\n\n"
                "- `retry_delay` now doubles per attempt, capped at 30 s.\n"
                "- `cargo test -p sync` passes.\n\n"
                f"Prompt: {prompt}\n")
        write_in = {"file_path": note, "content": body}
        tid, ok, msg, meta = self.gate(mid, "Write", write_in, os.path.basename(note),
                                       os.path.basename(note), None)
        self.pause(0.2)
        if ok:
            if self.write_files:
                self.write(note, body)
            self.tool_result(tid, f"File created successfully at: {note}", {
                "type": "create", "filePath": note, "content": body,
                "structuredPatch": [], "originalFile": None,
                "userModified": False}, meta=meta)
        else:
            self.denied(tid, "Write", write_in, msg, meta)
        self.pause()

        # final text + result ---------------------------------------------------
        summary = f"Done with turn {n}."
        if self.denials:
            summary += " Denied: " + ", ".join(d["tool_name"] for d in self.denials) + "."
        if self.auto_allowed:
            summary += " Ran without asking: " + "; ".join(self.auto_allowed) + "."
        self.text(msg_id(), summary)
        self.pause(0.2)
        self.result(started, True, summary, "completed", "end_turn", self.denials,
                    self.calls)

    # -- files ---------------------------------------------------------------

    def ensure_demo(self, path, old, new):
        if not self.write_files or os.path.exists(path):
            return
        self.write(path, "use std::time::Duration;\n\n" + old)

    def read(self, path):
        try:
            with open(path, encoding="utf-8") as f:
                return f.read()
        except OSError:
            return None

    def write(self, path, text):
        try:
            with open(path, "w", encoding="utf-8") as f:
                f.write(text)
        except OSError as e:
            log("ERR", f"cannot write {path}: {e}")


def ack(rid):
    return {"type": "control_response", "response": {
        "subtype": "success", "request_id": rid, "response": {"still_queued": []}}}


def patch_hunk(old, new):
    o = old.splitlines()
    n = new.splitlines()
    i = 0
    while i < min(len(o), len(n)) and o[i] == n[i]:
        i += 1
    j = 0
    while j < min(len(o), len(n)) - i and o[-1 - j] == n[-1 - j]:
        j += 1
    lines = [" " + l for l in o[:i]]
    lines += ["-" + l for l in o[i:len(o) - j]]
    lines += ["+" + l for l in n[i:len(n) - j]]
    lines += [" " + l for l in o[len(o) - j:]]
    return {"oldStart": 3, "oldLines": len(o), "newStart": 3, "newLines": len(n),
            "lines": lines}


def main():
    argv = sys.argv[1:]
    if "--version" in argv:
        print(f"{VERSION} (Claude Code stub)")
        return 0
    if "--verbose" not in argv and "--output-format" in argv:
        print("Error: When using --print, --output-format=stream-json requires --verbose",
              file=sys.stderr)
        return 1
    try:
        return Stub(argv).run()
    except BrokenPipeError:
        log("END", "stdout closed")
        return 0
    except Exception as e:  # noqa: BLE001 - report like the CLI would
        log("ERR", f"crash: {e!r}")
        print(f"Error: stub-agent crashed: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
