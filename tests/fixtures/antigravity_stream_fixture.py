#!/usr/bin/env python3
"""Stands in for the Antigravity CLI (`agy`) in driver tests.

Logs its argv to argv.log (and the native-auto hint to auto-hint.log)
next to itself, answers `agy models` and the
`--mode auto -p=/help` probe, and plays one stream-json print-mode turn
chosen by the prompt text:
  FAIL <message>  a failing `result`
  HANG            init, then waits until it is killed
  anything else   text deltas, one tool step and a successful `result`
Like agy, it exits only once stdin closes after the turn. With
`--mode auto` it warns that Auto is unavailable when an `auto-unavailable`
file sits next to it (probe and turns) or `auto-revoked` (turns only),
followed by 200 KiB more stderr when `stderr-noise` exists. The probe logs
its cwd to probe-cwd.log, fails when `auto-probe-fails` exists, and while
`auto-probe-gate` exists appends to probe-started and waits for its removal.
"""
import json
import os
import sys
import time

here = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(here, "argv.log"), "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\n")
with open(os.path.join(here, "auto-hint.log"), "a") as log:
    log.write(os.environ.get("TODEX_AGY_NATIVE_AUTO", "-") + "\n")

args = sys.argv[1:]
if args[:1] == ["models"]:
    print("gemini-3.8-flash-high\tGemini 3.8 Flash (High)")
    print("gemini-3.8-flash-low\tGemini 3.8 Flash (Low)")
    sys.exit(0)


def marked(name):
    return os.path.exists(os.path.join(here, name))


if "--mode" in args and args[args.index("--mode") + 1] == "auto":
    probe = "-p=/help" in args
    if probe:
        with open(os.path.join(here, "probe-cwd.log"), "a") as log:
            log.write(os.getcwd() + "\n")
        if marked("auto-probe-gate"):
            with open(os.path.join(here, "probe-started"), "a") as log:
                log.write("probe\n")
            deadline = time.time() + 60
            while marked("auto-probe-gate") and time.time() < deadline:
                time.sleep(0.05)
        if marked("auto-probe-fails"):
            sys.stderr.write("error: probe crashed\n")
            sys.exit(1)
    if marked("auto-unavailable") or (not probe and marked("auto-revoked")):
        sys.stderr.write("warning: --mode auto is not available for this account; "
                         "continuing in the default mode.\n")
        if not probe and marked("stderr-noise"):
            for _ in range(200):
                sys.stderr.write("n" * 1023 + "\n")
        sys.stderr.flush()
    if probe:
        print("/help\tShow help")
        sys.exit(0)


def emit(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


conversation = "agy-conv-1"
if "--conversation" in args:
    conversation = args[args.index("--conversation") + 1]

line = sys.stdin.readline()
frame = json.loads(line)
assert frame["event"] == "user", frame
text = frame["message"]["content"]

emit({"event": "init", "conversation_id": conversation,
      "init": {"model": "fixture", "cwd": os.getcwd(), "tools": [], "permission_mode": "request-review"}})


def step(index, state, kind, **extra):
    update = {"conversation_id": conversation, "step_index": index, "state": state, "step_type": kind}
    update.update(extra)
    emit({"event": "step_update", "step_update": update})


usage = {"input_tokens": 10, "output_tokens": 2, "thinking_tokens": 0, "cache_read_tokens": 0, "total_tokens": 12}
step(0, "DONE", "user_input")
if text == "HANG":
    while True:
        time.sleep(1)
elif text.startswith("FAIL "):
    emit({"event": "result", "result": {"conversation_id": conversation, "status": "ERROR",
                                        "response": "", "error": text[5:], "usage": usage}})
else:
    step(1, "ACTIVE", "agent_response", text_delta="Hi")
    step(1, "DONE", "agent_response", text_delta=" there", usage=usage)
    step(2, "ACTIVE", "tool", tool_name="run_command",
         tool_info={"name": "run_command", "parameters": {"CommandLine": "true"}})
    step(2, "DONE", "tool", tool_name="run_command",
         tool_info={"name": "run_command", "parameters": {"CommandLine": "true"}, "output": "ok\n"})
    emit({"event": "result", "result": {"conversation_id": conversation, "status": "SUCCESS",
                                        "response": "Hi there", "num_turns": 1, "usage": usage}})
sys.stdin.read()
