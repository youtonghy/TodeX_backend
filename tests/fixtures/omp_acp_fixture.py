#!/usr/bin/env python3
"""Offline ACP wire fixture based on omp 18.13.2 (`omp acp --approval-mode always-ask`).

Frame shapes follow a capture of the real binary: `mode`/`model`/`thinking`
config options, `session/request_permission` with JSON-RPC id 0 and the four
ACP option kinds, `_session/steering` answering `outcome`, and prompt usage in
`inputTokens`/`outputTokens`. No credentials or network. A request journal
lets tests assert lifecycle and policy.
"""
import json
import os
import sys

model = "mock/fixture"
mode = "default"
thinking = "off"
session = "01a1816a-fixture"
active = None
permission_pending = False
elicitation_pending = None


def write(value):
    print(json.dumps(value), flush=True)


def result(request_id, value):
    write({"jsonrpc": "2.0", "id": request_id, "result": value})


def options():
    levels = [{"value": "off", "name": "Off"}]
    if model == "anthropic/thinker":
        levels += [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]
    levels.append({"value": "auto", "name": "Auto", "description": "Auto-detect per prompt"})
    return [
        {"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": mode,
         "options": [{"value": "default", "name": "Default"}, {"value": "plan", "name": "Plan"}]},
        {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": model,
         "options": [{"value": "mock/fixture", "name": "Fixture", "description": "mock/fixture"},
                     {"value": "anthropic/thinker", "name": "Thinker", "description": "anthropic/thinker"}]},
        {"id": "thinking", "name": "Thinking", "category": "thought_level", "type": "select",
         "currentValue": thinking, "options": levels},
    ]


def update(session_update, **fields):
    write({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": session, "update": {"sessionUpdate": session_update, **fields}}})


def finish(stop="end_turn"):
    global active
    update("usage_update", size=128000, used=10)
    result(active, {"stopReason": stop, "usage": {
        "inputTokens": 20, "outputTokens": 6, "totalTokens": 26}})
    active = None


for line in sys.stdin:
    request = json.loads(line)
    with open("omp-requests.jsonl", "a") as journal:
        journal.write(json.dumps({**request, "fixturePid": os.getpid(), "argv": sys.argv[1:],
                                  "piAgentDir": os.environ.get("PI_CODING_AGENT_DIR"),
                                  "noTitle": os.environ.get("PI_NO_TITLE")}) + "\n")
    method, params, request_id = request.get("method"), request.get("params", {}), request.get("id")
    if method == "initialize":
        result(request_id, {"protocolVersion": 1,
                            "agentInfo": {"name": "omp", "title": "omp", "version": "18.13.2"},
                            "authMethods": [{"id": "agent", "name": "Use existing local credentials"}],
                            "agentCapabilities": {
                                "loadSession": True,
                                "mcpCapabilities": {"http": True, "sse": True},
                                "promptCapabilities": {"embeddedContext": True, "image": True},
                                "sessionCapabilities": {"list": {}, "fork": {}, "resume": {}, "close": {}}},
                            "_meta": {"steering": {"supported": True}}})
    elif method in ("session/new", "session/load", "session/resume"):
        if method != "session/new":
            session = params["sessionId"]
        result(request_id, {"sessionId": session, "configOptions": options(),
                            "modes": {"availableModes": [{"id": "default", "name": "Default"},
                                                         {"id": "plan", "name": "Plan"}],
                                      "currentModeId": mode}})
        update("available_commands_update", availableCommands=[
            {"name": "compact", "description": "Compact the session"},
            {"name": "effort", "description": "Set or show reasoning effort", "input": {"hint": "[level]"}}])
    elif method == "session/fork":
        result(request_id, {"sessionId": "01a1816a-forked", "configOptions": options()})
    elif method == "session/set_config_option":
        value = params["value"]
        if params["configId"] == "model":
            model = value
            thinking = "off"
        elif params["configId"] == "thinking":
            if value not in [level["value"] for level in options()[2]["options"]]:
                write({"jsonrpc": "2.0", "id": request_id, "error": {
                    "code": -32603, "message": "Internal error",
                    "data": {"details": f"Unknown ACP thinking level: {value}"}}})
                continue
            thinking = value
        elif params["configId"] == "mode":
            mode = value
            update("current_mode_update", currentModeId=mode)
        result(request_id, {"configOptions": options()})
    elif method == "_session/steering":
        result(request_id, {"outcome": "injected" if active else "promptRequired", "queue": "steering"})
        if active:
            update("agent_message_chunk", messageId="msg_steer",
                   content={"type": "text", "text": "steered"})
            finish()
    elif method == "session/prompt":
        active = request_id
        text = params["prompt"][0]["text"]
        update("agent_message_chunk", messageId="msg_fixture", content={"type": "text", "text": "ok"})
        if text == "spawn":
            # A `task` call running two subagents inline (omp 18.13.3 capture
            # with async.enabled: false); Beta fails.
            def progress(status_a, status_b):
                return [{"index": 0, "id": "Alpha", "agent": "task", "status": status_a, "task": "Complete:\n\nList files",
                         "assignment": "List files", "description": "list", "lastIntent": "Listing", "recentTools": [],
                         "toolCount": 1, "tokens": 13, "cost": 0, "durationMs": 40},
                        {"index": 1, "id": "Beta", "agent": "explore", "status": status_b, "task": "Read docs",
                         "assignment": "Read docs", "recentTools": [], "toolCount": 0, "tokens": 0, "cost": 0, "durationMs": 0}]
            update("tool_call", toolCallId="call_task", name="task", title="task", kind="other", status="pending",
                   rawInput={"tasks": [{"name": "Alpha"}, {"name": "Beta"}]})
            for statuses in (("running", "pending"), ("running", "pending"), ("completed", "running")):
                update("tool_call_update", toolCallId="call_task", status="in_progress",
                       rawOutput={"content": [{"type": "text", "text": "Running..."}],
                                  "details": {"results": [], "progress": progress(*statuses)}})
            update("tool_call_update", toolCallId="call_task", status="completed",
                   rawOutput={"content": [{"type": "text", "text": "<task-result>"}], "details": {"progress": [], "results": [
                       {"index": 0, "id": "Alpha", "agent": "task", "exitCode": 0, "aborted": False, "output": "a.txt b.txt",
                        "durationMs": 88, "tokens": 13},
                       {"index": 1, "id": "Beta", "agent": "explore", "exitCode": 1, "aborted": False, "output": "",
                        "error": "model refused", "durationMs": 20, "tokens": 2}]}})
            finish()
        elif text in ("approve", "question"):
            # Tool approvals other than bash, and `ask` questions, arrive as
            # form elicitations (omp 18.13.3 capture).
            elicitation_pending = text
            enum = (["Approve", "Approve write for this session", "Deny"] if text == "approve"
                    else ["Postgres", "SQLite"])
            message = "Allow tool: write\nPath: w.txt\nContent:\nhi" if text == "approve" else "Which database?"
            write({"jsonrpc": "2.0", "id": 1, "method": "elicitation/create", "params": {
                "mode": "form", "sessionId": session, "message": message,
                "requestedSchema": {"type": "object", "properties": {"value": {"type": "string", "enum": enum}},
                                    "required": ["value"]}}})
        elif text == "permission":
            permission_pending = True
            # omp numbers its own requests from 0.
            write({"jsonrpc": "2.0", "id": 0, "method": "session/request_permission", "params": {
                "sessionId": session,
                "toolCall": {"toolCallId": "call_1", "title": "echo hello > note.txt", "kind": "execute",
                             "status": "pending", "rawInput": {"command": "echo hello > note.txt"}},
                "options": [{"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
                            {"optionId": "allow_always", "name": "Always allow", "kind": "allow_always"},
                            {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
                            {"optionId": "reject_always", "name": "Always reject", "kind": "reject_always"}]}})
        elif text not in ("hold",):
            finish()
    elif method == "session/cancel":
        if active:
            finish("cancelled")
    elif method == "session/close":
        result(request_id, {})
    elif method is not None:
        write({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "Method not found"}})
    elif request_id == 1 and elicitation_pending and active is not None:
        elicitation_pending = None
        finish()
    elif request_id == 0 and permission_pending and active is not None:
        # The client answered the permission request; the tool ran.
        permission_pending = False
        update("tool_call", toolCallId="call_1", title="$ echo hello > note.txt", kind="execute",
               status="pending", rawInput={"command": "echo hello > note.txt"})
        update("tool_call_update", toolCallId="call_1", status="completed",
               content=[{"type": "content", "content": {"type": "text", "text": "(no output)"}}])
        finish()
