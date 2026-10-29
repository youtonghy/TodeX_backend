#!/usr/bin/python3
"""Deterministic, offline Pi RPC contract fixture. Never invokes a model."""
import json
import os
import sys
import time
import threading

args = sys.argv[1:]
session = args[args.index('--session') + 1] if '--session' in args else args[args.index('--session-id') + 1] if '--session-id' in args else 'discovery'
if session == 'missing':
    print('No session found matching missing', file=sys.stderr)
    sys.exit(1)
if os.path.exists('pi-ignore-term'):
    # Lets tests observe a runtime that is still stopping.
    import signal
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
with open('pi-launches', 'a') as marker:
    marker.write(json.dumps(args) + '\n')
model = {'provider': 'fixture', 'id': 'text', 'input': ['text', 'image'], 'reasoning': True}
level = 'high'
streaming = False
scenario = ''
queue = []
steering = []
waiting_dialog = False
output_lock = threading.Lock()

def out(value):
    with output_lock:
        print(json.dumps(value), flush=True)

def custom(text, display=True):
    message = {'role': 'custom', 'customType': 'fixture-report', 'content': [{'type': 'text', 'text': text}], 'display': display, 'details': {'fixture': True}, 'timestamp': 123456}
    out({'type': 'message_start', 'message': message})
    out({'type': 'message_end', 'message': message})

def ui(method, **fields):
    out({'type': 'extension_ui_request', 'id': fields.pop('id', 'ui-' + method), 'method': method, **fields})

def idle_events():
    time.sleep(0.08)
    ui('notify', message='idle notification', notifyType='info')
    custom('idle custom message')
    ui('confirm', id='idle-confirm', title='Background confirmation')

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

def stream_with_tool():
    # Real Pi stream shape: every assistantMessageEvent repeats `partial`, the
    # whole message so far with a zero usage/cost object, and each tool
    # execution update repeats args plus the accumulated partialResult.
    zero = {'input': 0, 'output': 0, 'cacheRead': 0, 'cacheWrite': 0, 'totalTokens': 0,
            'cost': {'input': 0, 'output': 0, 'cacheRead': 0, 'cacheWrite': 0, 'total': 0}}
    content = []
    def partial():
        return {'role': 'assistant', 'content': content, 'api': 'fixture', 'provider': 'fixture',
                'model': 'text', 'usage': zero, 'stopReason': 'stop', 'timestamp': 123456}
    def update(event):
        event['partial'] = partial()
        out({'type': 'message_update', 'assistantMessageEvent': event})
    out({'type': 'message_start', 'message': {'role': 'assistant'}})
    content.append({'type': 'thinking', 'thinking': ''})
    update({'type': 'thinking_start', 'contentIndex': 0})
    for index in range(50):
        token = 't%02d' % index
        content[0]['thinking'] += token
        update({'type': 'thinking_delta', 'contentIndex': 0, 'delta': token})
    update({'type': 'thinking_end', 'contentIndex': 0, 'content': content[0]['thinking']})
    content.append({'type': 'text', 'text': ''})
    update({'type': 'text_start', 'contentIndex': 1})
    for token in ['Run', 'ning', ' it']:
        content[1]['text'] += token
        update({'type': 'text_delta', 'contentIndex': 1, 'delta': token})
    update({'type': 'text_end', 'contentIndex': 1, 'content': content[1]['text']})
    call = {'type': 'toolCall', 'id': 'call-1', 'name': 'bash', 'arguments': {'command': 'make'}}
    content.append(call)
    update({'type': 'toolcall_start', 'contentIndex': 2, 'id': 'call-1', 'toolName': 'bash'})
    update({'type': 'toolcall_end', 'contentIndex': 2, 'toolCall': call})
    out({'type': 'message_end', 'message': {'role': 'assistant', 'stopReason': 'toolUse', 'content': content, 'usage': {'input': 5, 'output': 7}}})
    args = {'command': 'make'}
    out({'type': 'tool_execution_start', 'toolCallId': 'call-1', 'toolName': 'bash', 'args': args})
    output = ''
    for index in range(25):
        if index == 20:
            time.sleep(0.6)
        output += 'line %d\n' % index
        out({'type': 'tool_execution_update', 'toolCallId': 'call-1', 'toolName': 'bash', 'args': args,
             'partialResult': {'content': [{'type': 'text', 'text': output}]}})
    out({'type': 'tool_execution_end', 'toolCallId': 'call-1', 'toolName': 'bash',
         'result': {'content': [{'type': 'text', 'text': output}]}, 'isError': False})
    finish()

def subagent_runs():
    # @tintinweb/pi-subagents: a foreground Agent call reports progress in
    # partialResult.details; a background one returns at once and settles
    # between turns through a session record and a completion notification.
    def details(status, **extra):
        return {'displayName': 'Explore', 'description': 'Find auth', 'subagentType': 'Explore',
                'toolUses': 2, 'tokens': '1.2k token', 'durationMs': 900, 'status': status, **extra}
    args = {'prompt': 'Find the auth code', 'description': 'Find auth', 'subagent_type': 'Explore', 'run_in_background': False}
    out({'type': 'tool_execution_start', 'toolCallId': 'agent-call', 'toolName': 'Agent', 'args': args})
    out({'type': 'tool_execution_update', 'toolCallId': 'agent-call', 'toolName': 'Agent', 'args': args,
         'partialResult': {'content': [{'type': 'text', 'text': '2 tool uses...'}], 'details': details('running', activity='reading')}})
    out({'type': 'tool_execution_end', 'toolCallId': 'agent-call', 'toolName': 'Agent', 'isError': False,
         'result': {'content': [{'type': 'text', 'text': 'Agent completed.\n\nFound it.'}], 'details': details('completed', agentId='fg-agent')}})
    background = {**args, 'description': 'Scan tests', 'run_in_background': True}
    out({'type': 'tool_execution_start', 'toolCallId': 'bg-call', 'toolName': 'Agent', 'args': background})
    out({'type': 'tool_execution_end', 'toolCallId': 'bg-call', 'toolName': 'Agent', 'isError': False,
         'result': {'content': [{'type': 'text', 'text': 'Agent started in background.\nAgent ID: bg-agent\nOutput file: /tmp/bg-agent.output'}],
                    'details': details('background', agentId='bg-agent', description='Scan tests')}})
    finish()
    def settle():
        time.sleep(0.08)
        out({'type': 'entry_appended', 'entry': {'type': 'custom', 'id': 'e1', 'customType': 'subagents:record',
             'data': {'id': 'bg-agent', 'type': 'Explore', 'description': 'Scan tests', 'status': 'completed', 'result': 'All tests found.'}}})
        message = {'role': 'custom', 'customType': 'subagent-notification', 'display': True,
                   'content': '<task-notification>...</task-notification>',
                   'details': {'id': 'bg-agent', 'description': 'Scan tests', 'status': 'completed', 'totalTokens': 4321,
                               'outputFile': '/tmp/bg-agent.output', 'resultPreview': 'All tests'}}
        out({'type': 'message_start', 'message': message})
        out({'type': 'message_end', 'message': message})
    threading.Thread(target=settle, daemon=True).start()

def spawn_runs():
    # pi-agenticoding: `spawn` runs a child session in the foreground with
    # one running update; the second call is still running when the turn is
    # stopped and never returns.
    def details(outcome, **extra):
        return {'model': 'text', 'thinking': 'high', 'truncated': False, 'outcome': outcome,
                'route': {'status': 'inherited'}, **extra}
    for call in ['spawn-done', 'spawn-open']:
        args = {'prompt': 'Child task ' + call + '\nwith details'}
        out({'type': 'tool_execution_start', 'toolCallId': call, 'toolName': 'spawn', 'args': args})
        out({'type': 'tool_execution_update', 'toolCallId': call, 'toolName': 'spawn', 'args': args,
             'partialResult': {'content': [], 'details': details('running')}})
        if call == 'spawn-done':
            stats = {'inputTokens': 10, 'outputTokens': 5, 'cacheReadTokens': 0, 'cacheWriteTokens': 0,
                     'totalTokens': 15, 'cost': 0, 'turns': 1}
            out({'type': 'tool_execution_end', 'toolCallId': call, 'toolName': 'spawn', 'isError': False,
                 'result': {'content': [{'type': 'text', 'text': 'child answer'}], 'details': details('success', stats=stats)}})

def background_subagent(settles):
    # pi-subagents' default: the Agent call returns at once, the run goes on in
    # the Pi process writing nothing but its status line, then settles through
    # a session record and a notification that makes Pi start a run of its own.
    out({'type': 'agent_start'})
    ui('setStatus', id='status-bg', statusKey='subagents', statusText='1 running agent')
    args = {'prompt': 'Scan the tests', 'description': 'Scan tests', 'subagent_type': 'general-purpose'}
    out({'type': 'tool_execution_start', 'toolCallId': 'bg-call', 'toolName': 'Agent', 'args': args})
    out({'type': 'tool_execution_end', 'toolCallId': 'bg-call', 'toolName': 'Agent', 'isError': False,
         'result': {'content': [{'type': 'text', 'text': 'Agent started in background.\nAgent ID: bg-1\nOutput file: /tmp/bg-1.output'}],
                    'details': {'description': 'Scan tests', 'subagentType': 'general-purpose', 'status': 'background', 'agentId': 'bg-1'}}})
    finish(text='launched')
    if not settles:
        return
    def settle():
        global streaming
        time.sleep(float(open('pi-bg-delay').read()) if os.path.exists('pi-bg-delay') else 0.8)
        ui('setStatus', id='status-bg-done', statusKey='subagents')
        out({'type': 'entry_appended', 'entry': {'type': 'custom', 'id': 'r1', 'customType': 'subagents:record',
             'data': {'id': 'bg-1', 'type': 'general-purpose', 'description': 'Scan tests', 'status': 'completed', 'result': 'Tests scanned.'}}})
        streaming = True
        out({'type': 'agent_start'})
        message = {'role': 'custom', 'customType': 'subagent-notification', 'display': True,
                   'content': '<task-notification>...</task-notification>',
                   'details': {'id': 'bg-1', 'description': 'Scan tests', 'status': 'completed', 'resultPreview': 'Tests scanned.'}}
        out({'type': 'message_start', 'message': message})
        out({'type': 'message_end', 'message': message})
        if os.path.exists('pi-own-run-wait'):
            # Holds the run until a prompt queued behind it arrives.
            for _ in range(300):
                if queue:
                    break
                time.sleep(0.01)
        out({'type': 'message_start', 'message': {'role': 'assistant'}})
        out({'type': 'message_end', 'message': {'role': 'assistant', 'stopReason': 'stop', 'content': [{'type': 'text', 'text': 'report'}], 'usage': {'input': 1, 'output': 1}}})
        while queue:
            text = queue.pop(0)
            out({'type': 'message_start', 'message': {'role': 'user', 'content': [{'type': 'text', 'text': text}]}})
            out({'type': 'message_start', 'message': {'role': 'assistant'}})
            out({'type': 'message_end', 'message': {'role': 'assistant', 'stopReason': 'stop', 'content': [{'type': 'text', 'text': 'after ' + text}], 'usage': {'input': 1, 'output': 1}}})
        streaming = False
        out({'type': 'agent_end'})
        out({'type': 'agent_settled'})
    threading.Thread(target=settle, daemon=True).start()

def update_queue():
    out({'type': 'queue_update', 'steering': steering[:], 'followUp': queue[:]})

# User messages on the branch, as (entryId, text), for get_fork_messages.
user_entries = []

for line in sys.stdin:
    cmd = json.loads(line)
    kind = cmd['type']
    with open('pi-commands', 'a') as marker:
        marker.write(json.dumps(cmd) + '\n')
    if kind == 'get_commands':
        if scenario == 'stall-catalog':
            ui('confirm', id='catalog-confirm', title='Catalog is stalled')
            continue
        ack(cmd, {'commands': [{'name': 'fixture-report', 'description': 'fixture command', 'source': 'extension', 'sourceInfo': {'path': os.path.join(os.getcwd(), 'pi.py'), 'origin': 'top-level', 'scope': 'user', 'source': 'npm:fixture-plugin'}}]})
    elif kind == 'get_state':
        if cmd.get('id') == 'state' and scenario == 'buffer-idle':
            out({'type': 'message_start', 'message': {'role': 'assistant'}})
            out({'type': 'message_end', 'message': {'role': 'assistant', 'stopReason': 'stop', 'content': [{'type': 'text', 'text': 'background before next prompt'}], 'usage': {'input': 1, 'output': 2}}})
        ack(cmd, state())
        if cmd.get('id') == 'accepted-state':
            if scenario == 'normal':
                finish()
            elif scenario == 'thoughts':
                for index, reason in enumerate(['toolUse', 'stop']):
                    out({'type': 'message_start', 'message': {'role': 'assistant'}})
                    out({'type': 'message_update', 'assistantMessageEvent': {'type': 'thinking_delta', 'contentIndex': 0, 'delta': 'thought' + str(index)}})
                    out({'type': 'message_update', 'assistantMessageEvent': {'type': 'thinking_delta', 'contentIndex': 0, 'delta': ' more'}})
                    out({'type': 'message_update', 'assistantMessageEvent': {'type': 'text_delta', 'contentIndex': 1, 'delta': 'part'}})
                    out({'type': 'message_end', 'message': {'role': 'assistant', 'stopReason': reason, 'content': [{'type': 'text', 'text': 'part'}]}})
                streaming = False
                out({'type': 'agent_settled'})
            elif scenario == 'stream':
                stream_with_tool()
            elif scenario == 'subagents':
                subagent_runs()
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
    elif kind == 'prompt' and cmd.get('streamingBehavior') == 'followUp':
        queue.append(cmd['message'])
        ack(cmd, {'disposition': 'queued'})
    elif kind == 'prompt':
        scenario = cmd['message']
        user_entries.append(('entry-%d' % (len(user_entries) + 1), scenario))
        streaming = scenario not in ['pure', 'preack', 'preack-dialog', 'extension-error', 'ui', 'schedule-idle', 'stall-catalog', 'buffer-idle', 'rejected']
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
        elif scenario == 'ui':
            ui('notify', message='plugin notification', notifyType='warning')
            ui('setStatus', id='status-one', statusKey='fixture', statusText='working')
            ui('setStatus', id='status-two', statusKey='fixture', statusText='working')
            ui('setWidget', widgetKey='fixture', widgetLines=['row one', 'row two'], widgetPlacement='belowEditor')
            ui('setTitle', title='Extension title')
            ui('set_editor_text', text='suggested prompt')
            custom('visible extension report')
            custom('hidden extension context', False)
            for index in range(50):
                ui('setWidget', widgetKey='fixture', widgetLines=['progress ' + str(index)])
            ui('setStatus', statusKey='fixture')
            ui('setWidget', widgetKey='fixture')
            ack(cmd)
        elif scenario in ['stall-catalog', 'buffer-idle']:
            ack(cmd)
        elif scenario == 'schedule-idle':
            ack(cmd)
            threading.Thread(target=idle_events, daemon=True).start()
        elif scenario == 'rejected':
            out({'type': 'response', 'id': cmd.get('id'), 'command': kind, 'success': False, 'error': 'fixture command rejected'})
        elif scenario in ['bg-subagent', 'bg-forever']:
            ack(cmd)
            background_subagent(scenario == 'bg-subagent')
        elif scenario == 'malformed':
            ack(cmd)
            print('not-json', flush=True)
        else:
            ack(cmd)
            if streaming:
                out({'type': 'agent_start'})
            if scenario == 'hold':
                out({'type': 'tool_execution_start', 'toolCallId': 'holding', 'toolName': 'fixture'})
            if scenario == 'spawn-hold':
                spawn_runs()
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
    elif kind == 'get_fork_messages':
        ack(cmd, {'messages': [{'entryId': entry, 'text': text} for entry, text in user_entries]})
    elif kind == 'fork':
        session = 'forked-before-' + cmd['entryId']
        ack(cmd, {'text': dict(user_entries).get(cmd['entryId'], ''), 'cancelled': False})
    elif kind == 'compact':
        out({'type': 'compaction_start', 'reason': 'manual'})
        out({'type': 'compaction_end', 'result': {'summary': 'compact summary'}})
        ack(cmd, {'summary': 'compact summary'})
    elif kind == 'extension_ui_response':
        if cmd.get('id') == 'idle-confirm':
            ui('notify', message='idle answer: ' + str(cmd.get('confirmed', 'cancelled')), notifyType='info')
    else:
        out({'type': 'response', 'id': cmd.get('id'), 'success': False, 'error': 'unexpected command ' + kind})
