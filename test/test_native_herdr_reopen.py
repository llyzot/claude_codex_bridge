"""A deliberate project kill can reopen its verified history without server restart."""
import json
import os
import subprocess
import sys
import time
import uuid
from pathlib import Path
import pytest
from test_native_herdr_recovery import stopped
from test_native_herdr_namespace import setup,server,mutate_count,fault
from test_native_herdr_agents import change
from test_native_herdr_open import cli,owned_stop,project,NODE
from test_native_herdr_worker import wait_for

pytestmark=pytest.mark.skipif(os.name!='nt',reason='Windows process lifetime recovery')

def read(path):
    # A live Windows atomic replacement can briefly deny a reader. Retry this
    # observation only; never restart a process or suppress malformed JSON.
    for attempt in range(11):
        try:return json.loads(path.read_text(encoding='utf-8'))
        except (PermissionError,FileNotFoundError):
            if attempt==10:raise
            time.sleep(.02)

def retire(p):
    old=read(p['state']/'namespace.json')
    result=cli('--state-root',p['runtime'],'kill',timeout=25)
    assert result.returncode==0,result.stderr
    assert json.loads(result.stdout)['status']=='closed'
    # The offline transport closes metadata only. Explicitly end its owned dummy
    # foreground process to model the real terminal-close process lifetime.
    p['first'].kill();p['first'].wait(timeout=5)
    return old

@pytest.mark.parametrize('stopped',['codex','claude'],indirect=True)
def test_reopen_after_kill_preserves_exact_session_and_running_server(stopped):
    p=stopped;server_before=read(p['state']/'herdr-open.json')['server_process'];before=p['log'].read_bytes()
    old=retire(p);replacement=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[replacement.pid]))
    result=p['open']();assert result.returncode==0,result.stderr
    wait_for(lambda:read(p['state']/'herdr-open.json')['phase']=='running',timeout=20)
    after=read(p['state']/'namespace.json')
    assert after['owner_id']!=old['owner_id'] and after['workspace_id']!=old['workspace_id']
    assert read(p['state']/'herdr-open.json')['server_process']==server_before
    assert p['log'].read_bytes()==before
    rows=[json.loads(s) for s in (p['backend']/'calls.jsonl').read_text().splitlines()]
    launches=[r for r in rows if r[:2]==['agent','start']]
    assert len(launches)==2
    selector='--resume' if p['provider']=='claude' else 'resume'
    assert launches[-1][launches[-1].index(selector)+1]==p['session']
    assert mutate_count(p['backend'],['workspace','close'])==1
    assert p['open']().returncode==0 and mutate_count(p['backend'],['agent','start'])==2
    recovery=read(p['state']/'herdr-recovery.json')
    assert recovery['kind']=='retired_namespace' and recovery['phase']=='complete'

def test_reopen_does_not_replace_missing_history_with_a_fresh_session(stopped):
    p=stopped;retire(p);p['log'].unlink()
    result=p['open'](ms=0);assert result.returncode==0,result.stderr
    wait_for(lambda:cli('--state-root',p['runtime'],'ping').returncode==0)
    actor=json.loads(cli('--state-root',p['runtime'],'agents').stdout)['agents'][0]
    assert actor['activation']['reason']=='recovery_evidence_required'
    assert not p['log'].exists()
    assert mutate_count(p['backend'],['agent','start'])==1

def test_reappeared_retired_workspace_is_not_closed_or_replaced(stopped):
    p=stopped;old=retire(p)
    change(p['backend'],lambda s:s['workspaces'].update({old['workspace_id']:{'workspace_id':old['workspace_id'],'label':'foreign','tokens':{}}}))
    result=p['open']();assert result.returncode!=0
    assert server(p['backend'])['workspaces'][old['workspace_id']]['label']=='foreign'
    assert mutate_count(p['backend'],['workspace','close'])==1
    assert mutate_count(p['backend'],['agent','start'])==1

def test_reopen_rejects_same_session_active_elsewhere(stopped):
    p=stopped;retire(p)
    change(p['backend'],lambda s:s['agents'].update({'foreign':{'agent':'codex','workspace_id':'external','agent_session':{'kind':'id','value':p['session']}}}))
    result=p['open']();assert result.returncode!=0 and 'already active' in result.stderr
    assert mutate_count(p['backend'],['agent','start'])==1

@pytest.mark.parametrize('stopped',['codex','claude'],indirect=True)
def test_recovery_rejects_a_provider_that_resumed_the_wrong_native_session(stopped):
    p=stopped;retire(p);replacement=p['helper']();foreign=str(uuid.uuid4())
    log=p['log'].parent/(f'{foreign}.jsonl' if p['provider']=='claude' else f'rollout-{foreign}.jsonl')
    header={'type':'user','uuid':'foreign-user','sessionId':foreign,'cwd':str(p['project']),'message':{'role':'user','content':'foreign'}} if p['provider']=='claude' else {'type':'session_meta','payload':{'id':foreign,'cwd':str(p['project'])}}
    log.write_text(json.dumps(header)+'\n',encoding='utf-8')
    change(p['backend'],lambda s:s.update(provider_pids=[replacement.pid],agent_sessions={p['provider']:{'agent':p['provider'],'kind':'id','value':foreign,'source':'fixture'}}))
    result=p['open'](ms=0);assert result.returncode==0,result.stderr
    error=wait_for(lambda:(read(p['state']/'herdr-open.json').get('binding_errors',{}) or {}).get('worker'),timeout=20)
    assert 'recovered native session' in error
    actor=json.loads(cli('--state-root',p['runtime'],'agents').stdout)['agents'][0]
    assert actor['activation']['session_verified'] is False
    assert cli('--state-root',p['runtime'],'ping').returncode==0
    assert read(p['state']/'herdr-recovery.json')['phase']=='reset'


def test_recovery_gates_dispatch_but_not_daemon_start_on_session_confirmation(stopped):
    p=stopped;retire(p);replacement=p['helper']()
    change(p['backend'],lambda s:s.update(provider_pids=[replacement.pid],agent_sessions={}))
    assert p['open'](ms=0).returncode==0
    wait_for(lambda:read(p['state']/'herdr-open.json')['phase']=='binding_sessions')
    assert cli('--state-root',p['runtime'],'ping').returncode==0
    assert json.loads(cli('--state-root',p['runtime'],'agents').stdout)['agents'][0]['activation']['session_verified'] is False
    assert mutate_count(p['backend'],['agent','prompt'])==0
    assert read(p['state']/'herdr-recovery.json')['phase']=='reset'
    def publish(s):
        for a in s['agents'].values():
            if a['agent']=='codex':a['agent_session']={'agent':'codex','kind':'id','value':p['session'],'source':'fixture'}
    change(p['backend'],publish)
    result=p['open']();assert result.returncode==0,result.stderr
    wait_for(lambda:read(p['state']/'herdr-open.json')['phase']=='running')
    assert mutate_count(p['backend'],['agent','start'])==2

def test_reopen_observes_started_agent_after_opener_crash_without_duplicate(stopped):
    p=stopped;retire(p);replacement=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[replacement.pid]))
    fault(p['backend'],['agent','start'],'hang-after')
    assert p['open'](ms=0).returncode==0
    wait_for(lambda:(p['backend']/'mutation-finished').exists())
    intent=read(p['state']/'herdr-open-start.json');owned_stop(intent['process'])
    wait_for(lambda:cli('process-identity',intent['process']['pid']).returncode!=0)
    result=p['open']();assert result.returncode==0,result.stderr
    assert mutate_count(p['backend'],['agent','start'])==2
    assert mutate_count(p['backend'],['workspace','close'])==1
    assert len((p['state']/'herdr-recoveries.jsonl').read_text().splitlines())==1

def test_reopen_waits_for_retired_foreground_exit_without_launching_replacement(stopped):
    p=stopped
    result=cli('--state-root',p['runtime'],'kill',timeout=25);assert result.returncode==0,result.stderr
    assert p['open'](ms=0).returncode==0
    wait_for(lambda:read(p['state']/'herdr-open.json')['phase']=='waiting_for_retired_processes')
    assert mutate_count(p['backend'],['agent','start'])==1
    assert mutate_count(p['backend'],['workspace','create'])==1
    replacement=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[replacement.pid]))
    p['first'].kill();p['first'].wait(timeout=5)
    result=p['open']();assert result.returncode==0,result.stderr
    assert mutate_count(p['backend'],['agent','start'])==2

def test_retired_namespace_reopens_after_its_owned_server_also_stops(stopped):
    p=stopped;old_server=read(p['state']/'herdr-open.json')['server_process'];retire(p)
    owned_stop(old_server);wait_for(lambda:cli('process-identity',old_server['pid']).returncode!=0)
    replacement=p['helper']();change(p['backend'],lambda s:s.update(server_running=False,provider_pids=[replacement.pid]))
    result=p['open']();assert result.returncode==0,result.stderr
    new_server=read(p['state']/'herdr-open.json')['server_process']
    assert new_server['pid']!=old_server['pid']
    assert read(p['state']/'herdr-recovery.json')['sessions']['worker']['session_id']==p['session']
    assert mutate_count(p['backend'],['workspace','close'])==1

@pytest.mark.parametrize('native_host,fresh_pi,pending_work',[(False,False,False),(True,False,False),(False,True,False),(True,False,True),(True,True,False)],ids=['terminal','hosted','foreign_pi','hosted_pending','hosted_foreign_pi'])
def test_pi_and_codex_reopen_preserves_sessions_and_callback_routing(project,native_host,fresh_pi,pending_work,record_property):
    from test_native_codex_tui_host import CODEX
    from test_native_pi_terminal import HOST
    p=project
    if native_host:
        if not CODEX.exists():pytest.skip('Installed Codex required for no-model host resumption')
        home=p['root']/'private-codex-home';home.mkdir()
        p['options']['config']['agents']['worker'].update(command=[str(CODEX),'app-server','--stdio'],driver_kind='codex_app_server',
            profile_family='protocol_turn',tui_host_root=str(p['root']/'hosts'),env={'CODEX_HOME':str(home)})
        p['file'].write_text(json.dumps(p['options']),encoding='utf-8')
        change(p['backend'],lambda s:s.update(shell_name='pwsh.exe'))
    assert p['open']().returncode==0
    wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=60)
    original=json.loads(p['pi'].args[2]);history=Path(original['native_session_path'])
    expected_pi=original['session_id']
    history.write_text(json.dumps({'type':'session','id':original['session_id'],'cwd':str(p['root'])})+'\n',encoding='utf-8')
    history_before=history.read_bytes()
    worker_before=read(p['native']/'providers/codex/worker/session.json') if native_host else None
    if pending_work:
        first=cli('--state-root',p['native'],'ask','lead','old active task');assert first.returncode==0,first.stderr
        wait_for(lambda:p['prompts']())
        queued=cli('--state-root',p['native'],'ask','lead','preserve queued task');assert queued.returncode==0,queued.stderr
        queued_id=json.loads(queued.stdout)['job_id']
    result=cli('--state-root',p['native'],'kill',timeout=25)
    assert result.returncode==0 and json.loads(result.stdout)['status']=='closed',result.stderr
    for proc in [p['pi'],p['codex']]:proc.kill();proc.wait(timeout=5)
    gate=p['root']/'new-epoch-ready';original['start_gate']=str(gate)
    if fresh_pi:
        original['session_id']=str(uuid.uuid4());original['native_session_path']=str(p['root']/'wrong-pi-session.jsonl')
        Path(original['native_session_path']).write_text(json.dumps({'type':'session','id':original['session_id'],'cwd':str(p['root'])})+'\n',encoding='utf-8')
        change(p['backend'],lambda s:s['agent_sessions']['pi'].update(value=original['native_session_path']))
    pi=subprocess.Popen([NODE,str(HOST),json.dumps(original)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8',creationflags=0x08000000)
    codex=subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(180)'],creationflags=0x08000000)
    def command(**data):
        pi.stdin.write(json.dumps(data)+'\n');pi.stdin.flush()
        line=pi.stdout.readline();assert line,pi.stderr.read();return json.loads(line)
    try:
        change(p['backend'],lambda s:s.update(provider_pids=[pi.pid,codex.pid]))
        assert p['open'](0).returncode==0
        wait_for(lambda:(p['native']/'herdr/herdr-recovery.json').exists() and read(p['native']/'herdr/herdr-recovery.json')['phase']=='reset',timeout=20)
        if native_host:
            # Hold Pi's new bridge generation back. A quiet recovery must not
            # serialize Codex initialization behind this unrelated provider.
            def early_host():
                path=p['native']/'providers/codex/worker/session.json'
                value=read(path) if path.exists() else {}
                return value if value.get('runtime_instance_id')!=worker_before['runtime_instance_id'] and value.get('state',{}).get('thread_id') else None
            started=time.monotonic()
            early=wait_for(early_host,timeout=15)
            record_property('codex_ready_while_pi_blocked_ms',round((time.monotonic()-started)*1000,2))
            assert early['state']['thread_id']==worker_before['state']['thread_id']
            actors=json.loads(cli('--state-root',p['native'],'agents').stdout)['agents']
            assert {a['agent_name'] for a in actors}=={'lead','worker'}
            lead=next(a for a in actors if a['agent_name']=='lead')
            assert lead['activation']['session_verified'] is False
            # Existing unsent work remains durable while only the target's
            # session verification prevents its execution.
            if pending_work:
                assert not any('preserve queued task' in r['body'] for r in p['prompts']())
                assert json.loads(cli('--state-root',p['native'],'watch',queued_id).stdout)['status'] in ['queued','accepted','pending']
        gate.touch();assert json.loads(pi.stdout.readline())['ready']
        if fresh_pi:
            error=wait_for(lambda:(read(p['native']/'herdr/herdr-open.json').get('binding_errors',{}) or {}).get('lead'),timeout=20)
            assert 'recovered native session' in error
            actors=json.loads(cli('--state-root',p['native'],'agents').stdout)['agents']
            lead=next(a for a in actors if a['agent_name']=='lead')
            assert lead['activation']['session_verified'] is False
            assert not p['prompts']()
            return
        # Include real no-model Codex host initialization under the product's
        # unchanged 60-second startup budget, rather than a 20-second fixture cap.
        wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=60)
        recovery=read(p['native']/'herdr/herdr-recovery.json')
        assert recovery['sessions']['lead']['session_id']==expected_pi
        assert history.read_bytes()==history_before
        assert read(p['native']/'herdr/herdr-agents.json')['agents']['lead']['binding']['process']['pid']==pi.pid
        if native_host:
            after=read(p['native']/'providers/codex/worker/session.json')
            assert after['state']['thread_id']==worker_before['state']['thread_id'] and after['pid']!=worker_before['pid']
            if not pending_work:
                assert after['runtime_instance_id']==early['runtime_instance_id']
            else:
                wait_for(lambda:any('preserve queued task' in row['body'] for row in p['prompts']()))
                assert sum('preserve queued task' in row['body'] for row in p['prompts']())==1
                assert json.loads(cli('--state-root',p['native'],'watch',queued_id).stdout)['status'] not in ['failed','cancelled']
        else:
            asked=cli('--state-root',p['native'],'ask','lead','delegate after reopen');assert asked.returncode==0,asked.stderr
            job=json.loads(asked.stdout)['job_id'];lead=wait_for(lambda:p['prompts']())[-1]
            command(action='prompt',prompt=lead['body'],delegate=True);p['idle']('lead')
            worker=wait_for(lambda:p['prompts']() if len(p['prompts']())>=2 else None)[1]
            with p['log'].open('a',encoding='utf-8') as out:
                for payload in [dict(type='task_started',turn_id='reopen-turn'),dict(type='user_message',message=worker['body']),dict(type='task_complete',turn_id='reopen-turn',last_agent_message='worker after reopen')]:
                    out.write(json.dumps(dict(type='event_msg',payload=payload))+'\n')
            p['idle']('worker');continuation=wait_for(lambda:p['prompts']() if len(p['prompts']())>=3 else None)[2]
            command(action='prompt',prompt=continuation['body'],reply='review after reopen');p['idle']('lead')
            final=cli('--state-root',p['native'],'watch',job,'--wait','--timeout-ms','5000')
            assert final.returncode==0 and json.loads(final.stdout)['reply']=='review after reopen'
        assert mutate_count(p['backend'],['workspace','close'])==1
    finally:
        for proc in [pi,codex]:
            if proc.poll() is None:proc.kill()
            proc.wait(timeout=5)


@pytest.mark.parametrize('activity',[False,True],ids=['unused','activity_without_history'])
def test_unwritten_pi_session_reopens_only_with_positive_empty_evidence(project,activity):
    from test_native_pi_terminal import HOST
    p=project;assert p['open']().returncode==0
    wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=60)
    original=json.loads(p['pi'].args[2]);assert not Path(original['native_session_path']).exists()
    if activity:p['command'](action='event',event='agent_start')
    result=cli('--state-root',p['native'],'kill',timeout=25)
    assert result.returncode==0 and json.loads(result.stdout)['status']=='closed',result.stderr
    for proc in [p['pi'],p['codex']]:proc.kill();proc.wait(timeout=5)
    if activity:
        assert p['open'](0).returncode==0
        error=wait_for(lambda:(read(p['native']/'herdr/herdr-open.json').get('initialization_errors',{}) or {}).get('lead'),timeout=20)
        assert 'Pi recovery history is unavailable' in error
        assert not Path(original['native_session_path']).exists()
        return
    gate=p['root']/'empty-reopen-ready';original.update(start_gate=str(gate),session_id='new-unused-pi',native_session_path=str(p['root']/'new-unused.jsonl'))
    change(p['backend'],lambda s:s['agent_sessions']['pi'].update(value=original['native_session_path']))
    pi=subprocess.Popen([NODE,str(HOST),json.dumps(original)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8',creationflags=0x08000000)
    codex=subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(180)'],creationflags=0x08000000)
    try:
        change(p['backend'],lambda s:s.update(provider_pids=[pi.pid,codex.pid]))
        assert p['open'](0).returncode==0
        wait_for(lambda:(p['native']/'herdr/herdr-recovery.json').exists() and read(p['native']/'herdr/herdr-recovery.json')['phase']=='reset',timeout=30)
        gate.touch();assert json.loads(pi.stdout.readline())['ready']
        wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=60)
        saved=read(p['native']/'herdr/herdr-recovery.json')['sessions']['lead']
        assert saved['fresh'] and saved['reason']=='unused_native_session_never_persisted'
        assert not Path(original['native_session_path']).exists(),'Recovery must not fabricate a native transcript'
    finally:
        for proc in [pi,codex]:
            if proc.poll() is None:proc.kill()
            proc.wait(timeout=5)
