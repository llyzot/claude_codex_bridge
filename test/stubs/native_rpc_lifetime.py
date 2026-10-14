"""Owned protocol fixture for exit/EOF races; never invokes a model."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

provider, mode, launches = sys.argv[1:4]
launches = Path(launches)
first = not launches.exists()
with launches.open('a', encoding='utf-8') as file:
    file.write(str(os.getpid()) + '\n')

state = dict(sessionId='lifetime-session', sessionFile=None, isStreaming=False,
             isCompacting=False, pendingMessageCount=0, messageCount=0)
thread = dict(id='lifetime-thread', path=None, status={'type': 'idle'}, turns=[])
active = False
job_id = None
if provider == 'claude':
    flag = '--resume' if '--resume' in sys.argv else '--session-id'
    session = sys.argv[sys.argv.index(flag) + 1]
    print(json.dumps(dict(type='system', subtype='init', session_id=session)), flush=True)
    if first and mode == 'idle-exit':
        time.sleep(.15); os._exit(0)

def emit(value):
    print(json.dumps(value), flush=True)

def finish():
    if first and mode in {'final-exit', 'buffered-final-exit'}:
        if mode == 'buffered-final-exit':
            for _ in range(300):
                emit({'type': 'irrelevant', 'method': 'fixture/noise'})
        return True
    return False

def exit_with_pipe_holder():
    # The owned parent is terminal while a known descendant holds stdout open.
    release = str(launches.parent / 'release-holder')
    subprocess.Popen([sys.executable, '-I', '-c',
                     'import sys,time; from pathlib import Path; '
                     'p=Path(sys.argv[1]);\nwhile not p.exists(): time.sleep(.025)', release],
                     stdin=subprocess.DEVNULL, stdout=sys.stdout,
                     stderr=subprocess.DEVNULL)
    os._exit(0)

for line in sys.stdin.buffer:
    command = json.loads(line)
    if first and mode == 'closing-exit' and (command.get('type') in {'prompt','user'} or command.get('method') == 'turn/start'):
        os.close(sys.stdout.fileno())
        time.sleep(.15)
        os._exit(0)
    if provider == 'pi':
        kind = command['type']
        if kind == 'get_state':
            exiting = finish() if active else False
            emit(dict(type='response', command=kind, id=command['id'], success=True, data=state))
            if exiting: os._exit(0)
            if first and mode == 'idle-exit' and not active:
                time.sleep(.15); os._exit(0)
        elif kind == 'prompt':
            active = True
            job_id = command['message'].splitlines()[0].split(': ', 1)[1]
            emit(dict(type='response', command=kind, id=command['id'], success=True, data={'disposition': 'started'}))
            if first and mode == 'active-exit': os._exit(0)
            if first and mode == 'held-stdout': exit_with_pipe_holder()
            if first and mode in {'cancel-exit', 'cancel-ack-only'}: continue
            emit(dict(type='message_end', message={'role': 'user', 'content': command['message']}))
            emit(dict(type='message_end', message={'role': 'assistant', 'content': 'lifetime result', 'stopReason': 'stop'}))
            emit(dict(type='agent_settled', aborted=False))
        elif kind == 'abort':
            root = Path(os.environ['CCB_NATIVE_TOKEN_FILE']).parent.parent
            rows = [json.loads(line) for line in (root.parent / 'agents/worker/jobs.jsonl').read_text(encoding='utf-8').splitlines()]
            job = next(row for row in reversed(rows) if row['job_id'] == job_id)
            events = [json.loads(line) for line in (root.parent / 'agents/worker/events.jsonl').read_text(encoding='utf-8').splitlines()]
            proof = dict(intent=job['provider_options']['native_cancel_intent'],
                         event_count=sum(row['type'] == 'job_cancel_requested' and row['job_id'] == job_id for row in events))
            (launches.parent / 'abort-proof.json').write_text(json.dumps(proof), encoding='utf-8')
            with (launches.parent / 'aborts').open('a', encoding='utf-8') as file:
                file.write(command['id'] + '\n')
            emit(dict(type='response', command=kind, id=command['id'], success=True))
            if mode == 'cancel-exit':
                time.sleep(.35); os._exit(0)
    elif provider == 'claude':
        emit(command)
        emit(dict(type='result', subtype='success', is_error=False,
                  session_id=session, uuid='result-' + command['uuid'],
                  user_message_uuid=command['uuid'], parent_tool_use_id=None,
                  result='lifetime result'))
    else:
        kind = command['method']
        if kind == 'initialize':
            emit({'id': command['id'], 'result': {'userAgent': 'fixture'}})
        elif kind == 'config/read':
            emit({'id': command['id'], 'result': {'config': {'developer_instructions': None}}})
        elif kind in {'thread/start', 'thread/resume'}:
            emit({'id': command['id'], 'result': {'thread': thread}})
            if first and mode == 'idle-exit':
                time.sleep(.15); os._exit(0)
        elif kind == 'turn/start':
            active = True
            turn = dict(id='lifetime-turn', status='completed', items=[
                dict(type='agentMessage', id='final', phase='final_answer', text='lifetime result')], error=None)
            emit({'id': command['id'], 'result': {'turn': dict(turn, status='inProgress', items=[])}})
            if first and mode == 'active-exit': os._exit(0)
            if first and mode == 'held-stdout': exit_with_pipe_holder()
            thread['turns'] = [turn]
            emit(dict(method='turn/completed', params=dict(threadId=thread['id'], turn=turn)))
        elif kind == 'thread/read':
            exiting = finish()
            emit({'id': command['id'], 'result': {'thread': thread}})
            if exiting: os._exit(0)
