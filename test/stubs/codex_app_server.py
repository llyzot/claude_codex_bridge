"""Codex app-server wire fixture, never a model or existing thread."""
import json
import sys
import os
import time
import threading
from pathlib import Path

mode=sys.argv[1]
thread_id='thread-fixture'
turns=[]
read_count=0
lock=threading.Lock()
skill_roots=[]
loaded_skills=[]

def skill_behavior():
    path=os.environ.get('CCB_SKILL_BEHAVIOR')
    return Path(path).read_text().strip() if path and Path(path).exists() else ''
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
    elif method=='skills/extraRoots/set':
        behavior=skill_behavior()
        if behavior=='bad-envelope':
            emit({'id':command['id'],'result':None})
            continue
        if behavior=='reject-set':
            emit({'id':command['id'],'error':{'code':-1,'message':'SECRET_SKILL_ERROR'}})
            continue
        skill_roots=command['params']['extraRoots']
        loaded_skills=[]
        if behavior=='delay-set':later(.6,lambda cmd=command:response(cmd,{}))
        else:response(command,{})
    elif method=='skills/list':
        assert command['params']['forceReload'] is True
        behavior=skill_behavior()
        loaded_skills=[dict(name=Path(root).name,path=str(Path(root)/'SKILL.md'),enabled=behavior!='disabled') for root in skill_roots]
        if behavior=='missing':loaded_skills=[]
        data={'data':[dict(cwd='C:/not-the-workspace' if behavior=='wrong-cwd' else command['params']['cwds'][0],skills=loaded_skills,errors=[])]}
        if behavior=='reject-list':emit({'id':command['id'],'error':{'code':-1,'message':'SECRET_SKILL_ERROR'}})
        elif behavior=='malformed':response(command,{'data':None})
        elif behavior in {'delay-list','foreign'}:
            if behavior=='foreign':
                emit({'id':'foreign-skill-list','result':data})
                emit(dict(method='turn/completed',params=dict(threadId=thread_id,turn=dict(id='foreign',status='completed',items=[]))))
            later(.6,lambda cmd=command,data=data:response(cmd,data))
        else:response(command,data)
    elif method=='turn/start':
        assert len(loaded_skills)==len(skill_roots)
        turn_id=f'turn-{len(turns)+1}'
        completed=dict(id=turn_id,status='completed',items=[{'type':'agentMessage','id':'final','phase':'final_answer','text':'Codex final result'}],error=None)
        if mode.startswith('followup-'):
            completed['status']='inProgress';turns.append(completed)
            response(command,{'turn':dict(id=turn_id,status='inProgress',items=[])})
            continue
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
    elif method=='turn/steer':
        assert command['params']['threadId']==thread_id
        assert command['params']['expectedTurnId']==turns[-1]['id']
        assert command['params']['clientUserMessageId']
        if mode=='followup-denied':
            emit({'id':command['id'],'error':{'code':-1,'message':'SECRET_FOLLOWUP_ERROR'}})
        elif mode=='followup-mismatch':response(command,{'turnId':'foreign-turn'})
        elif mode=='followup-no-ack':pass
        else:
            emit({'id':'followup-foreign','result':{'turnId':turns[-1]['id']}})
            if mode=='followup-delay':later(.4,lambda cmd=command,tid=turns[-1]['id']:response(cmd,{'turnId':tid}))
            else:response(command,{'turnId':turns[-1]['id']})
            if mode=='followup-finish':
                turns[-1]['status']='completed'
                turns[-1]['items'][0]['text']=command['params']['input'][0]['text']
                later(.3,lambda turn=turns[-1]:emit(dict(method='turn/completed',params=dict(threadId=thread_id,turn=turn))))
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
