"""Persistent offline Herdr inventory. Never starts or contacts Herdr."""
import json
import os
from pathlib import Path
import sys
import time

sys.stdout.reconfigure(encoding='utf-8');sys.stderr.reconfigure(encoding='utf-8')
root=Path(sys.argv[1]);args=sys.argv[2:]
assert args[:2]==['--session','native-namespace-test'];args=args[2:]
state_file=root/'server.json'
state=json.loads(state_file.read_text(encoding='utf-8'))
with (root/'calls.jsonl').open('a',encoding='utf-8') as output:output.write(json.dumps(args)+'\n')
fault_file=root/'fault.json'
fault=json.loads(fault_file.read_text()) if fault_file.exists() else {}
trigger=fault.get('command')==args[:2] and not fault.get('consumed')
if trigger:
    fault['consumed']=True;fault_file.write_text(json.dumps(fault))
    if fault.get('mode')=='before-error':sys.exit(1)
def option(name):return args[args.index(name)+1]
def save():
    temporary=state_file.with_suffix('.tmp');temporary.write_text(json.dumps(state),encoding='utf-8');temporary.replace(state_file)
if args==['api','schema','--json']:result={'title':'Herdr API'}
elif args==['workspace','list']:result={'workspaces':list(state['workspaces'].values())}
elif args[:2]==['pane','list']:
    workspace=option('--workspace');result={'panes':[p for p in state['panes'].values() if p['workspace_id']==workspace]}
elif args[:2]==['workspace','create']:
    assert '--no-focus' in args and '--focus' not in args
    state['next_workspace']+=1;wid=f"w{state['next_workspace']}";pid=wid+':p1'
    workspace={'workspace_id':wid,'label':option('--label'),'cwd':option('--cwd'),'tokens':{}}
    pane={'pane_id':pid,'workspace_id':wid,'tokens':{}}
    state['workspaces'][wid]=workspace;state['panes'][pid]=pane;save()
    result={'workspace':workspace,'root_pane':pane}
elif args[:2]==['pane','split']:
    assert '--no-focus' in args and '--focus' not in args
    parent=state['panes'][args[2]];wid=parent['workspace_id']
    count=sum(p['workspace_id']==wid for p in state['panes'].values())
    pid=f'{wid}:p{count+1}'
    pane={'pane_id':pid,'workspace_id':wid,'tokens':{},'parent':args[2],
          'direction':option('--direction'),'ratio':option('--ratio'),'cwd':option('--cwd')}
    state['panes'][pid]=pane;save();result={'pane':pane}
elif args[1]=='report-metadata':
    collection='workspaces' if args[0]=='workspace' else 'panes'
    row=state[collection][args[2]]
    for index,value in enumerate(args):
        if value=='--token':
            key,token=args[index+1].split('=',1);row['tokens'][key]=token
        if value=='--title':row['title']=args[index+1]
    save();result={'status':'ok'}
else:raise AssertionError(args)
if trigger and fault.get('mode')=='after-error':sys.exit(1)
if trigger and fault.get('mode')=='hang-after':
    (root/'mutation-finished').write_text('ready')
    time.sleep(30)
if args[1]=='report-metadata' and state.get('metadata_output') in {'empty','plain'}:
    if state['metadata_output']=='plain':print('metadata updated',flush=True)
else:print(json.dumps({'result':result}),flush=True)
