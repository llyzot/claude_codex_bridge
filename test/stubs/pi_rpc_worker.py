"""Persistent Pi protocol fixture; no model, user config or real agent."""
import json
import os
import sys
import threading
import time
from pathlib import Path
import socket

lock=threading.Lock()
state=dict(sessionId=f'session-{os.getpid()}',sessionFile=None,isStreaming=False,isCompacting=False,pendingMessageCount=0,messageCount=0,
           model={'headers':{'Authorization':'never-persist-this'}})
mode=sys.argv[1]
role_plan=Path(os.environ['CCB_PI_ROLE_PLAN']) if os.environ.get('CCB_PI_ROLE_PLAN') else None
loaded_plan=json.loads(role_plan.read_text()) if role_plan else None
bridge_path=sys.argv[sys.argv.index('-e')+1] if '-e' in sys.argv else ''

def role_behavior():
    path=os.environ.get('CCB_PI_SKILL_BEHAVIOR')
    return Path(path).read_text().strip() if path and Path(path).exists() else ''

def role_emit(command,record,phase):
    if role_behavior()=='delay-'+phase:
        timer=threading.Timer(.6,lambda:emit(record));timer.daemon=True;timer.start()
    else:emit(record)
session_file=Path(sys.argv[2]) if mode in {'persist','persist-lead'} else None
if mode=='persist-lead': mode='lead'
if session_file:
    if '--session' in sys.argv:session_file=Path(sys.argv[sys.argv.index('--session')+1])
    if session_file.exists():state.update(json.loads(session_file.read_text()))
    state['sessionFile']=str(session_file)
    session_file.write_text(json.dumps(state))
abort_event=threading.Event()
def emit(value):
    with lock:print(json.dumps(value),flush=True)
def api(op,payload):
    endpoint=json.loads(os.environ['CCB_NATIVE_ENDPOINT'])
    token=json.loads(Path(os.environ['CCB_NATIVE_TOKEN_FILE']).read_text())['token']
    with socket.create_connection((endpoint['host'],endpoint['port']),timeout=3) as stream:
        reader=stream.makefile('rb')
        stream.sendall((json.dumps({'schema':'ccbd-control-plane-token-v1','token':token})+'\n').encode())
        assert json.loads(reader.readline())['ok']
        stream.sendall((json.dumps({'api_version':2,'op':op,'request':payload})+'\n').encode())
        return json.loads(reader.readline())
def prompt(command):
    state['isStreaming']=True
    if mode!='late-ack':emit(dict(type='response',command='prompt',id=command['id'],success=True,data={'disposition':'started'}))
    message=command['message'] if mode!='foreign-anchor' else 'CCB_REQ_ID: foreign-job\nforeign prompt'
    emit(dict(type='message_end',message={'role':'user','content':message}))
    emit(dict(type='message_end',message={'role':'assistant','content':[{'type':'text','text':'intermediate'}],'stopReason':'toolUse'}))
    emit(dict(type='agent_end',messages=[]))
    if mode=='hang':
        abort_event.wait(timeout=10)
        emit(dict(type='message_end',message={'role':'assistant','content':[],'stopReason':'aborted'}))
        state['isStreaming']=False
        emit(dict(type='agent_settled',aborted=True))
        return
    body=command['message'].split('\n',1)[1]
    if mode=='lead' and not body.startswith('CCB result-chain continuation.'):
        result=api('submit',dict(project_id='native-test',to_agent='worker',from_actor='lead',body='delegate from persistent Pi',
                                 route_options={'mode':'chain'}))
        assert result['ok'],result
        reply='delegated'
    else:reply='persistent Pi reviewed result' if mode=='lead' else 'persistent Pi result'
    time.sleep(.15)
    emit(dict(type='message_end',message={'role':'assistant','content':[{'type':'text','text':reply}],'stopReason':'stop'}))
    state['messageCount']+=2;state['isStreaming']=False
    if session_file:session_file.write_text(json.dumps(state))
    emit(dict(type='agent_settled',aborted=False))
    if mode=='late-ack':
        time.sleep(.15)
        emit(dict(type='response',command='prompt',id=command['id'],success=True,data={'disposition':'started'}))

for line in sys.stdin.buffer:
    command=json.loads(line)
    if os.environ.get('CCB_FIXTURE_TRACE'):
        with open(os.environ['CCB_FIXTURE_TRACE'], 'a', encoding='utf-8') as file:
            file.write(json.dumps(command)+'\n')
    if command['type']=='get_state':
        observed=dict(state)
        if command['id'].startswith('role-state-'):
            if role_behavior()=='foreign-session':observed['sessionId']='wrong-session'
            if role_behavior()=='busy':observed['isStreaming']=True
        role_emit(command,dict(type='response',command='get_state',id=command['id'],success=True,data=observed),'state' if command['id'].startswith('role-state-') else 'init')
    elif command['type']=='get_commands':
        rows=[]
        if loaded_plan:
            rows=[dict(name=loaded_plan['command'],description=f"CCB native role bridge {loaded_plan['owner']} {loaded_plan['generation']}",
                       source='extension',sourceInfo={'path':bridge_path})]
            rows += [dict(name='skill:'+Path(root).name,description='Fixture skill',source='skill',
                          sourceInfo={'path':str(Path(root)/'SKILL.md')}) for root in loaded_plan['roots']]
        behavior=role_behavior()
        if behavior=='missing-bridge':rows=[r for r in rows if r['source']!='extension']
        if behavior=='missing-skill':rows=[r for r in rows if r['source']!='skill']
        record=dict(type='response',command='get_commands',id=command['id'],success=True,data={'commands':rows})
        if behavior=='bad-envelope':record['command']='get_state'
        if behavior=='foreign' and command['id'].startswith('role-verify-'):
            emit(dict(record,id='foreign-role-response'))
            timer=threading.Timer(.6,lambda rec=record:emit(rec));timer.daemon=True;timer.start()
        else:role_emit(command,record,'verify' if command['id'].startswith('role-verify-') else 'inspect')
    elif command['type']=='prompt':
        if command['message'].startswith('/ccb-native-role-'):
            behavior=role_behavior()
            if behavior!='stale-generation':loaded_plan=json.loads(role_plan.read_text())
            success=behavior!='reload-error'
            record=dict(type='response',command='prompt',id=command['id'],success=success,
                        data={'disposition':'started' if behavior=='reload-started' else 'handled'})
            if not success:record['error']='PRIVATE_RELOAD_ERROR'
            role_emit(command,record,'reload')
            continue
        threading.Thread(target=prompt,args=(command,),daemon=True).start()
    elif command['type']=='abort':
        emit(dict(type='response',command='abort',id=command['id'],success=True))
        abort_event.set()
