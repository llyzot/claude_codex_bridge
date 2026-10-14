"""Stopped-server recovery preserves history and refuses unresolved work."""
import json,os,subprocess,sys,uuid
from pathlib import Path
import pytest
from test_native_herdr_namespace import setup,server,fault,mutate_count
from test_native_herdr_agents import change
from test_native_herdr_open import cli,owned_stop
from test_native_herdr_worker import wait_for

pytestmark=pytest.mark.skipif(os.name!='nt',reason='Windows process lifetime proof')

@pytest.fixture
def stopped(setup,request):
    provider=getattr(request,'param','codex')
    project,backend,request=setup;(project/'.ccb').mkdir();(project/'.ccb/ccb.config').write_text(f'worker:{provider}')
    request['config']['agents'].pop('lead');request['config']['agents']['worker']['provider']=provider
    request['config']['source_config']={'layout':f'worker:{provider}'}
    runtime=project/'.ccb/ccbd';state=runtime/'herdr';processes=[]
    def helper():
        p=subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(300)'],creationflags=0x08000000);processes.append(p);return p
    first=helper();session=str(uuid.uuid4());history=project/'history';history.mkdir();log=history/(f'{session}.jsonl' if provider=='claude' else f'rollout-{session}.jsonl')
    header={'type':'user','uuid':'historical','sessionId':session,'cwd':str(project),'message':{'role':'user','content':'history'}} if provider=='claude' else {'type':'session_meta','payload':{'id':session,'cwd':str(project)}}
    log.write_text(json.dumps(header)+'\n',encoding='utf-8')
    s=server(backend);s.update(server_running=False,shell_pid=os.getpid(),provider_pids=[first.pid],agent_sessions={provider:{'agent':provider,'kind':'id','value':session,'source':'fixture'}})
    (backend/'server.json').write_text(json.dumps(s),encoding='utf-8')
    options=project/'open.json';options.write_text(json.dumps(dict(config=request['config'],command=request['command'],session=request['session'],history_roots={'worker':str(history)},transport_timeout_ms=1000)),encoding='utf-8')
    def open(recover=False,ms=10000):return cli('--project',project,'herdr','open','--options',options,'--no-attach','--timeout-ms',ms,*(['--recover-stopped'] if recover else []))
    def stop():
        ping=cli('--state-root',runtime,'ping')
        pid=json.loads(ping.stdout)['pid'] if ping.returncode==0 else None
        cli('--state-root',runtime,'stop')
        if pid:wait_for(lambda:cli('process-identity',pid).returncode!=0)
        record=json.loads((state/'herdr-open.json').read_text(encoding='utf-8'));owned_stop(record.get('server_process'))
        first.kill();first.wait(timeout=5)
        change(backend,lambda s:s.update(server_running=False,restore_without_tokens=True))
    try:
        started=open();assert started.returncode==0,started.stderr
        wait_for(lambda:json.loads((state/'herdr-open.json').read_text(encoding='utf-8')).get('phase')=='running',timeout=30)
        yield dict(project=project,backend=backend,runtime=runtime,state=state,open=open,stop=stop,helper=helper,session=session,log=log,first=first,provider=provider)
    finally:
        start=state/'herdr-open-start.json'
        if start.exists():owned_stop(json.loads(start.read_text(encoding='utf-8')).get('process'))
        cli('--state-root',runtime,'stop')
        record=state/'herdr-open.json'
        if record.exists():owned_stop(json.loads(record.read_text(encoding='utf-8')).get('server_process'))
        for p in processes:
            if p.poll() is None:p.kill()
            p.wait(timeout=5)

@pytest.mark.parametrize('stopped',['codex','claude'],indirect=True)
def test_recover_quiet_server_archives_epoch_and_resumes_exact_history(stopped):
    p=stopped;old=json.loads((p['state']/'namespace.json').read_text(encoding='utf-8'));bytes_before=p['log'].read_bytes();p['stop']()
    next_process=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[next_process.pid]))
    result=p['open'](True);assert result.returncode==0,result.stderr
    wait_for(lambda:cli('--state-root',p['runtime'],'ping').returncode==0)
    new=json.loads((p['state']/'namespace.json').read_text(encoding='utf-8'));assert new['owner_id']!=old['owner_id'] and new['workspace_id']!=old['workspace_id']
    assert p['log'].read_bytes()==bytes_before
    rows=[json.loads(r) for r in (p['backend']/'calls.jsonl').read_text(encoding='utf-8').splitlines()]
    launches=[r for r in rows if r[:2]==['agent','start']];assert len(launches)==2
    selector='--resume' if p['provider']=='claude' else 'resume'
    assert launches[-1][launches[-1].index(selector)+1]==p['session']
    assert mutate_count(p['backend'],['workspace','close'])==1
    assert len((p['state']/'herdr-recoveries.jsonl').read_text(encoding='utf-8').splitlines())==1
    assert p['open'](True).returncode==0 and p['open']().returncode==0
    assert mutate_count(p['backend'],['agent','start'])==2
    assert 'external' in server(p['backend'])['workspaces']

def test_recovery_reconciles_stopped_work_without_repeating_its_prompt(stopped):
    p=stopped;asked=cli('--state-root',p['runtime'],'ask','worker','must not replay');assert asked.returncode==0,asked.stderr
    wait_for(lambda:mutate_count(p['backend'],['agent','prompt'])==1)
    p['stop']();replacement=p['helper']()
    def restored_idle(s):
        s['provider_pids']=[replacement.pid]
        for agent in s['agents'].values():agent.update(agent_status='idle',interactive_ready=True)
    change(p['backend'],restored_idle)
    result=p['open'](True);assert result.returncode==0,result.stderr
    watched=cli('--state-root',p['runtime'],'watch',json.loads(asked.stdout)['job_id'])
    assert json.loads(watched.stdout)['status']=='incomplete'
    assert mutate_count(p['backend'],['agent','prompt'])==1
    assert (p['state']/'herdr-recoveries.jsonl').exists()

def test_lost_close_ack_does_not_duplicate_retirement_or_launch(stopped):
    p=stopped;p['stop']();proc=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[proc.pid]))
    fault(p['backend'],['workspace','close'],'after-error')
    result=p['open'](True);assert result.returncode==0,result.stderr
    assert mutate_count(p['backend'],['workspace','close'])==1 and mutate_count(p['backend'],['agent','start'])==2

@pytest.mark.parametrize('missing_history',[False,True])
def test_restored_foreign_session_is_not_closed(stopped,missing_history):
    p=stopped;p['stop']()
    if missing_history:p['log'].unlink()
    def foreign(s):
        for a in s['agents'].values():a['agent_session']['value']=str(uuid.uuid4())
    change(p['backend'],foreign)
    result=p['open'](True);assert result.returncode!=0 and 'unverified native session' in result.stderr
    assert mutate_count(p['backend'],['workspace','close'])==0 and mutate_count(p['backend'],['agent','start'])==1

def test_recovery_resumes_after_controller_dies_following_workspace_close(stopped):
    p=stopped;p['stop']();proc=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[proc.pid]))
    fault(p['backend'],['workspace','close'],'hang-after')
    assert p['open'](True,0).returncode==0
    wait_for(lambda:(p['backend']/'mutation-finished').exists())
    intent=json.loads((p['state']/'herdr-open-start.json').read_text(encoding='utf-8'));owned_stop(intent['process'])
    wait_for(lambda:cli('process-identity',intent['process']['pid']).returncode!=0)
    result=p['open'](True);assert result.returncode==0,result.stderr
    assert mutate_count(p['backend'],['workspace','close'])==1 and mutate_count(p['backend'],['agent','start'])==2
    assert len((p['state']/'herdr-recoveries.jsonl').read_text(encoding='utf-8').splitlines())==1

def test_missing_verified_history_is_not_replaced_by_a_fresh_session(stopped):
    p=stopped;p['stop']();p['log'].unlink()
    result=p['open'](True,ms=0);assert result.returncode==0,result.stderr
    wait_for(lambda:cli('--state-root',p['runtime'],'ping').returncode==0,timeout=30)
    actor=json.loads(cli('--state-root',p['runtime'],'agents').stdout)['agents'][0]
    assert actor['activation']['reason']=='recovery_evidence_required'
    assert not p['log'].exists()
    assert mutate_count(p['backend'],['agent','start'])==1

def test_extra_restored_pane_is_not_closed(stopped):
    p=stopped;p['stop']()
    old=json.loads((p['state']/'namespace.json').read_text(encoding='utf-8'))
    change(p['backend'],lambda s:s['panes'].update({old['workspace_id']+':p99':{'pane_id':old['workspace_id']+':p99','workspace_id':old['workspace_id']}}))
    result=p['open'](True);assert result.returncode!=0 and 'extra or missing panes' in result.stderr
    assert mutate_count(p['backend'],['workspace','close'])==0

def test_absent_old_workspace_does_not_allow_duplicate_live_session_elsewhere(stopped):
    p=stopped;p['stop']();proc=p['helper']();old=json.loads((p['state']/'namespace.json').read_text(encoding='utf-8'))
    def move(s):
        s['provider_pids']=[proc.pid];s['workspaces'].pop(old['workspace_id'])
        s['panes']={k:v for k,v in s['panes'].items() if v['workspace_id']!=old['workspace_id']}
        for a in s['agents'].values():a['workspace_id']='external';a['pane_id']='external:p1'
    change(p['backend'],move)
    result=p['open'](True);assert result.returncode!=0 and 'already active' in result.stderr
    assert mutate_count(p['backend'],['agent','start'])==1

def held_worker(p,aside=None):
    """Leave a sent execution whose native history is gone, then recover it into an explicit hold."""
    asked=cli('--state-root',p['runtime'],'ask','worker','must not replay');assert asked.returncode==0,asked.stderr
    wait_for(lambda:mutate_count(p['backend'],['agent','prompt'])==1)
    p['stop']()
    if aside:p['log'].rename(aside)
    else:p['log'].unlink()
    replacement=p['helper']()
    def restored_idle(s):
        s['provider_pids']=[replacement.pid]
        for agent in s['agents'].values():agent.update(agent_status='idle',interactive_ready=True)
    change(p['backend'],restored_idle)
    result=p['open'](True);assert result.returncode==0,result.stderr
    wait_for(lambda:cli('--state-root',p['runtime'],'ping').returncode==0,timeout=30)
    actor=json.loads(cli('--state-root',p['runtime'],'agents').stdout)['agents'][0]
    assert actor['activation']['reason']=='recovery_evidence_required'
    return json.loads(asked.stdout)['job_id'],replacement

def stop_all(p):
    p['stop']();start=p['state']/'herdr-open-start.json'
    if start.exists():owned_stop(json.loads(start.read_text(encoding='utf-8')).get('process'))
    wait_for(lambda:not json.loads(cli('--state-root',p['runtime'],'keeper-status').stdout)['alive'],timeout=20)

def repair(p,*extra):return cli('--project',p['project'],'herdr','repair','worker','--fresh-session',*extra)

@pytest.mark.parametrize('stopped',['codex','claude'],indirect=True)
def test_operator_fresh_session_releases_held_agent_without_replay(stopped):
    p=stopped;job,held=held_worker(p)
    live=repair(p);assert live.returncode!=0 and ('stopped' in live.stderr or 'ccb kill' in live.stderr)
    stop_all(p);held.kill();held.wait(timeout=5)
    plan=repair(p,'--dry-run');assert plan.returncode==0,plan.stderr
    planned=json.loads(plan.stdout)
    assert planned['status']=='planned' and planned['agent']=='worker' and planned['mutations'] is False
    assert [j['job_id'] for j in planned['abandoned_executions']]==[job] and planned['prior_error']
    assert not (p['state']/'operator-repairs.json').exists()
    done=repair(p);assert done.returncode==0,done.stderr
    assert json.loads(done.stdout)['status']=='requested'
    again=repair(p);assert again.returncode==0 and json.loads(again.stdout)['operation_id']==json.loads(done.stdout)['operation_id']
    fresh=str(uuid.uuid4());history=p['log'].parent
    header={'type':'user','uuid':'fresh','sessionId':fresh,'cwd':str(p['project']),'message':{'role':'user','content':'fresh'}} if p['provider']=='claude' else {'type':'session_meta','payload':{'id':fresh,'cwd':str(p['project'])}}
    (history/(f'{fresh}.jsonl' if p['provider']=='claude' else f'rollout-{fresh}.jsonl')).write_text(json.dumps(header)+'\n',encoding='utf-8')
    replacement=p['helper']()
    def fresh_session(s):
        s['provider_pids']=[replacement.pid]
        for agent in s['agents'].values():agent['agent_session']['value']=fresh
    change(p['backend'],fresh_session)
    result=p['open'](True);assert result.returncode==0,result.stderr
    wait_for(lambda:cli('--state-root',p['runtime'],'ping').returncode==0,timeout=30)
    watched=json.loads(cli('--state-root',p['runtime'],'watch',job).stdout)
    assert watched['status']=='incomplete'
    assert mutate_count(p['backend'],['agent','prompt'])==1
    launches=[r for r in json.loads('['+','.join((p['backend']/'calls.jsonl').read_text(encoding='utf-8').splitlines())+']') if r[:2]==['agent','start']]
    selector='--resume' if p['provider']=='claude' else 'resume'
    assert selector not in launches[-1] and p['session'] not in launches[-1]
    wait_for(lambda:json.loads(cli('--state-root',p['runtime'],'agents').stdout)['agents'][0]['activation']['session_verified'],timeout=30)
    assert not json.loads((p['state']/'operator-repairs.json').read_text(encoding='utf-8')).get('worker')
    audit=[json.loads(r) for r in (p['state']/'operator-repairs.jsonl').read_text(encoding='utf-8').splitlines()]
    assert [a['phase'] for a in audit]==['requested','applied']

def test_fresh_session_repair_refuses_agent_that_is_not_held(stopped):
    p=stopped;stop_all(p)
    result=repair(p);assert result.returncode!=0 and 'not held' in result.stderr
    assert not (p['state']/'operator-repairs.json').exists()

def test_held_agent_survives_a_second_stopped_recovery_without_replay(stopped):
    p=stopped;job,held=held_worker(p)
    stop_all(p);held.kill();held.wait(timeout=5)
    replacement=p['helper']()
    change(p['backend'],lambda s:s.update(provider_pids=[replacement.pid]))
    result=p['open'](True);assert result.returncode==0,result.stderr
    wait_for(lambda:cli('--state-root',p['runtime'],'ping').returncode==0,timeout=30)
    actor=json.loads(cli('--state-root',p['runtime'],'agents').stdout)['agents'][0]
    assert actor['activation']['reason']=='recovery_evidence_required'
    assert json.loads(cli('--state-root',p['runtime'],'watch',job).stdout)['status']=='running'
    assert mutate_count(p['backend'],['agent','prompt'])==1
    assert len((p['state']/'herdr-recoveries.jsonl').read_text(encoding='utf-8').splitlines())==2

def test_pending_fresh_session_repair_is_superseded_when_evidence_returns(stopped):
    p=stopped;aside=p['log'].with_suffix('.unavailable');job,held=held_worker(p,aside)
    stop_all(p);held.kill();held.wait(timeout=5)
    assert repair(p).returncode==0
    aside.rename(p['log']);replacement=p['helper']()
    change(p['backend'],lambda s:s.update(provider_pids=[replacement.pid]))
    result=p['open'](True);assert result.returncode==0,result.stderr
    launches=[r for r in json.loads('['+','.join((p['backend']/'calls.jsonl').read_text(encoding='utf-8').splitlines())+']') if r[:2]==['agent','start']]
    selector='--resume' if p['provider']=='claude' else 'resume'
    assert launches[-1][launches[-1].index(selector)+1]==p['session']
    assert mutate_count(p['backend'],['agent','prompt'])==1
    assert not json.loads((p['state']/'operator-repairs.json').read_text(encoding='utf-8')).get('worker')
    audit=[json.loads(r) for r in (p['state']/'operator-repairs.jsonl').read_text(encoding='utf-8').splitlines()]
    assert [a['phase'] for a in audit]==['requested','superseded']

def test_terminal_observation_fault_is_settled_by_stopped_recovery_without_replay(stopped):
    p=stopped;asked=cli('--state-root',p['runtime'],'ask','worker','must not replay');assert asked.returncode==0,asked.stderr
    job=json.loads(asked.stdout)['job_id'];wait_for(lambda:mutate_count(p['backend'],['agent','prompt'])==1)
    name,row=next(iter(server(p['backend'])['agents'].items()))
    change(p['backend'],lambda s:s['agents'].pop(name))
    fault=p['runtime']/f"providers/{p['provider']}/worker/fault.json"
    wait_for(lambda:fault.exists() and json.loads(fault.read_text(encoding='utf-8'))['status']=='blocked',timeout=20)
    agents=json.loads(cli('--state-root',p['runtime'],'agents').stdout)['agents']
    assert agents[0]['activation']['reason']=='provider_fault'
    change(p['backend'],lambda s:s['agents'].update({name:row}))
    stop_all(p);replacement=p['helper']()
    def restored_idle(s):
        s['provider_pids']=[replacement.pid]
        for agent in s['agents'].values():agent.update(agent_status='idle',interactive_ready=True)
    change(p['backend'],restored_idle)
    result=p['open'](True);assert result.returncode==0,result.stderr
    wait_for(lambda:cli('--state-root',p['runtime'],'ping').returncode==0,timeout=30)
    assert json.loads(cli('--state-root',p['runtime'],'watch',job).stdout)['status']=='incomplete'
    recovered=json.loads(fault.read_text(encoding='utf-8'))
    assert recovered['status']=='recovered' and recovered['recovered_by']=='stopped_terminal_reconciliation'
    wait_for(lambda:json.loads(cli('--state-root',p['runtime'],'agents').stdout)['agents'][0]['activation']['session_verified'],timeout=30)
    assert mutate_count(p['backend'],['agent','prompt'])==1

def test_held_agent_survives_kill_and_retired_namespace_recovery(stopped):
    p=stopped;job,held=held_worker(p)
    killed=cli('--project',p['project'],'kill',timeout=60);assert killed.returncode==0,killed.stderr
    held.kill();held.wait(timeout=5)
    replacement=p['helper']()
    change(p['backend'],lambda s:s.update(provider_pids=[replacement.pid]))
    result=p['open']();assert result.returncode==0,result.stderr
    wait_for(lambda:cli('--state-root',p['runtime'],'ping').returncode==0,timeout=30)
    actor=json.loads(cli('--state-root',p['runtime'],'agents').stdout)['agents'][0]
    assert actor['activation']['reason']=='recovery_evidence_required'
    assert json.loads(cli('--state-root',p['runtime'],'watch',job).stdout)['status']=='running'
    assert mutate_count(p['backend'],['agent','prompt'])==1

@pytest.mark.parametrize('stopped',['codex','claude'],indirect=True)
def test_dead_provider_reopen_retains_session_evidence_for_recovery(stopped):
    p=stopped;name,row=next(iter(server(p['backend'])['agents'].items()))
    change(p['backend'],lambda s:s['agents'].pop(name))
    p['open'](ms=3000)
    record=json.loads((p['state']/'herdr-open.json').read_text(encoding='utf-8'))
    assert 'worker' not in record['bindings'] and record['binding_errors']['worker']
    assert record['retained_bindings']['worker']['history']['session_id']==p['session']
    change(p['backend'],lambda s:s['agents'].update({name:row}))
    stop_all(p);replacement=p['helper']()
    def restored_idle(s):
        s['provider_pids']=[replacement.pid]
        for agent in s['agents'].values():agent.update(agent_status='idle',interactive_ready=True)
    change(p['backend'],restored_idle)
    result=p['open'](True);assert result.returncode==0,result.stderr
    launches=[r for r in json.loads('['+','.join((p['backend']/'calls.jsonl').read_text(encoding='utf-8').splitlines())+']') if r[:2]==['agent','start']]
    selector='--resume' if p['provider']=='claude' else 'resume'
    assert launches[-1][launches[-1].index(selector)+1]==p['session']
