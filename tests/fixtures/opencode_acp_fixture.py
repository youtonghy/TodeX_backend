#!/usr/bin/env python3
"""Offline ACP wire fixture based on opencode 1.18.20 (`opencode acp`).

No credentials or network. A request journal lets tests assert lifecycle and policy.
"""
import json
import os
import select
import sys
import time

model = "opencode/fixture-model"
mode = "build"
session = "ses_fixture"
active = None
late_id = None


def write(value):
    print(json.dumps(value), flush=True)


def result(request_id, value):
    write({"jsonrpc": "2.0", "id": request_id, "result": value})


def options():
    return [
        {"id": "model", "name": "Model", "category": "model", "type": "select",
         "currentValue": model,
         "options": [{"value": "opencode/fixture-model", "name": "Fixture"},
                     {"value": "opencode/other-model", "name": "Other"}]},
        {"id": "mode", "name": "Session Mode", "category": "mode", "type": "select",
         "currentValue": mode,
         "options": [{"value": "build", "name": "build"},
                     {"value": "plan", "name": "plan"}]},
    ]


def update(kind, **fields):
    write({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": session, "update": {"sessionUpdate": kind, **fields}}})


def finish(stop="end_turn"):
    global active
    update("usage_update", used=110, size=200000, cost={"amount": 0, "currency": "USD"})
    result(active, {"stopReason": stop, "usage": {
        "inputTokens": 100, "outputTokens": 10, "totalTokens": 160,
        "cachedReadTokens": 50}, "_meta": {}})
    active = None


for line in sys.stdin:
    request = json.loads(line)
    with open("opencode-requests.jsonl", "a") as journal:
        journal.write(json.dumps({**request, "fixturePid": os.getpid()}) + "\n")
    method, params, request_id = request.get("method"), request.get("params", {}), request.get("id")
    if method == "initialize":
        result(request_id, {"protocolVersion": 1,
                            "agentCapabilities": {
                                "loadSession": True,
                                "mcpCapabilities": {"http": True, "sse": True},
                                "promptCapabilities": {"embeddedContext": True, "image": True},
                                "sessionCapabilities": {"close": {}, "fork": {}, "list": {}, "resume": {}}},
                            "authMethods": [{"id": "opencode-login", "name": "Login with opencode",
                                             "description": "Run `opencode auth login` in the terminal"}],
                            "agentInfo": {"name": "OpenCode", "version": "fixture"}})
    elif method in ("session/new", "session/load"):
        if method == "session/load":
            session = params["sessionId"]
        result(request_id, {"sessionId": session, "configOptions": options()})
        update("available_commands_update", availableCommands=[
            {"name": "project:review", "description": "Project plugin"}])
    elif method == "session/set_config_option":
        assert isinstance(params["value"], str)
        value = params["value"]
        if value == "malformed":
            result(request_id, {})
            continue
        if value == "hang":
            continue
        if value == "hang-terminal":
            finish()
            continue
        if value == "late-ack":
            late_id = request_id
            continue
        if value == "reject":
            write({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32602, "message": "unsupported value"}})
            continue
        if params["configId"] == "model":
            model = value
        else:
            mode = value
            # The prompt may finish before a configuration command is acknowledged.
            if active:
                finish()
        result(request_id, {"configOptions": options()})
    elif method == "session/prompt":
        active = request_id
        text = params["prompt"][0]["text"]
        update("agent_message_chunk", messageId="msg_fixture", content={"type": "text", "text": "ok"})
        if text not in ("hold", "cancel", "permission"):
            finish()
        elif text == "permission":
            write({"jsonrpc": "2.0", "id": 17, "method": "session/request_permission",
                   "params": {"sessionId": session, "toolCall": {"toolCallId": "tool"},
                              "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}})
    elif method == "session/cancel":
        if late_id:
            result(late_id, {"configOptions": options()})
            if select.select([sys.stdin], [], [], 0.05)[0]:
                unexpected = json.loads(sys.stdin.readline())
                assert unexpected.get("id") != late_id, "client responded to an RPC response"
        time.sleep(0.05)
        if active:
            finish("cancelled")
    elif method == "session/close":
        result(request_id, {})
    elif method is not None:
        write({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "unknown method"}})
