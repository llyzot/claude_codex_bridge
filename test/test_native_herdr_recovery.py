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
def stopped(setup):
    project,backend,request=setup;(project/'.ccb').mkdir();(project/'.ccb/ccb.config').write_text('worker:codex')
    request['config']['agents'].pop('lead');request['config']['source_config']={'layout':'worker:codex'}
    runtime=project/'.ccb/ccbd';state=runtime/'herdr';processes=[]
    def helper():
        p=subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(300)'],creationflags=0x08000000);processes.append(p);return p
    first=helper();session=str(uuid.uuid4());history=project/'history';history.mkdir();log=history/f'rollout-{session}.jsonl'
    log.write_text(json.dumps({'type':'session_meta','payload':{'id':session,'cwd':str(project)}})+'\n',encoding='utf-8')
    s=server(backend);s.update(server_running=False,shell_pid=os.getpid(),provider_pids=[first.pid],agent_sessions={'codex':{'agent':'codex','kind':'id','value':session,'source':'fixture'}})
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
        wait_for(lambda:cli('--state-root',runtime,'ping').returncode==0)
        yield dict(project=project,backend=backend,runtime=runtime,state=state,open=open,stop=stop,helper=helper,session=session,log=log,first=first)
    finally:
        start=state/'herdr-open-start.json'
        if start.exists():owned_stop(json.loads(start.read_text(encoding='utf-8')).get('process'))
        cli('--state-root',runtime,'stop')
        record=state/'herdr-open.json'
        if record.exists():owned_stop(json.loads(record.read_text(encoding='utf-8')).get('server_process'))
        for p in processes:
            if p.poll() is None:p.kill()
            p.wait(timeout=5)

def test_recover_quiet_server_archives_epoch_and_resumes_exact_codex_history(stopped):
    p=stopped;old=json.loads((p['state']/'namespace.json').read_text(encoding='utf-8'));bytes_before=p['log'].read_bytes();p['stop']()
    next_process=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[next_process.pid]))
    result=p['open'](True);assert result.returncode==0,result.stderr
    wait_for(lambda:cli('--state-root',p['runtime'],'ping').returncode==0)
    new=json.loads((p['state']/'namespace.json').read_text(encoding='utf-8'));assert new['owner_id']!=old['owner_id'] and new['workspace_id']!=old['workspace_id']
    assert p['log'].read_bytes()==bytes_before
    rows=[json.loads(r) for r in (p['backend']/'calls.jsonl').read_text(encoding='utf-8').splitlines()]
    launches=[r for r in rows if r[:2]==['agent','start']];assert len(launches)==2 and launches[-1][-3:]==['--','resume',p['session']]
    assert mutate_count(p['backend'],['workspace','close'])==1
    assert len((p['state']/'herdr-recoveries.jsonl').read_text(encoding='utf-8').splitlines())==1
    assert p['open'](True).returncode==0 and p['open']().returncode==0
    assert mutate_count(p['backend'],['agent','start'])==2
    assert 'external' in server(p['backend'])['workspaces']

def test_recovery_blocks_unfinished_jobs_before_server_start(stopped):
    p=stopped;asked=cli('--state-root',p['runtime'],'ask','worker','must not replay');assert asked.returncode==0,asked.stderr
    wait_for(lambda:mutate_count(p['backend'],['agent','prompt'])==1)
    p['stop']();before=mutate_count(p['backend'],['agent','start'])
    result=p['open'](True);assert result.returncode!=0 and 'unfinished' in result.stderr
    assert not server(p['backend'])['server_running'] and mutate_count(p['backend'],['agent','start'])==before
    assert not (p['state']/'herdr-recoveries.jsonl').exists()

def test_lost_close_ack_does_not_duplicate_retirement_or_launch(stopped):
    p=stopped;p['stop']();proc=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[proc.pid]))
    fault(p['backend'],['workspace','close'],'after-error')
    result=p['open'](True);assert result.returncode==0,result.stderr
    assert mutate_count(p['backend'],['workspace','close'])==1 and mutate_count(p['backend'],['agent','start'])==2

def test_restored_foreign_session_is_not_closed(stopped):
    p=stopped;p['stop']()
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
    result=p['open'](True);assert result.returncode!=0
    assert not server(p['backend'])['server_running'] and mutate_count(p['backend'],['workspace','close'])==0

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
