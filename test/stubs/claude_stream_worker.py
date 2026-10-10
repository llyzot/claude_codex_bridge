"""Claude Code stream-json fixture, not a real model."""
import json
import os
import sys
import time
from pathlib import Path
import socket

mode=sys.argv[1]
flag='--resume' if '--resume' in sys.argv else '--session-id'
session=sys.argv[sys.argv.index(flag)+1]
previous=None
def emit(value):print(json.dumps(value),flush=True)
def api(payload):
    endpoint=json.loads(os.environ['CCB_NATIVE_ENDPOINT']);token=json.loads(Path(os.environ['CCB_NATIVE_TOKEN_FILE']).read_text())['token']
    with socket.create_connection((endpoint['host'],endpoint['port']),timeout=3) as stream:
        reader=stream.makefile('rb');stream.sendall((json.dumps({'schema':'ccbd-control-plane-token-v1','token':token})+'\n').encode());assert json.loads(reader.readline())['ok']
        stream.sendall((json.dumps({'api_version':2,'op':'submit','request':payload})+'\n').encode());return json.loads(reader.readline())
emit(dict(type='system',subtype='init',session_id=session))
for line in sys.stdin.buffer:
    user=json.loads(line);body=user['message']['content'].split('\n',1)[1]
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
