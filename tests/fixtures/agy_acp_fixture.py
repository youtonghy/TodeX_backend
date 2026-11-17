#!/usr/bin/env python3
"""Stands in for the Antigravity CLI (`agy` 1.5.9) in tests.

Frames follow what the real `agy acp` sends (recorded 2026-11-17): both
camelCase and snake_case capability keys, `session/new` with `models`,
`modes` and `configOptions` (`model`, `thought_level`, `mode`), tool calls
with agy's non-schema `name`/`args`, and quota errors streamed as message
text on an `end_turn` without usage.

Logs its argv to argv.log and every JSON-RPC message it reads, with its cwd,
to agy-requests.jsonl, both next to itself.

  agy models                 two variants of gemini-3.8-flash
  agy ... -p=/usage          quota groups; Gemini exhausted when a
                             `quota-exhausted` file sits next to it
  agy acp                    the ACP server; `session/prompt` text picks:
    HANG            streams nothing until cancelled
    SLOW <seconds>  like anything else, after waiting that long
    FAIL <message>  a JSON-RPC error
    QUOTA           agy's quota error as message text
    PERMISSION      a write_file call that asks permission first
    TODEX_MCP       a todex_ssh MCP call that asks permission first
    IMAGE           replies with the number of image blocks
    anything else   thought, "Hi there", one run_command call, usage
  Marker files next to it: `no-auto` (the session offers no `auto` mode),
  `resume-fails` (session/resume and session/load fail).
"""
import json
import os
import sys
import time

here = os.path.dirname(os.path.abspath(__file__))
args = sys.argv[1:]
with open(os.path.join(here, "argv.log"), "a") as log:
    log.write(json.dumps(args) + "\n")


def marked(name):
    return os.path.exists(os.path.join(here, name))


if args[:1] == ["models"]:
    print("gemini-3.8-flash-high\tGemini 3.8 Flash (High)")
    print("gemini-3.8-flash-low\tGemini 3.8 Flash (Low)")
    sys.exit(0)

if "-p=/usage" in args:
    gemini = 0.0 if marked("quota-exhausted") else 0.5
    print(json.dumps({"status": "SUCCESS", "command": {"name": "usage", "data": {"groups": [
        {"name": "Gemini Models", "buckets": [
            {"id": "gemini-5h", "window": "5h", "remaining_fraction": gemini,
             "reset_time": "2030-01-01T00:00:00Z"}]}]}}}))
    sys.exit(0)

assert args[:1] == ["acp"], args

EFFORTS = ("low", "medium", "high", "xhigh")
MODELS = ["gemini-3.8-flash", "gemini-4-hydrogen"]
state = {"model": "gemini-3.8-flash", "thought": "medium", "mode": "default"}
session = "agy-session-1"
active = None
permission = None


def write(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()


def result(request_id, value):
    write({"jsonrpc": "2.0", "id": request_id, "result": value})


def error(request_id, message):
    write({"jsonrpc": "2.0", "id": request_id,
           "error": {"code": -32603, "message": message}})


def update(session_update, **fields):
    write({"jsonrpc": "2.0", "method": "session/update",
           "params": {"sessionId": session,
                      "update": {"sessionUpdate": session_update, **fields}}})


def modes():
    return ["default", "accept-edits", "plan"] + ([] if marked("no-auto") else ["auto"])


def options():
    thoughts = ["low", "medium", "high"] + (
        ["xhigh"] if state["model"] == "gemini-4-hydrogen" else [])
    return [
        {"id": "model", "name": "Model", "category": "model", "type": "select",
         "currentValue": state["model"],
         "options": [{"value": model, "name": model} for model in MODELS]},
        {"id": "thought_level", "name": "Thinking Level", "category": "thought_level",
         "type": "select", "currentValue": state["thought"],
         "options": [{"value": level, "name": level} for level in thoughts]},
        {"id": "mode", "name": "Mode", "category": "mode", "type": "select",
         "currentValue": state["mode"],
         "options": [{"value": mode, "name": mode} for mode in modes()]},
    ]


def session_response():
    return {
        "sessionId": session,
        "models": {"currentModelId": state["model"] + "-" + state["thought"],
                   "availableModels": [{"modelId": model + "-low", "name": model}
                                       for model in MODELS]},
        "modes": {"currentModeId": state["mode"],
                  "availableModes": [{"id": mode, "name": mode} for mode in modes()]},
        "configOptions": options(),
        "_meta": {"antigravity": {"tier": {"name": "Fixture", "id": "fixture-tier"}}},
    }


def tool_call(session_update, call_id, name, raw, status, **extra):
    update(session_update, toolCallId=call_id, name=name, args=raw, status=status,
           kind=extra.pop("tool_kind", "other"), title=extra.pop("title", name),
           rawInput=raw, **extra)


def finish(stop="end_turn", usage=True):
    global active
    response = {"stopReason": stop}
    if usage:
        update("usage_update", used=120, size=1048576)
        response["usage"] = {"inputTokens": 100, "outputTokens": 20, "cachedReadTokens": 0,
                             "cachedWriteTokens": 0, "thoughtTokens": 0, "totalTokens": 120}
    result(active, response)
    active = None


def ask(call_id, name, raw):
    global permission
    permission = (call_id, name, raw)
    tool_call("tool_call_update", call_id, name, raw, "pending")
    write({"jsonrpc": "2.0", "id": 99, "method": "session/request_permission", "params": {
        "sessionId": session,
        "toolCall": {"sessionUpdate": "tool_call", "toolCallId": call_id, "name": name,
                     "args": raw, "status": "pending", "kind": "other", "title": name,
                     "rawInput": raw},
        "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "deny", "name": "Deny", "kind": "reject_once"}]}})


def prompt(request_id, params):
    global active
    active = request_id
    blocks = params["prompt"]
    text = next((block["text"] for block in blocks if block.get("type") == "text"), "")
    if text == "HANG":
        return
    if text.startswith("FAIL "):
        active = None
        error(request_id, text[5:])
        return
    if text == "QUOTA":
        update("agent_message_chunk", content={"type": "text", "text":
               "\n⚠ Individual quota reached. Please upgrade your subscription to "
               "increase your limits. Resets in 1h.\n"})
        finish(usage=False)
        return
    if text == "PERMISSION":
        raw = {"Content": "hi", "Path": os.path.join(os.getcwd(), "hello.txt")}
        tool_call("tool_call", "call-write", "write_file", raw, "in_progress", tool_kind="edit")
        ask("call-write", "write_file", raw)
        return
    if text == "TODEX_MCP":
        raw = {"toolSummary": "List hosts"}
        tool_call("tool_call", "call-mcp", "mcp_todex_ssh_ssh_list_hosts", raw, "in_progress")
        ask("call-mcp", "mcp_todex_ssh_ssh_list_hosts", raw)
        return
    if text == "IMAGE":
        images = sum(1 for block in blocks if block.get("type") == "image")
        update("agent_message_chunk", content={"type": "text", "text": f"images: {images}"})
        finish()
        return
    if text.startswith("SLOW "):
        time.sleep(float(text[5:]))
    update("agent_thought_chunk", content={"type": "text", "text": "Thinking"})
    update("agent_message_chunk", content={"type": "text", "text": "Hi"})
    update("agent_message_chunk", content={"type": "text", "text": " there"})
    raw = {"CommandLine": "true", "WaitMsBeforeAsync": 5000}
    tool_call("tool_call", "call-run", "run_command", raw, "in_progress",
              tool_kind="execute", title="true")
    tool_call("tool_call_update", "call-run", "run_command", raw, "completed",
              tool_kind="execute", title="true",
              rawOutput={"output": "Command exited with code 0.", "exit_code": 0})
    finish()


for line in sys.stdin:
    request = json.loads(line)
    with open(os.path.join(here, "agy-requests.jsonl"), "a") as journal:
        journal.write(json.dumps({**request, "cwd": os.getcwd(), "pid": os.getpid()}) + "\n")
    method, params, request_id = request.get("method"), request.get("params", {}), request.get("id")
    if method == "initialize":
        capabilities = {"resume": {}, "close": {}, "load": {}, "list": {}, "additionalDirectories": {}}
        result(request_id, {
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": True, "load_session": True,
                "promptCapabilities": {"image": True, "audio": False, "embeddedContext": True},
                "sessionCapabilities": capabilities, "session_capabilities": capabilities,
                "auth": {"logout": {}},
                "_meta": {"antigravity": {"tier": True, "systemPrompt": {"append": True}}}},
            "agentInfo": {"name": "Antigravity ACP Server", "version": "fixture"},
            "authMethods": [{"id": "oauth-personal", "name": "Log in with Google"},
                            {"id": "gemini-api-key", "name": "Gemini API key"}]})
    elif method in ("session/new", "session/load", "session/resume"):
        if method != "session/new":
            if marked("resume-fails"):
                error(request_id, "Failed to start Antigravity conversation")
                continue
            session = params["sessionId"]
        # agy restores neither model nor mode on resume.
        state.update(model="gemini-3.8-flash", thought="medium", mode="default")
        result(request_id, session_response())
        update("available_commands_update",
               availableCommands=[{"name": "goal", "description": "Set a goal"}])
        update("usage_update", used=0, size=1048576)
    elif method == "session/set_config_option":
        value = params["value"]
        if params["configId"] == "model":
            base, _, effort = value.rpartition("-")
            if effort in EFFORTS and base in MODELS:
                state.update(model=base, thought=effort)
            elif value in MODELS:
                state["model"] = value
            else:
                error(request_id, f"unknown model {value}")
                continue
        elif params["configId"] == "thought_level":
            state["thought"] = value
        elif params["configId"] == "mode":
            if value not in modes():
                error(request_id, f"unknown mode {value}")
                continue
            state["mode"] = value
            update("current_mode_update", currentModeId=value)
        result(request_id, {"configOptions": options()})
        update("config_option_update", configOptions=options())
    elif method == "session/prompt":
        prompt(request_id, params)
    elif method == "session/cancel":
        if active is not None:
            finish("cancelled")
    elif method == "session/close":
        result(request_id, {})
    elif method is not None:
        write({"jsonrpc": "2.0", "id": request_id,
               "error": {"code": -32601, "message": f"Method not found: {method}"}})
    elif request_id == 99 and permission is not None and active is not None:
        call_id, name, raw = permission
        permission = None
        allowed = request.get("result", {}).get("outcome", {}).get("optionId") == "allow"
        tool_call("tool_call_update", call_id, name, raw, "completed" if allowed else "failed")
        update("agent_message_chunk", content={"type": "text",
               "text": "allowed" if allowed else "denied"})
        finish()
