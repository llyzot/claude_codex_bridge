"""Owned interactive app-server fixture; no model, commands or permission changes."""
import json
import os
import sys
import threading

mode = sys.argv[1]
thread_id = 'interactive-thread'
turns = []
pending = None
lock = threading.Lock()


def emit(value):
    with lock:
        print(json.dumps(value), flush=True)


def later(delay, action):
    timer = threading.Timer(delay, action)
    timer.daemon = True
    timer.start()


def response(command, result):
    emit({'id': command['id'], 'result': result})


def thread():
    status = 'active' if turns and turns[-1]['status'] == 'inProgress' else 'idle'
    return dict(id=thread_id, path=None, status={'type': status}, turns=turns)


def finish(turn, status='completed'):
    turn['status'] = status
    turn['items'] = [{'type': 'agentMessage', 'id': 'final', 'phase': 'final_answer', 'text': 'Interactive task completed'}]
    emit({'method': 'turn/completed', 'params': {'threadId': thread_id, 'turn': turn}})


def resolved(request_id, foreign=False):
    emit({'method': 'serverRequest/resolved', 'params': {'threadId': 'foreign' if foreign else thread_id, 'requestId': request_id}})


def request(turn):
    kind = mode if mode in {'file', 'input', 'permissions', 'mcp', 'mcp-url', 'dynamic'} else 'command'
    methods = {'command': 'item/commandExecution/requestApproval', 'file': 'item/fileChange/requestApproval',
               'input': 'item/tool/requestUserInput', 'permissions': 'item/permissions/requestApproval',
               'mcp': 'mcpServer/elicitation/request', 'mcp-url': 'mcpServer/elicitation/request', 'dynamic': 'item/tool/call'}
    params = dict(threadId=thread_id, turnId=turn['id'], itemId='item', startedAtMs=1000)
    if kind == 'command':
        params.update(command='echo fixture', cwd='C:/fixture', availableDecisions=['accept', 'decline', 'cancel'])
    elif kind == 'input':
        params.update(isBlocking=True, questions=[dict(id='choice', header='Choose', question='Enter fixture answer', isSecret=True)])
    elif kind == 'permissions':
        params.update(cwd='C:/fixture', permissions={'network': {'enabled': False}, 'fileSystem': {'entries': [{'path': {'type': 'path', 'path': 'C:/fixture'}, 'access': 'write'}]}})
    elif kind in {'mcp', 'mcp-url'}:
        params.update(serverName='fixture', message='Fixture form')
        if kind == 'mcp':
            params.update(mode='form', requestedSchema={'type': 'object', 'properties': {'name': {'type': 'string', 'minLength': 1}}, 'required': ['name']})
        else:
            params.update(mode='url', url='https://example.invalid/no-navigation', elicitationId='fixture-flow', turnId=None)
    elif kind == 'dynamic':
        params.update(callId='call', tool='fixture_tool', arguments={'value': 1})
    if mode == 'foreign-thread':
        params['threadId'] = 'foreign'
    elif mode == 'foreign-turn':
        params['turnId'] = 'foreign'
    elif mode == 'malformed':
        params.pop('itemId')
    request_id = 'string-request' if mode == 'string-id' else 201
    return {'id': request_id, 'method': methods[kind], 'params': params}


for line in sys.stdin.buffer:
    command = json.loads(line)
    with open(os.environ['CCB_FIXTURE_TRACE'], 'a', encoding='utf-8') as file:
        file.write(json.dumps(command) + '\n')
    method = command.get('method')
    if method == 'initialize':
        response(command, {'userAgent': 'fixture'})
    elif method in {'thread/start', 'thread/resume'}:
        if method == 'thread/resume':
            thread_id = command['params']['threadId']
        if mode == 'slow-bind':
            later(.4, lambda cmd=command: response(cmd, {'thread': thread()}))
        else:
            response(command, {'thread': thread()})
    elif method == 'turn/start':
        turn = dict(id=f'turn-{len(turns) + 1}', status='inProgress', items=[], error=None)
        turns.append(turn)
        pending = request(turn)
        if mode == 'unknown':
            pending['method'] = 'unsupported/test'
        if mode == 'late-accept':
            emit(pending)
            later(.1, lambda cmd=command, current=turn: response(cmd, {'turn': current}))
        else:
            response(command, {'turn': turn})
            emit(pending)
        if mode == 'duplicate':
            emit(pending)
    elif method == 'thread/read':
        response(command, {'thread': thread()})
    elif method == 'turn/interrupt':
        assert command['params'] == {'threadId': thread_id, 'turnId': turns[-1]['id']}
        response(command, {})
        if pending:
            resolved(pending['id'])
        finish(turns[-1], 'interrupted')
    elif method is None and pending and command['id'] == pending['id']:
        if 'error' in command:
            continue
        if mode == 'resolved-only':
            later(.1, lambda request_id=pending['id']: resolved(request_id))
        else:
            resolved(pending['id'], foreign=True)
            later(.2, lambda request_id=pending['id']: resolved(request_id))
            later(.4, lambda current=turns[-1]: finish(current))
