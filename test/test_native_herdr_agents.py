"""Offline Herdr wire fixtures with real local PID lifetime observations only."""
import copy,json,os,subprocess,sys,time
from pathlib import Path
import pytest
from test_native_herdr_namespace import setup,run as namespace_run,server,calls,fault,mutate_count
from test_native_cli_security import BIN

pytestmark=pytest.mark.skipif(os.name!='nt',reason='external PID creation identity is Windows-specific')

@pytest.fixture
def prepared(setup):
    project,backend,request=setup
    processes=[subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(120)'],stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,creationflags=0x08000000) for _ in range(3)]
    state=server(backend);state['shell_pid']=os.getpid();state['provider_pids']=[p.pid for p in processes]
    (backend/'server.json').write_text(json.dumps(state))
    namespace_run(request)
    request['action']='mount'
    yield project,backend,request,processes
    for process in processes:
        if process.poll() is None:process.kill()
        process.wait(timeout=5)

def call(request):
    return subprocess.run([str(BIN),'herdr-agents'],input=json.dumps(request),capture_output=True,text=True,encoding='utf-8',timeout=25)
def run(request):
    result=call(request);assert result.returncode==0,result.stderr
    return json.loads(result.stdout)
def ledger(project):return project/'.ccb/ccbd/herdr/herdr-agents.json'
def change(backend,fn):
    state=server(backend);fn(state);(backend/'server.json').write_text(json.dumps(state))

def test_mount_keeps_layout_focus_and_binds_local_process_lifetimes(prepared):
    project,backend,request,processes=prepared;before=server(backend)
    request['interactive_args']={'worker':['-m','example-model','literal;not-a-shell-command']}
    request['interactive_args']['lead']=['--session','PRIVATE_SESSION_ARGUMENT']
    result=run(request)
    assert result['mount']['status']=='ready' and not result['daemon_connected'] and not result['task_completion_supported']
    rows=result['mount']['agents'];assert set(rows)=={'lead','worker'}
    assert len({r['herdr_name'] for r in rows.values()})==2
    assert {r['binding']['process']['pid'] for r in rows.values()}=={p.pid for p in processes[:2]}
    assert all(r['binding']['process']['created'].isdigit() for r in rows.values())
    assert server(backend)['focused']==before['focused']
    assert server(backend)['panes']['external:p1']==before['panes']['external:p1']
    assert mutate_count(backend,['agent','start'])==2
    repeated=run(request);assert repeated['mount']['agents']==rows
    assert mutate_count(backend,['agent','start'])==2
    launch=[r for r in calls(backend) if r[:2]==['agent','start']][-1]
    assert launch[-4:]==['--','-m','example-model','literal;not-a-shell-command']
    first_launch=[r for r in calls(backend) if r[:2]==['agent','start']][0]
    assert first_launch[-3:]==['--','--session','PRIVATE_SESSION_ARGUMENT']
    assert not any(r[:2] in [['pane','run'],['agent','prompt'],['server','stop'],['workspace','focus']] for r in calls(backend))
    text=ledger(project).read_text()
    assert 'literal;not-a-shell-command' not in text and 'DO_NOT_PERSIST_SECRET_ARGUMENT' not in text
    assert 'PRIVATE_SESSION_ARGUMENT' not in text

@pytest.mark.parametrize('mode',['after-error','hang-after'])
def test_lost_start_receipt_recovers_by_unique_alias_without_relaunch(prepared,mode):
    project,backend,request,_=prepared;fault(backend,['agent','start'],mode)
    result=run(request)['mount'];assert result['status']=='ready'
    assert result['agents']['lead']['launch_acknowledged'] is False
    assert mutate_count(backend,['agent','start'])==2
    assert run(request)['mount']['status']=='ready'
    assert mutate_count(backend,['agent','start'])==2

def test_absent_agent_after_failed_submission_is_never_blindly_replayed(prepared):
    _,backend,request,_=prepared;fault(backend,['agent','start'],'before-error')
    first=run(request)['mount'];assert first['status']=='uncertain'
    assert first['agents']['lead']['sent'] and first['agents']['lead']['binding'] is None
    before=mutate_count(backend,['agent','start'])
    for action in ['observe','mount']:
        request['action']=action;assert run(request)['mount']['status']=='uncertain'
    assert mutate_count(backend,['agent','start'])==before

def test_blocked_start_is_bound_without_answering_or_restarting_it(prepared):
    _,backend,request,_=prepared;change(backend,lambda s:s.update(start_status='blocked'))
    result=run(request)['mount'];assert result['status']=='blocked'
    assert all(row['binding'] for row in result['agents'].values())
    before=mutate_count(backend,['agent','start'])
    def idle(state):
        for agent in state['agents'].values():agent.update(agent_status='idle',interactive_ready=True)
    change(backend,idle);request['action']='observe'
    assert run(request)['mount']['status']=='ready'
    assert mutate_count(backend,['agent','start'])==before
    assert not any(row[:2]==['agent','send-keys'] for row in calls(backend))

@pytest.mark.parametrize('kind',['occupied','busy-shell','wrong-cwd','missing-cwd'])
def test_preflight_refuses_unsuitable_shell_before_any_agent_start(prepared,kind):
    project,backend,request,processes=prepared
    namespace=json.loads((project/'.ccb/ccbd/herdr/namespace.json').read_text());pane_id=namespace['bindings']['worker']['pane_id']
    def mutate(state):
        pane=state['panes'][pane_id]
        if kind=='occupied':pane['agent']='claude'
        elif kind=='busy-shell':pane['foreground_pid']=processes[2].pid
        elif kind=='wrong-cwd':pane['cwd']=str(backend)
        else:pane.pop('cwd')
    change(backend,mutate)
    assert call(request).returncode!=0
    assert not ledger(project).exists() and mutate_count(backend,['agent','start'])==0

@pytest.mark.parametrize('kind',['terminal','owner','provider','process','creation','moved-name'])
def test_changed_binding_is_not_adopted_or_replaced(prepared,kind):
    project,backend,request,processes=prepared;first=run(request)['mount'];lead=first['agents']['lead'];pane_id=lead['pane_id']
    if kind=='creation':
        record=json.loads(ledger(project).read_text());record['agents']['lead']['binding']['process']['created']='1';ledger(project).write_text(json.dumps(record))
    else:
        def mutate(state):
            pane=state['panes'][pane_id];agent=state['agents'][lead['herdr_name']]
            if kind=='terminal':pane['terminal_id']='replaced'
            elif kind=='owner':pane['tokens']['ccb_native_owner']='foreign'
            elif kind=='provider':agent['agent']='claude'
            elif kind=='process':pane['foreground_pid']=processes[2].pid
            else:agent['pane_id']='external:p1'
        change(backend,mutate)
    before=mutate_count(backend,['agent','start'])
    assert call(request).returncode!=0
    assert mutate_count(backend,['agent','start'])==before

def test_name_disappearing_retains_lost_binding_without_new_launch(prepared):
    _,backend,request,_=prepared;first=run(request)['mount'];name=first['agents']['lead']['herdr_name']
    change(backend,lambda s:s['agents'].pop(name))
    assert run(request)['mount']['status']=='lost'
    assert mutate_count(backend,['agent','start'])==2


def test_foreground_tool_does_not_replace_live_bound_provider(prepared):
    _,backend,request,processes=prepared;first=run(request)['mount'];lead=first['agents']['lead']
    def working(state):
        state['agents'][lead['herdr_name']].update(agent_status='working',interactive_ready=False)
        state['panes'][lead['pane_id']]['foreground_pid']=processes[2].pid
    change(backend,working);request['action']='observe'
    result=run(request)['mount'];assert result['agents']['lead']['status']=='working'
    assert result['agents']['lead']['binding']==lead['binding']
    def idle(state):
        state['agents'][lead['herdr_name']].update(agent_status='idle',interactive_ready=True)
        state['panes'][lead['pane_id']]['foreground_pid']=lead['binding']['process']['pid']
    change(backend,idle);assert run(request)['mount']['status']=='ready'
    assert mutate_count(backend,['agent','start'])==2

def test_local_inspection_and_plan_do_not_contact_backend_or_rewrite_ledger(prepared):
    project,backend,request,_=prepared;run(request)
    before=ledger(project).read_bytes();count=len(calls(backend))
    for action in ['inspect','plan']:
        request['action']=action;run(request)
    assert ledger(project).read_bytes()==before and len(calls(backend))==count

def test_changed_launch_arguments_require_explicit_migration(prepared):
    _,backend,request,_=prepared;run(request)
    request['interactive_args']={'lead':['--model','changed']}
    assert call(request).returncode!=0 and mutate_count(backend,['agent','start'])==2

def test_controller_death_after_committed_start_intent_does_not_replay(prepared):
    project,backend,request,_=prepared;fault(backend,['agent','start'],'hang-after')
    request['timeout_ms']=10000
    process=subprocess.Popen([str(BIN),'herdr-agents'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8',creationflags=0x08000000)
    process.stdin.write(json.dumps(request));process.stdin.close()
    try:
        until=time.monotonic()+6
        while not (backend/'mutation-finished').exists() and time.monotonic()<until:time.sleep(.02)
        assert (backend/'mutation-finished').exists()
        assert json.loads(ledger(project).read_text())['agents']['lead']['sent']
        process.kill();process.wait(timeout=5)
        assert run(request)['mount']['status']=='ready'
        assert mutate_count(backend,['agent','start'])==2
    finally:
        if process.poll() is None:process.kill();process.wait(timeout=5)
