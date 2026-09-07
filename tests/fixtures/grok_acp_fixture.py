#!/usr/bin/env python3
"""Offline ACP wire fixture based on grok-build 72a61251fcffb464bcc687aeb5a998e5a98ec0c9.

No credentials or network. A request journal lets tests assert lifecycle and policy.
"""
import json
import os
import select
import sys
import time

model = "grok-fixture"
effort = "low"
session = "grok-session"
active = None
late_id = None


def write(value):
    print(json.dumps(value), flush=True)


def result(request_id, value):
    write({"jsonrpc": "2.0", "id": request_id, "result": value})


def options():
    return [
        {"id": "model", "name": "Model", "type": "select", "currentValue": model,
         "options": [{"value": "grok-fixture", "name": "Fixture"}, {"value": "grok-other", "name": "Other"}]},
        {"id": "reasoning_effort", "name": "Reasoning effort", "type": "select", "currentValue": effort,
         "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
    ]


def update(kind, **fields):
    write({"jsonrpc": "2.0", "method": "_x.ai/session_notification", "params": {
        "sessionId": session, "update": {"sessionUpdate": kind, **fields}}})


def finish(stop="end_turn"):
    global active
    result(active, {"stopReason": stop, "_meta": {
        "prompt_id": active, "model_id": model,
        "usage": {"inputTokens": 100, "outputTokens": 10, "totalTokens": 110,
                  "cachedReadTokens": 50, "costUsdTicks": 10000000},
        "structured_output": {"ok": True},
        "cancellation_category": "user" if stop == "cancelled" else None,
    }})
    active = None


for line in sys.stdin:
    request = json.loads(line)
    with open("grok-requests.jsonl", "a") as journal:
        journal.write(json.dumps({**request, "fixturePid": os.getpid()}) + "\n")
    method, params, request_id = request.get("method"), request.get("params", {}), request.get("id")
    if method == "initialize":
        result(request_id, {"protocolVersion": 1,
                            "agentCapabilities": {"loadSession": True},
                            "authMethods": [{"id": "cached_token", "name": "Cached"}],
                            "_meta": {"defaultAuthMethodId": "cached_token"}})
    elif method == "authenticate":
        assert params["methodId"] == "cached_token"
        result(request_id, {})
    elif method in ("session/new", "session/load"):
        assert params["_meta"]["yoloMode"] is False
        assert params["_meta"]["autoMode"] is False
        if method == "session/load":
            assert params["_meta"]["noReplay"] is True
            session = params["sessionId"]
        result(request_id, {"sessionId": session, "configOptions": options()})
    elif method == "session/set_config_option":
        assert isinstance(params["value"], dict)
        value = params["value"]["value"]
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
            effort = value
            # The prompt may finish before a configuration command is acknowledged.
            if active:
                finish()
        result(request_id, {"configOptions": options()})
    elif method == "session/prompt":
        active = request_id
        text = params["prompt"][0]["text"]
        update("subagent_spawned", subagent_id="child", description="fixture task")
        update("subagent_finished", subagent_id="child", status="completed", output="done")
        update("auto_compact_started")
        update("auto_compact_completed")
        update("future_vendor_update", value="preserve me")
        if text not in ("hold", "cancel", "permission"):
            finish()
        elif text == "permission":
            write({"jsonrpc": "2.0", "id": 17, "method": "session/request_permission",
                   "params": {"sessionId": session, "toolCall": {"toolCallId": "tool"},
                              "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}})
    elif method == "_x.ai/interject":
        result(request_id, {"status": "queued"})
        if params["text"] == "finish":
            finish()
    elif method == "session/cancel":
        if late_id:
            result(late_id, {"configOptions": options()})
            if select.select([sys.stdin], [], [], 0.05)[0]:
                unexpected = json.loads(sys.stdin.readline())
                assert unexpected.get("id") != late_id, "client responded to an RPC response"
        time.sleep(0.05)
        if active:
            finish("cancelled")
    elif method == "_x.ai/commands/list":
        assert params.get("cwd")
        result(request_id, {"commands": [{"name": "project:review", "description": "Project plugin", "input": {"hint": "[path]"}}]})
    elif method == "_x.ai/session/fork":
        assert params["sourceCwd"] == params["newCwd"]
        result(request_id, {"newSessionId": "forked-session", "parentSessionId": params["sourceSessionId"]})
    elif method is not None:
        write({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "unknown method"}})
