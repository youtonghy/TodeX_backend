#!/usr/bin/python3
"""Deterministic, offline Pi RPC contract fixture. Never invokes a model."""
import json
import os
import sys
import time

args = sys.argv[1:]
session = args[args.index('--session') + 1] if '--session' in args else args[args.index('--session-id') + 1] if '--session-id' in args else 'discovery'
if session == 'missing':
    print('No session found matching missing', file=sys.stderr)
    sys.exit(1)
with open('pi-launches', 'a') as marker:
    marker.write(json.dumps(args) + '\n')
model = {'provider': 'fixture', 'id': 'text', 'input': ['text', 'image'], 'reasoning': True}
level = 'high'
streaming = False
scenario = ''
queue = []
steering = []
waiting_dialog = False

def out(value):
    print(json.dumps(value), flush=True)

def ack(cmd, data=None):
    value = {'type': 'response', 'id': cmd.get('id'), 'command': cmd['type'], 'success': True}
    if data is not None:
        value['data'] = data
    out(value)

def state():
    return {'sessionId': session, 'sessionFile': '/tmp/pi-fixture.jsonl', 'model': model, 'thinkingLevel': level, 'isStreaming': streaming, 'isCompacting': False, 'pendingMessageCount': len(queue) + len(steering)}

def finish(reason='stop', text='answer'):
    global streaming
    out({'type': 'message_start', 'message': {'role': 'assistant'}})
    out({'type': 'message_end', 'message': {'role': 'assistant', 'stopReason': reason, 'errorMessage': 'fixture model failure' if reason == 'error' else None, 'content': [{'type': 'text', 'text': text}], 'usage': {'input': 3, 'output': 2}}})
    streaming = False
    out({'type': 'agent_end'})
    out({'type': 'agent_settled'})

def update_queue():
    out({'type': 'queue_update', 'steering': steering[:], 'followUp': queue[:]})

for line in sys.stdin:
    cmd = json.loads(line)
    kind = cmd['type']
    with open('pi-commands', 'a') as marker:
        marker.write(json.dumps(cmd) + '\n')
    if kind == 'get_state':
        ack(cmd, state())
        if cmd.get('id') == 'accepted-state':
            if scenario == 'normal':
                finish()
            elif scenario == 'thoughts':
                for index, reason in enumerate(['toolUse', 'stop']):
                    out({'type': 'message_start', 'message': {'role': 'assistant'}})
                    out({'type': 'message_update', 'assistantMessageEvent': {'type': 'thinking_delta', 'contentIndex': 0, 'delta': 'thought' + str(index)}})
                    out({'type': 'message_end', 'message': {'role': 'assistant', 'stopReason': reason, 'content': [{'type': 'text', 'text': 'part'}]}})
                streaming = False
                out({'type': 'agent_settled'})
            elif scenario == 'dialog':
                out({'type': 'extension_ui_request', 'id': 'dialog-a', 'method': 'confirm', 'title': 'Expires', 'timeout': 20})
                out({'type': 'extension_ui_request', 'id': 'dialog-b', 'method': 'confirm', 'title': 'Concurrent', 'timeout': 30})
                # Native timeout should not block the host's receipt of these events.
                finish()
    elif kind == 'set_model':
        if cmd['modelId'] == 'reject':
            out({'type': 'response', 'id': cmd.get('id'), 'success': False, 'error': 'fixture rejected model'})
            continue
        model['provider'] = cmd['provider']
        model['id'] = cmd['modelId']
        ack(cmd, model)
    elif kind == 'set_thinking_level':
        level = 'high' if cmd['level'] == 'max' else cmd['level']
        ack(cmd)
    elif kind == 'prompt':
        scenario = cmd['message']
        streaming = scenario not in ['pure', 'preack', 'preack-dialog', 'extension-error']
        if scenario == 'extension-error':
            out({'type': 'extension_error', 'error': 'fixture extension failed'})
            ack(cmd)
        elif scenario == 'preack-dialog':
            out({'type': 'extension_ui_request', 'id': 'expires-before-ack', 'method': 'confirm', 'title': 'Expires before ack', 'timeout': 10})
            time.sleep(0.04)
            ack(cmd)
        elif scenario == 'preack':
            finish(text='before ack')
            ack(cmd)
        else:
            ack(cmd)
            if streaming:
                out({'type': 'agent_start'})
            if scenario == 'hold':
                out({'type': 'tool_execution_start', 'toolCallId': 'holding', 'toolName': 'fixture'})
            if scenario == 'extension-warning':
                out({'type': 'extension_error', 'error': 'nonfatal hook failed'})
                finish()
            if scenario == 'error':
                finish('error', '')
            elif scenario == 'retry':
                out({'type': 'message_end', 'message': {'role': 'assistant', 'stopReason': 'error', 'errorMessage': 'temporary'}})
                out({'type': 'auto_retry_start', 'attempt': 1})
                finish()
    elif kind == 'follow_up':
        if cmd['message'] == '/expanded':
            if queue:
                consumed = queue.pop(0)
                update_queue()
                out({'type': 'message_start', 'message': {'role': 'user', 'content': [{'type': 'text', 'text': consumed}]}})
            queue.append('expanded queued input')
        else:
            queue.append(cmd['message'])
        update_queue()
        ack(cmd)
    elif kind == 'clear_queue':
        data = {'steering': steering[:], 'followUp': queue[:]}
        queue.clear()
        steering.clear()
        update_queue()
        ack(cmd, data)
    elif kind == 'steer':
        if cmd['message'] == 'disconnect':
            sys.exit(0)
        if cmd['message'] == 'delayed':
            time.sleep(0.2)
        ack(cmd)
        if cmd['message'] == 'finish':
            while queue:
                text = queue.pop(0)
                update_queue()
                out({'type': 'message_start', 'message': {'role': 'user', 'content': [{'type': 'text', 'text': text}]}})
            finish()
    elif kind == 'abort':
        streaming = False
        ack(cmd)
    elif kind == 'clone':
        session = 'cloned-session'
        ack(cmd, {'cancelled': False})
    elif kind == 'compact':
        out({'type': 'compaction_start', 'reason': 'manual'})
        out({'type': 'compaction_end', 'result': {'summary': 'compact summary'}})
        ack(cmd, {'summary': 'compact summary'})
    elif kind == 'extension_ui_response':
        pass
    else:
        out({'type': 'response', 'id': cmd.get('id'), 'success': False, 'error': 'unexpected command ' + kind})
