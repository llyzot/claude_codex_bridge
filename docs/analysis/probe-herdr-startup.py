"""Bounded no-prompt startup observation for the explicitly authorized test project."""
import json
import pathlib
import subprocess
import sys
import time

project = pathlib.Path(sys.argv[1])
session = sys.argv[2]
ccb = pathlib.Path('C:/Users/user/Desktop/work/ccb-rust/bin/ccb.exe')
runtime = project / '.ccb/ccbd'
start = time.monotonic()
trace = project / 'startup-observation.jsonl'

def emit(row):
    row = {'elapsed_ms': round((time.monotonic()-start)*1000), **row}
    text = json.dumps(row,ensure_ascii=False)
    with trace.open('a',encoding='utf-8') as f:f.write(text+'\n')
    print(text,flush=True)

def read(relative):
    try:return json.loads((runtime/relative).read_text(encoding='utf-8'))
    except (FileNotFoundError,PermissionError,json.JSONDecodeError):return {}

if '--observe-only' not in sys.argv:
    result = subprocess.run([str(ccb),'--project',str(project),'herdr','open','--session',session,'--timeout-ms','1000','--no-attach'],capture_output=True,text=True,encoding='utf-8',timeout=20)
    emit({'event':'open_return','code':result.returncode,'result':json.loads(result.stdout) if result.stdout.strip().startswith('{') else result.stderr.strip()})
else:
    created = int(read('herdr/herdr-open-start.json')['process']['created'])/10000000-11644473600
    start = time.monotonic() - (time.time()-created)
previous = None
while previous is None or time.monotonic()-start < 90:
    opened = read('herdr/herdr-open.json')
    intent = read('herdr/herdr-open-start.json')
    ledger = read('herdr/herdr-agents.json')
    pi = read('pi-terminal-events/pi/current.json')
    snapshot = {'phase':opened.get('phase'),'opener':intent.get('status'),
        'bindings':list(opened.get('bindings',{})), 'pi_ready':pi.get('ready'),
        'agents':{name:{key:row.get(key) for key in ['status','sent','launch_acknowledged','observed_lifecycle','interactive_ready']} for name,row in ledger.get('agents',{}).items()}}
    if snapshot != previous:
        emit({'event':'state',**snapshot});previous=snapshot
    if intent.get('status') in ['finished','failed']:
        emit({'event':'terminal','phase':opened.get('phase'),'opener':intent.get('status')});break
    time.sleep(.1)
else:emit({'event':'observation_timeout'})
