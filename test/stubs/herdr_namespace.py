"""Persistent offline Herdr inventory. Never starts or contacts Herdr."""
import json
import os
from pathlib import Path
import sys
import time
import uuid

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
def environment():return dict(args[i+1].split('=',1) for i,value in enumerate(args) if value=='--env')
def save():
    temporary=state_file.with_suffix('.tmp');temporary.write_text(json.dumps(state),encoding='utf-8');temporary.replace(state_file)
if args==['server']:
    if state.get('restore_without_tokens'):
        for row in state['workspaces'].values():row.pop('tokens',None)
        for row in state['panes'].values():row.pop('tokens',None);row['terminal_id']=str(uuid.uuid4())
    state['server_running']=True;state['server_pid']=os.getpid();save()
    while not (root/'stop-server').exists():time.sleep(.1)
    sys.exit(0)
elif args==['status','server','--json']:
    result={'status':'running' if state.get('server_running',True) else 'not_running','running':state.get('server_running',True),'session':'native-namespace-test','compatible':state.get('server_compatible',True)}
elif args==['api','schema','--json']:result={'title':'Herdr API'}
elif args==['workspace','list']:result={'workspaces':list(state['workspaces'].values())}
elif args==['api','snapshot']:
    result={'snapshot':{'version':'0.9.0','protocol':1,'workspaces':list(state['workspaces'].values()),
                       'panes':list(state['panes'].values()),'agents':list(state.get('agents',{}).values()),'tabs':[],'layouts':[]}}
elif args[:2]==['pane','process-info']:
    pane=state['panes'][option('--pane')]
    pid=pane.get('foreground_pid',state.get('shell_pid'))
    result={'process_info':{'pane_id':pane['pane_id'],'shell_pid':state.get('shell_pid'),
            'foreground_processes':[{'pid':pid,'name':state.get('shell_name','fixture'),'cwd':pane.get('cwd'),'argv':pane.get('argv',[])}],'tty':None,'foreground_process_group_id':None}}
elif args[:2]==['pane','run']:
    import re
    pane=state['panes'][args[2]]
    assert 'codex-tui-attach' in args[3]
    quoted=re.findall(r"'((?:[^']|'')*)'",args[3]);host=json.loads((Path(quoted[-1].replace("''","'"))/'host.json').read_text(encoding='utf-8'))
    pid=state['provider_pids'].pop(0);pane['foreground_pid']=pid;pane['agent']='codex'
    pane['argv']=[host['executable'],'--remote',host['endpoint'],'resume',host['thread_id']]
    row={'name':None,'agent':'codex','agent_status':'idle','pane_id':pane['pane_id'],'workspace_id':pane['workspace_id'],
         'terminal_id':pane['terminal_id'],'tab_id':pane['workspace_id']+':t1','interactive_ready':True,
         'launch_pending':False,'revision':1,'focused':False,'tokens':pane['tokens'].copy()}
    state.setdefault('agents',{})['host-unnamed']=row;save();result={'status':'ok'}
elif args[:2]==['agent','start']:
    name=args[2];pane=state['panes'][option('--pane')];kind=option('--kind')
    assert not pane.get('agent') and not any(a['name']==name for a in state.get('agents',{}).values())
    pid=state['provider_pids'].pop(0)
    pane['foreground_pid']=pid;pane['agent']=kind
    status=state.get('start_status','idle')
    row={'name':name,'agent':kind,'agent_status':status,'pane_id':pane['pane_id'],'workspace_id':pane['workspace_id'],
         'terminal_id':pane['terminal_id'],'tab_id':pane['workspace_id']+':t1','interactive_ready':status in ['idle','done'],
         'launch_pending':False,'revision':1,'focused':False,'tokens':pane['tokens'].copy()}
    if kind in state.get('agent_sessions',{}):row['agent_session']=state['agent_sessions'][kind]
    if kind=='pi' and state.get('pi_hook_authority'):
        row.pop('name');row.pop('interactive_ready');row['screen_detection_skipped']=True
        row['agent_session']['source']='herdr:pi'
    state.setdefault('agents',{})[name]=row;save();result={'type':'agent_started','agent':row,'argv':['DO_NOT_PERSIST_SECRET_ARGUMENT']}
elif args[:2]==['agent','rename']:
    matches=[(key,a) for key,a in state['agents'].items() if a['pane_id']==args[2]];assert len(matches)==1
    key,row=matches[0];del state['agents'][key];row['name']=args[3];state['agents'][args[3]]=row;save()
    result={'type':'agent_renamed','agent':row}
elif args[:2]==['agent','prompt']:
    assert len(args)==4 and '--wait' not in args
    row=state['agents'][args[2]]
    row.update(agent_status='working',interactive_ready=False);save()
    with (root/'prompts.jsonl').open('a',encoding='utf-8') as output:
        output.write(json.dumps({'name':args[2],'body':args[3]})+'\n')
    result={'type':'agent_prompted','agent':row}
elif args[:2]==['pane','report-agent-session']:
    matches=[a for a in state['agents'].values() if a['pane_id']==args[2]]
    assert len(matches)==1
    matches[0]['agent_session']={'agent':option('--agent'),'kind':'path','value':option('--agent-session-path'),'source':option('--source')}
    save();result={'status':'ok'}
elif args[:2]==['pane','list']:
    workspace=option('--workspace');result={'panes':[p for p in state['panes'].values() if p['workspace_id']==workspace]}
elif args[:2]==['workspace','create']:
    assert '--no-focus' in args and '--focus' not in args
    state['next_workspace']+=1;wid=f"w{state['next_workspace']}";pid=wid+':p1'
    workspace={'workspace_id':wid,'label':option('--label'),'cwd':option('--cwd'),'tokens':{}}
    pane={'pane_id':pid,'workspace_id':wid,'tokens':{},'cwd':option('--cwd'),'terminal_id':str(uuid.uuid4()),'env':environment()}
    state['workspaces'][wid]=workspace;state['panes'][pid]=pane;save()
    result={'workspace':workspace,'root_pane':pane}
elif args[:2]==['workspace','close']:
    wid=args[2];assert '--group' not in args
    del state['workspaces'][wid]
    state['panes']={k:v for k,v in state['panes'].items() if v['workspace_id']!=wid}
    state['agents']={k:v for k,v in state.get('agents',{}).items() if v['workspace_id']!=wid}
    save();result={'status':'ok'}
elif args[:2]==['pane','split']:
    assert '--no-focus' in args and '--focus' not in args
    parent=state['panes'][args[2]];wid=parent['workspace_id']
    count=sum(p['workspace_id']==wid for p in state['panes'].values())
    pid=f'{wid}:p{count+1}'
    pane={'pane_id':pid,'workspace_id':wid,'tokens':{},'parent':args[2],
          'direction':option('--direction'),'ratio':option('--ratio'),'cwd':option('--cwd'),'terminal_id':str(uuid.uuid4()),'env':environment()}
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
