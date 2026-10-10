"""Codex app-server wire fixture, never a model or existing thread."""
import json
import sys
import os
import time

mode=sys.argv[1]
thread_id='thread-fixture'
turns=[]
read_count=0
def emit(value):print(json.dumps(value),flush=True)
def response(command,result):emit({'id':command['id'],'result':result})
def thread():return dict(id=thread_id,path=None,status={'type':'idle'},turns=turns)
for line in sys.stdin.buffer:
    command=json.loads(line)
    method=command['method']
    if method=='initialize':response(command,{'userAgent':'fixture'})
    elif method in {'thread/start','thread/resume'}:
        if method=='thread/resume':thread_id=command['params']['threadId']
        response(command,{'thread':thread()})
    elif method=='turn/start':
        turn_id=f'turn-{len(turns)+1}'
        completed=dict(id=turn_id,status='completed',items=[{'type':'agentMessage','id':'final','phase':'final_answer','text':'Codex final result'}],error=None)
        if mode!='late-response':response(command,{'turn':dict(id=turn_id,status='inProgress',items=[])})
        emit(dict(method='item/completed',params=dict(threadId=thread_id,turnId=turn_id,item={'type':'agentMessage','id':'progress','phase':'commentary','text':'wrong intermediate result'})))
        time.sleep(.1)
        foreign=mode=='foreign-turn'
        emit(dict(method='turn/completed',params=dict(threadId=thread_id,turn=completed if not foreign else dict(completed,id='foreign-turn'))))
        turns.append(completed)
        if mode=='late-response':response(command,{'turn':dict(id=turn_id,status='inProgress',items=[])})
    elif method=='thread/read':
        read_count+=1
        if mode=='late-idle' and read_count==1:
            response(command,{'thread':dict(thread(),status={'type':'active','activeFlags':[]})})
            emit(dict(method='thread/status/changed',params={'threadId':thread_id,'status':{'type':'idle'}}))
        else:response(command,{'thread':thread()})
