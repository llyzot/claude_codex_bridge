"""Disposable Pi/Codex protocols for context-control evidence and failure tests."""
import json
import os
import sys
import threading
import time
import uuid
from pathlib import Path

provider, mode, directory = sys.argv[1:4]
root = Path(directory)
root.mkdir(exist_ok=True)
lock = threading.Lock()
abort = threading.Event()

def emit(value):
    with lock:
        print(json.dumps(value), flush=True)

def trace(value):
    with (root/'wire.jsonl').open('a', encoding='utf-8') as output:
        output.write(json.dumps(value)+'\n')

def hold():
    if mode in {'delay', 'cancel', 'foreign', 'ack-only', 'old-event'}:
        deadline = time.monotonic()+15
        while not (root/'release').exists() and not abort.is_set() and time.monotonic()<deadline:
            time.sleep(.01)

def sid(): return str(uuid.uuid4())

state = dict(sessionId=sid(), sessionFile=None, isStreaming=False, isCompacting=False,
             pendingMessageCount=0, messageCount=0)
if '--session' in sys.argv:
    state = json.loads(Path(sys.argv[sys.argv.index('--session')+1]).read_text())
else:
    state['sessionFile'] = str(root/(state['sessionId']+'.json'))

def save_pi():
    if mode == 'empty-file' and state['messageCount'] == 0: return
    Path(state['sessionFile']).write_text(json.dumps(state))

save_pi()
history = {}
thread_id = None

def thread():
    return dict(id=thread_id, path=None, status={'type':'idle'}, turns=history[thread_id])

def pi_response(command, data=None, success=True):
    emit(dict(type='response', id=command['id'], command=command['type'], success=success, data=data))

def pi_control(command):
    abort.clear()
    if mode == 'crash': os._exit(0)
    if command['type'] == 'compact': state['isCompacting'] = True
    if mode == 'foreign':
        emit(dict(type='response', id='foreign-control', command=command['type'], success=True,
                  data={'cancelled':False, 'summary':'private', 'firstKeptEntryId':'kept', 'tokensBefore':200}))
    hold()
    if mode == 'ack-only': return
    if abort.is_set() or mode == 'reject':
        state['isCompacting'] = False
        pi_response(command, success=False)
        return
    if command['type'] == 'new_session':
        if mode != 'veto':
            state['sessionId'] = sid()
            state['sessionFile'] = str(root/(state['sessionId']+'.json'))
            state['messageCount'] = 0
        save_pi()
        pi_response(command, {'cancelled':mode=='veto'})
    else:
        state['isCompacting'] = False
        save_pi()
        pi_response(command, {'summary':'PRIVATE_SUMMARY_NOT_FOR_CONTROLLER', 'firstKeptEntryId':'kept', 'tokensBefore':200})
    if mode == 'wrong-state': state['sessionId'] = 'unrelated'

def response(command, result): emit(dict(id=command['id'], result=result))

def codex_compact(command):
    abort.clear()
    if mode == 'crash': os._exit(0)
    if mode == 'reject':
        emit(dict(id=command['id'], error={'code':-1, 'message':'SECRET_REJECT'}))
        return
    if mode != 'ack-late': response(command, {})
    if mode == 'old-event':
        old = history[thread_id][0]
        emit(dict(method='turn/started', params=dict(threadId=thread_id, turn=old)))
        emit(dict(method='turn/completed', params=dict(threadId=thread_id, turn=old)))
    if mode in {'ack-only', 'old-event'}:
        hold()
        return
    turn = dict(id=sid(), status='inProgress', items=[], error=None)
    history[thread_id].append(turn)
    item = dict(id=sid(), type='contextCompaction')
    emit(dict(method='turn/started', params=dict(threadId=thread_id, turn=turn)))
    emit(dict(method='item/started', params=dict(threadId=thread_id, turnId=turn['id'], item=item)))
    hold()
    if abort.is_set(): turn['status'] = 'interrupted'
    else:
        turn['status'] = 'completed'
        if mode != 'missing-item':
            turn['items'] = [item]
            emit(dict(method='item/completed', params=dict(threadId=thread_id, turnId=turn['id'], item=item)))
    emit(dict(method='turn/completed', params=dict(threadId=thread_id, turn=turn)))
    if mode == 'ack-late':
        time.sleep(.05)
        response(command, {})

for line in sys.stdin:
    command = json.loads(line)
    trace(command)
    if provider == 'pi':
        kind = command['type']
        if kind == 'get_state':
            if mode != 'ack-only' or not command['id'].startswith('context-state-'):
                pi_response(command, state)
        elif kind in {'compact', 'new_session'}:
            threading.Thread(target=pi_control, args=(command,), daemon=True).start()
        elif kind == 'abort':
            abort.set(); pi_response(command)
        elif kind == 'prompt':
            pi_response(command, {'disposition':'started'})
            emit(dict(type='message_end', message={'role':'user','content':command['message']}))
            if mode=='hold-prompt':
                state['isStreaming']=True
                continue
            emit(dict(type='message_end', message={'role':'assistant','content':'normal reply','stopReason':'stop'}))
            state['messageCount'] += 2; save_pi()
            emit(dict(type='agent_settled', aborted=False))
    else:
        method = command['method']
        if method == 'initialize': response(command, {'userAgent':'context fixture'})
        elif method in {'thread/start', 'thread/resume'}:
            thread_id = command['params'].get('threadId') or sid()
            history.setdefault(thread_id, [])
            response(command, {'thread':thread()})
        elif method == 'thread/read': response(command, {'thread':thread()})
        elif method == 'thread/compact/start':
            threading.Thread(target=codex_compact, args=(command,), daemon=True).start()
        elif method == 'turn/interrupt':
            abort.set(); response(command, {})
        elif method == 'turn/start':
            turn = dict(id=sid(), status='completed', items=[dict(id=sid(), type='agentMessage',
                        text='normal reply', phase='final_answer')], error=None)
            history[thread_id].append(turn)
            if mode=='hold-prompt':
                turn['status']='inProgress';turn['items']=[]
                response(command, {'turn':turn})
                continue
            response(command, {'turn':turn})
            emit(dict(method='turn/completed', params=dict(threadId=thread_id, turn=turn)))
