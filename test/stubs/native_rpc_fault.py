"""Owned protocol-fault fixture; all work and control remain in test roots."""
import json
import os
from pathlib import Path
import sys
import time
import subprocess
import threading

provider, mode, directory = sys.argv[1:4]
tree = mode in {'tree', 'tree-exit'}
tree_exit = mode == 'tree-exit'
if tree: mode = 'active'
directory = Path(directory)
launches = directory / 'fault-launches'
first = not launches.exists()
with launches.open('a', encoding='utf-8') as file:
    file.write(str(os.getpid()) + '\n')
secret = 'do-not-persist-protocol-error-secret'
if mode == 'active-delayed':
    mode = 'active'
    delayed = True
else: delayed = False

def emit(value): print(json.dumps(value), flush=True)

def hold():
    if tree:
        child = Path(__file__).with_name('native_scope_descendant.py')
        subprocess.Popen([sys.executable, '-I', str(child), str(directory), 'child'],
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 8
        while not (directory / 'grandchild.pid').exists() and time.monotonic() < deadline: time.sleep(.025)
    (directory / 'fault-emitted').touch()
    if tree_exit: os._exit(0)
    while not (directory / 'release-fault').exists(): time.sleep(.025)
    os._exit(0)

def fault_when_released():
    while not (directory / 'release-error').exists(): time.sleep(.025)
    print('invalid-json-' + secret, flush=True)
    hold()

if first and mode == 'idle-late-fault':
    threading.Thread(target=fault_when_released, daemon=True).start()

state = dict(sessionId='fault-session', sessionFile=None, isStreaming=False,
             isCompacting=False, pendingMessageCount=0, messageCount=0)
thread = dict(id='fault-thread', path=None, status={'type': 'idle'}, turns=[])
if provider == 'claude':
    flag = '--resume' if '--resume' in sys.argv else '--session-id'
    session = sys.argv[sys.argv.index(flag) + 1]
    emit(dict(type='system', subtype='init', session_id=secret if first and mode == 'init' else session))
    if first and mode == 'init': hold()

for line in sys.stdin.buffer:
    command = json.loads(line)
    if delayed and command.get('type') in {'prompt', 'user'} or delayed and command.get('method') == 'turn/start':
        while not (directory / 'release-error').exists(): time.sleep(.025)
    if first and mode == 'malformed' and command.get('type') == 'prompt' or (
            first and mode == 'malformed' and command.get('type') == 'user') or (
            first and mode == 'malformed' and command.get('method') == 'turn/start'):
        print('invalid-json-' + secret, flush=True); hold()
    if provider == 'pi':
        kind = command['type']
        if kind == 'get_state':
            emit(dict(type='response', command=kind, id=command['id'],
                      success=not (first and mode == 'init'), data=state, error=secret))
            if first and mode == 'init': hold()
        elif kind == 'prompt':
            emit(dict(type='response', command='wrong-' + secret if first and mode == 'active' else kind,
                      id=command['id'], success=True, data={'disposition': 'started'}))
            if first and mode == 'active': hold()
            emit(dict(type='message_end', message={'role': 'user', 'content': command['message']}))
            emit(dict(type='message_end', message={'role': 'assistant', 'content': 'fault fixture result', 'stopReason': 'stop'}))
            emit(dict(type='agent_settled', aborted=False))
    elif provider == 'claude':
        if first and mode == 'active':
            emit(dict(type='system', subtype='init', session_id=secret)); hold()
        emit(command)
        emit(dict(type='result', subtype='success', is_error=False, session_id=session,
                  uuid='result-' + command['uuid'], user_message_uuid=command['uuid'],
                  parent_tool_use_id=None, result='fault fixture result'))
    else:
        kind = command['method']
        if kind == 'initialize':
            if first and mode == 'init':
                emit({'id': command['id'], 'error': {'code': -32000, 'message': secret}}); hold()
            emit({'id': command['id'], 'result': {'userAgent': 'fixture'}})
        elif kind in {'thread/start', 'thread/resume'}:
            emit({'id': command['id'], 'result': {'thread': thread}})
        elif kind == 'turn/start':
            if first and mode == 'active':
                emit({'id': command['id'], 'result': {'turn': {'error': secret}}}); hold()
            turn = dict(id='fault-turn', status='completed', items=[
                dict(type='agentMessage', id='final', phase='final_answer', text='fault fixture result')], error=None)
            emit({'id': command['id'], 'result': {'turn': dict(turn, status='inProgress', items=[])}})
            thread['turns'] = [turn]
            emit(dict(method='turn/completed', params=dict(threadId=thread['id'], turn=turn)))
        elif kind == 'thread/read': emit({'id': command['id'], 'result': {'thread': thread}})
