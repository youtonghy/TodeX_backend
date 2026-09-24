#!/usr/bin/env python3
"""Offline ACP wire fixture for `devin acp` model discovery.

Mirrors Devin 3000.11: `thought_level` is advertised only for the currently
selected model. No credentials or network. Every request is journaled to
`journal.jsonl` next to this script. Marker files there shape timing: `stall`
makes `stall-model` never answer (sweep deadline); `slow-open` delays the
first extra `session/new` answer (late session cleanup).

Tests run it as `python acp` from the workspace, matching the driver's
`<binary> acp` spawn on every platform without a shebang or wrapper.
"""
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
STALL = os.path.exists(os.path.join(HERE, "stall"))
SLOW_OPEN = os.path.exists(os.path.join(HERE, "slow-open"))
MODELS = [f"model-{index}" for index in range(12)] + (["stall-model"] if STALL else [])
sessions = {}


def levels(model):
    # Even-numbered models think; odd-numbered ones expose no thought_level.
    if model.startswith("model-") and int(model.split("-")[1]) % 2 == 0:
        return ["medium", "high", "max"]
    return []


def options(model):
    entries = [
        {"id": "mode", "name": "Session Mode", "category": "mode", "type": "select",
         "currentValue": "accept-edits",
         "options": [{"value": "accept-edits", "name": "Code"}]},
        {"id": "model", "name": "Model", "category": "model", "type": "select",
         "currentValue": model,
         "options": [{"value": value, "name": value.upper()} for value in MODELS]},
    ]
    if levels(model):
        entries.append({"id": "thought_level", "name": "Thought Level", "type": "select",
                        "currentValue": "high",
                        "options": [{"value": value, "name": value} for value in levels(model)]})
    return entries


def write(value):
    print(json.dumps(value), flush=True)


def journal(entry):
    with open(os.path.join(HERE, "journal.jsonl"), "a") as handle:
        handle.write(json.dumps(entry) + "\n")


for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    journal({"method": method, "params": params})
    if method == "initialize":
        write({"jsonrpc": "2.0", "id": request_id, "result": {"protocolVersion": 1, "authMethods": []}})
    elif method == "session/new":
        session_id = f"session-{len(sessions)}"
        if SLOW_OPEN and len(sessions) == 1:
            time.sleep(1)
        sessions[session_id] = MODELS[0]
        write({"jsonrpc": "2.0", "id": request_id,
               "result": {"sessionId": session_id, "configOptions": options(MODELS[0])}})
        write({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": session_id,
            "update": {"sessionUpdate": "available_commands_update",
                       "availableCommands": [{"name": "review", "description": "Review changes"}]}}})
    elif method == "session/set_config_option":
        if params.get("value") == "stall-model":
            continue
        sessions[params["sessionId"]] = params["value"]
        write({"jsonrpc": "2.0", "id": request_id,
               "result": {"configOptions": options(params["value"])}})
    elif method == "session/delete":
        sessions.pop(params.get("sessionId"), None)
        write({"jsonrpc": "2.0", "id": request_id, "result": {}})
    elif request_id is not None:
        write({"jsonrpc": "2.0", "id": request_id,
               "error": {"code": -32601, "message": f"unsupported {method}"}})
