"""Claude Code stream-json fixture, not a real model."""
import json
import os
import sys
import time
from pathlib import Path
import socket
import threading
import re

mode=sys.argv[1]
flag='--resume' if '--resume' in sys.argv else '--session-id'
session=sys.argv[sys.argv.index(flag)+1]
previous=None
lock=threading.Lock()
previous_skills=[]
def emit(value):
    with lock:print(json.dumps(value),flush=True)

def behavior():
    path=os.environ.get('CCB_CLAUDE_SKILL_BEHAVIOR')
    return Path(path).read_text().strip() if path and Path(path).exists() else ''

def catalogue():
    root=Path(sys.argv[sys.argv.index('--add-dir')+1])/'.claude/skills'
    rows=[]
    for directory in root.iterdir():
        if directory.name.startswith('.'):continue
        text=(directory/'SKILL.md').read_text(encoding='utf-8')
        name=re.search(r'^name:\s*(.+)$',text,re.M)
        rows.append(dict(name=name.group(1) if name else directory.name,description='Fixture role skill',argumentHint='',aliases=[directory.name]))
    return rows
def api(payload):
    endpoint=json.loads(os.environ['CCB_NATIVE_ENDPOINT']);token=json.loads(Path(os.environ['CCB_NATIVE_TOKEN_FILE']).read_text())['token']
    with socket.create_connection((endpoint['host'],endpoint['port']),timeout=3) as stream:
        reader=stream.makefile('rb');stream.sendall((json.dumps({'schema':'ccbd-control-plane-token-v1','token':token})+'\n').encode());assert json.loads(reader.readline())['ok']
        stream.sendall((json.dumps({'api_version':2,'op':'submit','request':payload})+'\n').encode());return json.loads(reader.readline())
emit(dict(type='system',subtype='init',session_id=session))
for line in sys.stdin.buffer:
    user=json.loads(line)
    if os.environ.get('CCB_FIXTURE_TRACE'):
        with open(os.environ['CCB_FIXTURE_TRACE'], 'a', encoding='utf-8') as file:
            file.write(json.dumps(user)+'\n')
    if user['type']=='control_request':
        kind=user['request']['subtype'];mode_now=behavior()
        if kind=='initialize':
            if mode_now=='init-exit':sys.exit(0)
            response=dict(subtype='error' if mode_now=='init-error' else 'success',request_id=user['request_id'],response={})
        elif kind=='reload_skills':
            rows=catalogue()
            if mode_now=='missing':rows=[]
            if mode_now=='duplicate':rows=rows+rows
            if mode_now=='collision':rows=rows+[dict(name=r['name'],builtin=True,description='Builtin collision',argumentHint='') for r in rows]
            if mode_now=='builtin':rows=[dict(r,builtin=True) for r in rows]
            if mode_now=='stale':rows=previous_skills
            else:previous_skills=rows
            response=dict(subtype='error' if mode_now=='reject' else 'success',request_id=user['request_id'],response={'skills':rows})
            if mode_now=='reject':response['error']='PRIVATE_REFRESH_ERROR'
            if mode_now=='malformed':response['subtype']='unknown'
        else:response=dict(subtype='error',request_id=user['request_id'],error='unsupported fixture request')
        result=dict(type='control_response',response=response)
        if mode_now=='foreign' and kind=='reload_skills':emit(dict(type='control_response',response=dict(response,request_id='foreign')))
        if mode_now in ('delay','foreign') and kind=='reload_skills':
            timer=threading.Timer(.6,lambda result=result:emit(result));timer.daemon=True;timer.start()
        else:emit(result)
        continue
    body=user['message']['content'].split('\n',1)[1]
    emit(user)
    if mode=='duplicate' and previous:emit(previous)
    emit(dict(type='assistant',session_id=session,parent_tool_use_id='subagent',message={'content':[{'type':'text','text':'wrong subagent'}]}))
    if mode=='lead' and not body.startswith('CCB result-chain continuation.'):
        result=api(dict(project_id='native-test',to_agent='worker',from_actor='lead',body='delegated',route_options={'mode':'chain'}));assert result['ok']
        reply='delegated'
    else:reply='Claude reviewed result' if mode=='lead' else 'Claude final result'
    emit(dict(type='assistant',session_id=session,parent_tool_use_id=None,message={'content':[{'type':'text','text':'intermediate'}]}))
    time.sleep(.1)
    record=dict(type='result',subtype='success',is_error=False,session_id='foreign' if mode=='foreign' else session,uuid='result-'+user['uuid'],parent_tool_use_id=None,result=reply)
    emit(record);previous=record
