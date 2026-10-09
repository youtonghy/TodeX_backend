#!/usr/bin/env python3
"""Stands in for the Antigravity CLI (`agy`) in driver tests.

Logs its argv to argv.log next to itself, answers `agy models`, and plays
one stream-json print-mode turn chosen by the prompt text:
  FAIL <message>  a failing `result`
  HANG            init, then waits until it is killed
  anything else   text deltas, one tool step and a successful `result`
Like agy, it exits only once stdin closes after the turn.
"""
import json
import os
import sys
import time

here = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(here, "argv.log"), "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\n")

args = sys.argv[1:]
if args[:1] == ["models"]:
    print("gemini-3.8-flash-high\tGemini 3.8 Flash (High)")
    print("gemini-3.8-flash-low\tGemini 3.8 Flash (Low)")
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
