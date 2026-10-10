"""Codex app-server wire fixture, never a model or existing thread."""
import json
import sys
import os
import time
import threading

mode=sys.argv[1]
thread_id='thread-fixture'
turns=[]
read_count=0
lock=threading.Lock()
def emit(value):
    with lock:print(json.dumps(value),flush=True)
def trace(command):
    if os.environ.get('CCB_FIXTURE_TRACE'):
        with open(os.environ['CCB_FIXTURE_TRACE'],'a',encoding='utf-8') as file:
            file.write(json.dumps(command)+'\n')
def later(delay,action):
    timer=threading.Timer(delay,action);timer.daemon=True;timer.start()
def finish_cancel(turn):
    turn['status']='completed' if mode=='cancel-natural-finish' else 'failed' if mode=='cancel-failed-finish' else 'interrupted'
    if mode=='cancel-failed-finish':turn['error']={'message':'fixture provider error'}
    emit(dict(method='turn/completed',params=dict(threadId=thread_id,turn=turn)))
def response(command,result):emit({'id':command['id'],'result':result})
def thread():return dict(id=thread_id,path=None,status={'type':'idle'},turns=turns)
for line in sys.stdin.buffer:
    command=json.loads(line)
    trace(command)
    method=command['method']
    if method=='initialize':response(command,{'userAgent':'fixture'})
    elif method in {'thread/start','thread/resume'}:
        if method=='thread/resume':thread_id=command['params']['threadId']
        response(command,{'thread':thread()})
    elif method=='turn/start':
        turn_id=f'turn-{len(turns)+1}'
        completed=dict(id=turn_id,status='completed',items=[{'type':'agentMessage','id':'final','phase':'final_answer','text':'Codex final result'}],error=None)
        if mode.startswith('cancel-') and not turns:
            completed['status']='inProgress';turns.append(completed)
            if mode=='cancel-late-accept':later(.6,lambda cmd=command,tid=turn_id:response(cmd,{'turn':dict(id=tid,status='inProgress',items=[])}))
            else:response(command,{'turn':dict(id=turn_id,status='inProgress',items=[])})
            continue
        if mode!='late-response':response(command,{'turn':dict(id=turn_id,status='inProgress',items=[])})
        emit(dict(method='item/completed',params=dict(threadId=thread_id,turnId=turn_id,item={'type':'agentMessage','id':'progress','phase':'commentary','text':'wrong intermediate result'})))
        time.sleep(.1)
        foreign=mode=='foreign-turn'
        emit(dict(method='turn/completed',params=dict(threadId=thread_id,turn=completed if not foreign else dict(completed,id='foreign-turn'))))
        turns.append(completed)
        if mode=='late-response':response(command,{'turn':dict(id=turn_id,status='inProgress',items=[])})
    elif method=='turn/interrupt':
        assert command['params']=={'threadId':thread_id,'turnId':turns[-1]['id']}
        if mode=='cancel-denied':
            emit({'id':command['id'],'error':{'code':-32000,'message':'interrupt rejected by fixture'}})
            continue
        response(command,{})
        if mode=='cancel-foreign':
            emit(dict(method='turn/completed',params=dict(threadId=thread_id,turn=dict(turns[-1],id='foreign-turn',status='interrupted'))))
        elif mode!='cancel-ack-only':later(.35,lambda turn=turns[-1]:finish_cancel(turn))
    elif method=='thread/read':
        read_count+=1
        if mode=='late-idle' and read_count==1:
            response(command,{'thread':dict(thread(),status={'type':'active','activeFlags':[]})})
            emit(dict(method='thread/status/changed',params={'threadId':thread_id,'status':{'type':'idle'}}))
        else:response(command,{'thread':thread()})
