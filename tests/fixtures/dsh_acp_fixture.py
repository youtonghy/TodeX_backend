#!/usr/bin/env python3
"""Offline ACP wire fixture based on DeepSeek Harness 0.2.0-rc.2 (`dsh --profile acp`).

No credentials or network. A request journal lets tests assert lifecycle and policy.
Like the real server it advertises `session/resume` but not `session/load`, groups
`model` choices by provider route, and offers `reasoning_effort` only for models
that declare reasoning.
"""
import json
import os
import sys
import time

FLASH = '["deepseek-official","deepseek-v4-flash"]'
PRO = '["deepseek-official","deepseek-v4-pro"]'
PLAIN = '["gateway","plain-chat"]'
model = FLASH
effort = "high"
sessions = set()
counter = 0
active = None
active_session = None


def write(value):
    print(json.dumps(value), flush=True)


def result(request_id, value):
    write({"jsonrpc": "2.0", "id": request_id, "result": value})


def error(request_id, code, message):
    write({"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}})


def options():
    entries = [{
        "id": "model", "name": "Model", "category": "model", "type": "select",
        "currentValue": model,
        "options": [
            {"group": "deepseek-official", "name": "DeepSeek", "options": [
                {"value": FLASH, "name": "deepseek-v4-flash"},
                {"value": PRO, "name": "DeepSeek-V4-Pro", "description": "Stronger agentic coding"}]},
            {"group": "gateway", "name": "Gateway", "options": [
                {"value": PLAIN, "name": "plain-chat"}]},
        ],
    }]
    if model != PLAIN:
        entries.append({
            "id": "reasoning_effort", "name": "Reasoning effort", "category": "thought_level",
            "type": "select", "currentValue": effort,
            "options": [{"value": "", "name": "Provider default"},
                        {"value": "off", "name": "Off"},
                        {"value": "high", "name": "High"},
                        {"value": "max", "name": "Max"}]})
    return entries


def update(session, kind, **fields):
    write({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": session, "update": {"sessionUpdate": kind, **fields}}})


def finish(stop="end_turn"):
    global active
    update(active_session, "agent_message_chunk", messageId="msg_fixture",
           content={"type": "text", "text": "ok"})
    update(active_session, "usage_update", used=120, size=1000000)
    result(active, {"stopReason": stop})
    active = None


for line in sys.stdin:
    request = json.loads(line)
    with open("dsh-requests.jsonl", "a") as journal:
        journal.write(json.dumps({**request, "fixturePid": os.getpid(), "argv": sys.argv[1:]}) + "\n")
    method, params, request_id = request.get("method"), request.get("params", {}), request.get("id")
    if method == "initialize":
        result(request_id, {"protocolVersion": 1,
                            "agentInfo": {"name": "deepseek-harness-acp", "version": "0.0.1"},
                            "agentCapabilities": {
                                "mcpCapabilities": {"http": True},
                                "promptCapabilities": {"image": False, "audio": False,
                                                       "embeddedContext": False},
                                "sessionCapabilities": {"close": {}, "list": {}, "resume": {}}},
                            "authMethods": []})
    elif method == "session/new":
        counter += 1
        session = "dsh-session-%d" % counter
        sessions.add(session)
        result(request_id, {"sessionId": session, "configOptions": options()})
    elif method == "session/resume":
        # The persisted log outlives the process; "gone" models a deleted session.
        if params["sessionId"] == "gone":
            error(request_id, -32002, "Resource not found: gone")
            continue
        sessions.add(params["sessionId"])
        result(request_id, {"configOptions": options()})
    elif method == "session/set_config_option":
        if params["configId"] == "model":
            model = params["value"]
        elif params["configId"] == "reasoning_effort":
            if model == PLAIN:
                error(request_id, -32602, "unknown session config option: reasoning_effort")
                continue
            effort = params["value"]
        else:
            error(request_id, -32602, "unknown session config option: " + params["configId"])
            continue
        result(request_id, {"configOptions": options()})
    elif method == "session/prompt":
        active, active_session = request_id, params["sessionId"]
        text = params["prompt"][0]["text"]
        if text == "permission":
            write({"jsonrpc": "2.0", "id": 17, "method": "session/request_permission",
                   "params": {"sessionId": active_session,
                              "toolCall": {"toolCallId": "call-1", "title": "bash"},
                              "options": [{"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                                          {"optionId": "reject-once", "name": "Reject", "kind": "reject_once"}]}})
        elif text != "hold":
            finish()
    elif method == "session/cancel":
        time.sleep(0.05)
        if active is not None:
            result(active, {"stopReason": "cancelled"})
            active = None
    elif method == "session/close":
        sessions.discard(params.get("sessionId"))
        result(request_id, {})
    elif method is not None:
        error(request_id, -32601, "Method not found: " + method)
    elif request_id == 17 and active is not None:
        outcome = request.get("result", {}).get("outcome", {})
        with open("dsh-permission.json", "w") as decision:
            json.dump(outcome, decision)
        finish()
