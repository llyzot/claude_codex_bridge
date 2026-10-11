"""Installed-command contract fixture. No model credentials or account files."""
import datetime
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

if sys.argv[1]=='environment':
    Path(sys.argv[2]).write_text(json.dumps({key:os.environ.get(key) for key in ['PATH','CCB_NATIVE_CLI','CCB_INSTALL_PREFIX']}),encoding='utf-8')
    sys.exit(subprocess.call(sys.argv[3:]))

job=json.load(sys.stdin)
report=Path(sys.argv[2])
request=job['request']
ping=subprocess.run(['ccb','ping'],capture_output=True,text=True,encoding='utf-8',check=True)
assert json.loads(ping.stdout)['runtime']=='rust'
with report.open('a',encoding='utf-8') as file:
    file.write(json.dumps({'ask':shutil.which('ask'),'ccb':shutil.which('ccb'),
                          'prefix':os.environ.get('CCB_INSTALL_PREFIX'),'kind':request['message_type']})+'\n')
if request['message_type']=='chain_continuation':
    assert 'Codex final result' in request['body']
    reply='installed native Lead reviewed Codex result'
else:
    submitted=subprocess.run(['ask','--chain','worker','delegate from installed Lead'],stdin=subprocess.DEVNULL,
                             capture_output=True,text=True,encoding='utf-8',check=True)
    assert json.loads(submitted.stdout)['status'] in {'accepted','queued'}
    reply='delegated through installed ask'
decision=dict(schema_version=2,record_type='completion_decision',terminal=True,status='completed',reason='task_complete',
              confidence='observed',reply=reply,anchor_seen=True,reply_started=True,reply_stable=True,
              provider_turn_ref=None,source_cursor=None,finished_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),diagnostics={})
print(json.dumps(dict(job_id=job['job_id'],agent_name=job['agent_name'],provider=job['provider'],
                     execution_instance_id=job['provider_options']['native_execution_instance'],decision=decision)))
