"""Opt-in real CLI approval probe; uses only a local fake API, never a paid model.

Run: python3 tests/fixtures/claude_permission_probe.py [claude-binary]
The CLI writes only to a temporary workspace after an explicit allow response.
"""
from contextlib import nullcontext
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def probe(binary, allow, mode="default", expect_prompt=True, directory=None, resume=None):
    with (tempfile.TemporaryDirectory(prefix="todex-claude-permission-") if directory is None else nullcontext(directory)) as directory:
        target = Path(directory) / "probe.txt"
        class Handler(BaseHTTPRequestHandler):
            message_count = 0
            def log_message(self, *args):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if "count_tokens" in self.path:
                    payload = json.dumps({"input_tokens": 1}).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(payload)
                    return
                done = Handler.message_count > 0
                Handler.message_count += 1
                content = ({"type": "text", "text": "Finished."} if done else {
                    "type": "tool_use", "id": "toolu_permission_probe", "name": "Bash",
                    "input": {"command": "printf approved > probe.txt", "description": "Create the isolated approval probe file"},
                })
                response = {"id": "msg_probe", "type": "message", "role": "assistant", "model": body.get("model"),
                            "content": [content], "stop_reason": "end_turn" if done else "tool_use", "stop_sequence": None,
                            "usage": {"input_tokens": 1, "output_tokens": 1}}
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream" if body.get("stream") else "application/json")
                self.end_headers()
                if not body.get("stream"):
                    self.wfile.write(json.dumps(response).encode())
                    return
                events = [
                    ("message_start", {"message": {**response, "content": [], "stop_reason": None}}),
                    ("content_block_start", {"index": 0, "content_block": {**content, **({"input": {}} if not done else {"text": ""})}}),
                    ("content_block_delta", {"index": 0, "delta": {"type": "text_delta", "text": "Finished."} if done else {"type": "input_json_delta", "partial_json": json.dumps(content["input"])}}),
                    ("content_block_stop", {"index": 0}),
                    ("message_delta", {"delta": {"stop_reason": response["stop_reason"], "stop_sequence": None}, "usage": {"output_tokens": 1}}),
                    ("message_stop", {}),
                ]
                for event, data in events:
                    self.wfile.write(f"event: {event}\ndata: {json.dumps({'type': event, **data})}\n\n".encode())
                self.wfile.flush()

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        env = {k: v for k, v in os.environ.items() if not k.startswith(("ANTHROPIC_", "CLAUDE_"))}
        env.update(ANTHROPIC_API_KEY="local-fake-key", ANTHROPIC_BASE_URL=f"http://127.0.0.1:{server.server_port}",
                   CLAUDE_CONFIG_DIR=str(Path(directory) / "config"), CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1")
        args = [binary, "-p", "--bare", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
                "--permission-prompts", "host", "--permission-prompt-tool", "stdio", "--permission-mode", mode,
                "--model", "sonnet", "--tools", "default", "--strict-mcp-config"]
        if resume:
            args.extend(["--resume", resume])
        session_id = None
        process = subprocess.Popen(args, cwd=directory, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        messages = queue.Queue()
        def read():
            for line in process.stdout:
                messages.put(json.loads(line))
            messages.put(None)
        threading.Thread(target=read, daemon=True).start()
        def send(value):
            process.stdin.write(json.dumps(value) + "\n")
            process.stdin.flush()
        try:
            send({"type": "control_request", "request_id": "todex-initialize", "request": {"subtype": "initialize", "hooks": None}})
            initialized = messages.get(timeout=20)
            assert initialized["response"]["subtype"] == "success", initialized
            send({"type": "user", "message": {"role": "user", "content": "Write the probe file."}})
            saw_permission = False
            while True:
                message = messages.get(timeout=30)
                if os.environ.get("TODEX_PROBE_VERBOSE"):
                    print(message, flush=True)
                assert message is not None, "CLI exited before result"
                if message.get("type") == "system" and message.get("subtype") == "init":
                    session_id = message["session_id"]
                    assert message.get("permissionMode") == mode, "CLI silently changed permission mode"
                if message.get("type") == "control_request":
                    assert message["request"]["subtype"] == "can_use_tool", message
                    assert not target.exists(), "Tool ran before approval"
                    saw_permission = True
                    response = {"behavior": "allow", "updatedInput": message["request"]["input"]} if allow else {"behavior": "deny", "message": "User rejected this tool request"}
                    send({"type": "control_response", "response": {"subtype": "success", "request_id": message["request_id"], "response": response}})
                elif message.get("type") == "result":
                    assert not message.get("is_error"), message
                    break
            assert saw_permission == expect_prompt, "Unexpected approval routing"
            assert target.exists() == allow, "Tool execution did not match approval"
            print(f"PASS real Claude CLI {mode} {'allow' if allow else 'deny'} round trip")
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            server.shutdown()
        return session_id


if __name__ == "__main__":
    binary = sys.argv[1] if len(sys.argv) > 1 else "claude"
    probe(binary, False)
    probe(binary, True)
    with tempfile.TemporaryDirectory(prefix="todex-claude-plan-resume-") as directory:
        session = probe(binary, False, "plan", expect_prompt=False, directory=directory)
        assert session
        probe(binary, True, "default", directory=directory, resume=session)
    probe(binary, True, "bypassPermissions", expect_prompt=False)
