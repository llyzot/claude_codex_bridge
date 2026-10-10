"""Contract fixture, never a real Pi/Codex/Claude provider."""
import datetime
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time

def rpc(op, request):
    endpoint=json.loads(os.environ['CCB_NATIVE_ENDPOINT'])
    token=json.loads(Path(os.environ['CCB_NATIVE_TOKEN_FILE']).read_text(encoding='utf-8'))['token']
    with socket.create_connection((endpoint['host'],endpoint['port']),timeout=3) as stream:
        stream.sendall((json.dumps({'schema':'ccbd-control-plane-token-v1','token':token})+'\n').encode())
        reader=stream.makefile('rb')
        assert json.loads(reader.readline())['ok']
        stream.sendall((json.dumps({'api_version':2,'op':op,'request':request})+'\n').encode())
        response=json.loads(reader.readline())
        assert response['ok'], response
        return response

mode=sys.argv[1]
if mode=='slow-input':time.sleep(.7)
job=json.load(sys.stdin)
if mode=='descendant':
    child=subprocess.Popen([sys.executable,'-I','-c','import time; time.sleep(60)'])
    Path(sys.argv[2]).write_text(str(child.pid),encoding='utf-8')
    time.sleep(60)
request=job['request']
reply='worker result'
status='completed'
reason='task_complete'
if mode=='lead':
    if request['message_type']=='chain_continuation':reply='reviewed: worker result'
    elif request['message_type']=='reply_delivery':reply='received: '+request['body']
    else:
        receipt=rpc('submit',dict(project_id=request['project_id'],to_agent='worker',from_actor=job['agent_name'],
                                 body='perform delegated task',message_type='ask',delivery_scope='single',route_options={'mode':'chain'}))
        assert receipt['status'] in {'accepted','queued'}
        reply='delegated'
elif mode=='retry-once':
    if not job['provider_options'].get('retry_source_job_id'):
        status='failed';reason='api_error';reply='temporary provider API error'
    else:reply='retry succeeded'
elif mode=='foreign':
    job['job_id']='foreign-job'
elif mode=='worker':time.sleep(.35)
record=dict(schema_version=2,record_type='completion_decision',terminal=True,status=status,reason=reason,
            confidence='observed',reply=reply,anchor_seen=True,reply_started=True,reply_stable=True,
            provider_turn_ref=None,source_cursor=None,finished_at=datetime.datetime.now(datetime.timezone.utc).isoformat().replace('+00:00','Z'),diagnostics={})
print(json.dumps(dict(job_id=job['job_id'],agent_name=job['agent_name'],provider=job['provider'],
                      execution_instance_id=job['provider_options']['native_execution_instance'],decision=record)),flush=True)
