"""Offline Herdr CLI contract fixture; never contacts a real Herdr session."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

sys.stdout.reconfigure(encoding='utf-8')
sys.stderr.reconfigure(encoding='utf-8')

mode, root, *args=sys.argv[1:]
root=Path(root)
with (root/'calls.jsonl').open('a',encoding='utf-8') as stream:
    stream.write(json.dumps({'args':args,'env':{k:v for k,v in os.environ.items() if k.startswith('HERDR_')}})+'\n')
assert args[:2]==['--session','test 独立 session']
secret='do-not-echo-this-error-or-prompt'
if mode=='hang':
    (root/'cli.pid').write_text(str(os.getpid()))
    time.sleep(30)
elif mode=='descendant':
    child=subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(30)'])
    (root/'descendant.pid').write_text(str(child.pid))
    print(json.dumps({'ok':True,'result':{'accepted':True}}),flush=True)
elif mode=='stderr-flood':
    sys.stderr.write(secret*500000);sys.stderr.flush();time.sleep(30)
elif mode=='stdout-flood':
    sys.stdout.write(secret*500000);sys.stdout.flush();time.sleep(30)
elif mode=='missing-server':
    print(json.dumps({'error':{'code':'server_not_running','message':secret}}),file=sys.stderr)
    sys.exit(1)
elif mode=='failed-envelope':print(json.dumps({'ok':True,'result':{'status':'failed','error':secret}}))
elif mode=='unknown-status':print(json.dumps({'result':{'status':'uncertain-new-status'}}))
elif mode=='boolean-error':print(json.dumps({'error':True}))
elif mode.startswith('server-status-'):
    running=mode=='server-status-running'
    print(json.dumps({'status':'running' if running or mode=='server-status-conflict' else 'not_running','running':running,'session':'test 独立 session'}))
elif mode=='bad-json':print(secret)
elif mode=='bad-utf8':sys.stdout.buffer.write(b'\xff'+secret.encode())
elif mode=='stdin-eof':
    assert not sys.stdin.read()
    print(json.dumps({'ok':True,'result':{'closed_stdin':True}}))
else:
    if args[2:]==['workspace','list']:
        print(json.dumps({'result':{'workspaces':[{'workspace_id':'w1','label':'fixture'}]}}))
    elif args[2:]==['api','schema','--json']:print(json.dumps({'title':'Herdr API'}))
    elif args[2:]==['--version']:print('herdr fixture')
    elif args[2:]==['status','--json']:print(json.dumps({'client':{'version':'fixture'},'server':{'socket':'fixture'}}))
    elif args[2:4]==['pane','read']:print('fixture text\n中文 output')
    else:print(json.dumps({'ok':True,'result':{'accepted':True}}))
