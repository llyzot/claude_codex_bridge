"""Host-backed display recovery: real Codex RPC, fake Herdr, real process deaths."""
import json
import os
from pathlib import Path
import subprocess
import sys
import pytest
from test_native_herdr_open import project, cli, owned_stop
from test_native_herdr_namespace import setup, server, mutate_count, fault
from test_native_herdr_agents import change
from test_native_herdr_worker import wait_for
from test_native_codex_tui_host import CODEX

pytestmark=pytest.mark.skipif(os.name!='nt' or not CODEX.is_file(),reason='Installed Windows Codex no-model probe')

def read(path):return json.loads(path.read_text(encoding='utf-8'))

@pytest.fixture
def hosted(project):
    p=project;home=p['root']/'private-codex-home';home.mkdir();helpers=[]
    p['options']['config']['agents']['worker'].update(command=[str(CODEX),'app-server','--stdio'],
        driver_kind='codex_app_server',profile_family='protocol_turn',tui_host_root=str(p['root']/'hosts'),env={'CODEX_HOME':str(home)})
    p['file'].write_text(json.dumps(p['options']),encoding='utf-8')
    change(p['backend'],lambda s:s.update(shell_name='pwsh.exe'))
    def helper():
        proc=subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(180)'],creationflags=0x08000000)
        helpers.append(proc);return proc
    p['helper']=helper
    try:
        result=p['open']();assert result.returncode==0,result.stderr
        wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=20)
        yield p
    finally:
        for proc in helpers:
            if proc.poll() is None:proc.kill()
            proc.wait(timeout=5)

def remove_display(p,*,kill=True):
    row=read(p['native']/'herdr/herdr-agents.json')['agents']['worker']
    if kill:p['codex'].kill();p['codex'].wait(timeout=5)
    def remove(s):
        s['agents'].pop(row['herdr_name']);pane=s['panes'][row['pane_id']]
        pane['agent']=None;pane['foreground_pid']=s['shell_pid'];pane['argv']=[]
    change(p['backend'],remove)
    return row

@pytest.mark.parametrize('lost_ack',[False,True])
def test_reopen_replaces_only_dead_display_and_keeps_native_thread(hosted,lost_ack):
    p=hosted;session=read(p['native']/'providers/codex/worker/session.json');before=remove_display(p)
    replacement=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[replacement.pid]))
    if lost_ack:fault(p['backend'],['pane','run'],'after-error')
    result=p['open']();assert result.returncode==0,result.stderr
    wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=20)
    after=read(p['native']/'herdr/herdr-agents.json')['agents']['worker']
    assert after['binding']['process']['pid']==replacement.pid and after['operation_id']!=before['operation_id']
    assert read(p['native']/'providers/codex/worker/session.json')['pid']==session['pid']
    assert after['host_launch']['thread_id']==session['state']['thread_id']
    assert mutate_count(p['backend'],['pane','run'])==2
    assert len((p['native']/'herdr/herdr-attachments.jsonl').read_text(encoding='utf-8').splitlines())==1
    assert p['open']().returncode==0 and mutate_count(p['backend'],['pane','run'])==2

def test_foreign_pane_occupant_is_not_replaced(hosted):
    p=hosted;row=remove_display(p);other=p['helper']()
    def foreign(s):
        pane=s['panes'][row['pane_id']];pane.update(agent='pi',foreground_pid=other.pid)
        s['agents']['foreign']={'name':'foreign','agent':'pi','pane_id':row['pane_id'],'workspace_id':pane['workspace_id']}
    change(p['backend'],foreign)
    result=p['open']();assert result.returncode!=0
    assert mutate_count(p['backend'],['pane','run'])==1 and other.poll() is None
    assert not (p['native']/'herdr/herdr-attachments.jsonl').exists()

@pytest.mark.parametrize('recycled_pid',[False,True])
def test_reopen_restarts_stopped_daemon_without_restarting_herdr_or_pi(hosted,recycled_pid):
    p=hosted;before=read(p['native']/'herdr/herdr-agents.json');session=read(p['native']/'providers/codex/worker/session.json')
    daemon=p['ready']()['pid'];server_before=read(p['native']/'herdr/herdr-open.json')['server_process']
    identity=json.loads(cli('process-identity',daemon).stdout)
    cli('--state-root',p['native'],'stop');wait_for(lambda:cli('process-identity',daemon).returncode!=0)
    surrogate=None
    if recycled_pid:
        surrogate=p['helper']();lease_path=p['native']/'lease.json';lease=read(lease_path);lease['ccbd_pid']=surrogate.pid;lease_path.write_text(json.dumps(lease),encoding='utf-8')
        intent_path=p['native']/'kernel/native-start.json';intent=read(intent_path);intent['pid']=surrogate.pid;intent['process']={**identity,'pid':surrogate.pid};intent_path.write_text(json.dumps(intent),encoding='utf-8')
    remove_display(p);new_display=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[new_display.pid]))
    result=p['open']();assert result.returncode==0,result.stderr
    wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=20)
    after=read(p['native']/'herdr/herdr-agents.json');new_session=read(p['native']/'providers/codex/worker/session.json')
    assert after['agents']['lead']['binding']==before['agents']['lead']['binding']
    assert read(p['native']/'herdr/herdr-open.json')['server_process']==server_before
    assert p['ready']()['pid']!=daemon and new_session['pid']!=session['pid']
    if surrogate:assert surrogate.poll() is None and p['ready']()['pid']!=surrogate.pid
    assert new_session['state']['thread_id']==session['state']['thread_id']
    assert mutate_count(p['backend'],['agent','start'])==1 and mutate_count(p['backend'],['pane','run'])==2

def test_background_restart_refuses_missing_native_history(hosted):
    p=hosted;s=read(p['native']/'providers/codex/worker/session.json');daemon=p['ready']()['pid']
    cli('--state-root',p['native'],'stop');wait_for(lambda:cli('process-identity',daemon).returncode!=0)
    remove_display(p);Path(s['state']['path']).unlink()
    result=p['open']();assert result.returncode!=0
    assert not p['ready']() and mutate_count(p['backend'],['pane','run'])==1

def test_background_restart_does_not_replay_unfinished_lead_job(hosted):
    p=hosted;asked=cli('--state-root',p['native'],'ask','lead','hold this request');assert asked.returncode==0,asked.stderr
    wait_for(lambda:len(p['prompts']())==1)
    daemon=p['ready']()['pid'];cli('--state-root',p['native'],'stop');wait_for(lambda:cli('process-identity',daemon).returncode!=0)
    remove_display(p);result=p['open']()
    assert result.returncode!=0 and 'unfinished' in result.stderr
    assert not p['ready']() and len(p['prompts']())==1

def test_background_restart_refuses_changed_live_pi_binding(hosted):
    p=hosted;daemon=p['ready']()['pid'];ledger=read(p['native']/'herdr/herdr-agents.json')
    cli('--state-root',p['native'],'stop');wait_for(lambda:cli('process-identity',daemon).returncode!=0)
    remove_display(p)
    change(p['backend'],lambda s:s['agents'][ledger['agents']['lead']['herdr_name']]['agent_session'].update(value=str(p['root']/'different-pi-history.jsonl')))
    result=p['open']();assert result.returncode!=0 and 'session' in result.stderr
    assert not p['ready']() and p['pi'].poll() is None
    assert mutate_count(p['backend'],['agent','start'])==1

def test_missing_inventory_does_not_relaunch_a_live_display(hosted):
    p=hosted;remove_display(p,kill=False)
    p['open'](500)
    assert p['codex'].poll() is None and mutate_count(p['backend'],['pane','run'])==1
    assert not (p['native']/'herdr/herdr-attachments.jsonl').exists()

def test_working_host_display_does_not_block_open_or_relaunch(hosted):
    p=hosted;row=read(p['native']/'herdr/herdr-agents.json')['agents']['worker']
    def busy(s):
        for agent in s['agents'].values():agent.update(agent_status='working',interactive_ready=False)
    change(p['backend'],busy)
    result=p['open'](1000);assert result.returncode==0,result.stderr
    wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=5)
    assert mutate_count(p['backend'],['pane','run'])==1

@pytest.mark.parametrize('forged_process',[False,True])
def test_clear_rebinds_only_the_supervised_new_display_without_another_launch(hosted,forged_process):
    p=hosted;row=read(p['native']/'herdr/herdr-agents.json')['agents']['worker']
    before=read(p['native']/'providers/codex/worker/session.json')
    result=cli('--state-root',p['native'],'clear','worker','--wait','--timeout-ms','10000');assert result.returncode==0,result.stderr
    after=read(p['native']/'providers/codex/worker/session.json');thread=after['state']['thread_id']
    assert thread!=before['state']['thread_id'] and Path(after['state']['path']).is_file()
    p['codex'].kill();p['codex'].wait(timeout=5);replacement=p['helper']()
    identity=json.loads(cli('process-identity',replacement.pid).stdout)
    supervisor=json.loads(cli('process-identity',os.getpid()).stdout)
    proof=dict(schema_version=1,attachment_id=row['operation_id'],epoch=1,status='running',thread_id=thread,
               host_process=row['host_launch']['host_process'],server_process=row['host_launch']['server_process'],supervisor=supervisor,process=identity)
    if forged_process:proof['process']['created']='1'
    host=Path(after['state']['tui_host']['path']);(host/f"display-{row['operation_id']}.json").write_text(json.dumps(proof),encoding='utf-8')
    def switched(s):
        agent=s['agents'].pop(row['herdr_name']);agent['name']=None;s['agents']['switched']=agent
        s['panes'][row['pane_id']].update(foreground_pid=replacement.pid,argv=[str(CODEX),'--remote',row['host_launch']['endpoint'],'resume',thread])
    change(p['backend'],switched)
    result=p['open']()
    assert mutate_count(p['backend'],['pane','run'])==1
    if forged_process:
        assert result.returncode!=0 and 'supervisor proof' in result.stderr
        assert not (p['native']/'herdr/herdr-attachments.jsonl').exists()
    else:
        assert result.returncode==0,result.stderr
        rebound=read(p['native']/'herdr/herdr-agents.json')['agents']['worker']
        assert rebound['binding']['process']['pid']==replacement.pid and rebound['host_launch']['thread_id']==thread
        assert read(p['native']/'providers/codex/worker/session.json')['pid']==before['pid']
        assert p['open']().returncode==0 and mutate_count(p['backend'],['pane','run'])==1

@pytest.fixture
def stopped_host(setup):
    project,backend,request=setup;(project/'.ccb').mkdir();(project/'.ccb/ccb.config').write_text('worker:codex',encoding='utf-8')
    request['config']['agents'].pop('lead');request['config']['source_config']={'layout':'worker:codex'}
    home=project/'empty-home';home.mkdir();runtime=project/'.ccb/ccbd';state=runtime/'herdr';helpers=[]
    def helper():
        p=subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(180)'],creationflags=0x08000000);helpers.append(p);return p
    first=helper();request['config']['agents']['worker'].update(command=[str(CODEX),'app-server','--stdio'],
        driver_kind='codex_app_server',profile_family='protocol_turn',tui_host_root=str(project/'hosts'),env={'CODEX_HOME':str(home)})
    s=server(backend);s.update(server_running=False,shell_pid=os.getpid(),shell_name='pwsh.exe',provider_pids=[first.pid]);(backend/'server.json').write_text(json.dumps(s),encoding='utf-8')
    options=project/'options.json';options.write_text(json.dumps({'config':request['config'],'command':request['command'],'session':request['session'],'transport_timeout_ms':request['timeout_ms']}),encoding='utf-8')
    def open(recover=False):return cli('--project',project,'herdr','open','--options',options,'--no-attach','--timeout-ms','10000',*(['--recover-stopped'] if recover else []))
    try:
        r=open();assert r.returncode==0,r.stderr
        wait_for(lambda:read(state/'herdr-open.json')['phase']=='running',timeout=20)
        old=read(runtime/'providers/codex/worker/session.json');log=Path(old['state']['path']);assert log.is_file()
        cli('--state-root',runtime,'stop');wait_for(lambda:cli('process-identity',old['pid']).returncode!=0)
        owned_stop(read(state/'herdr-open.json')['server_process']);first.kill();first.wait(timeout=5)
        def stopped(s):
            s.update(server_running=False,restore_without_tokens=True,agents={})
            for pane in s['panes'].values():pane['agent']=None;pane['foreground_pid']=s['shell_pid']
        change(backend,stopped)
        yield dict(project=project,backend=backend,runtime=runtime,state=state,open=open,helper=helper,old=old,log=log)
    finally:
        p=state/'herdr-open-start.json'
        if p.exists():owned_stop(read(p).get('process'))
        cli('--state-root',runtime,'stop')
        p=state/'herdr-open.json'
        if p.exists():owned_stop(read(p).get('server_process'))
        for p in helpers:
            if p.poll() is None:p.kill()
            p.wait(timeout=5)

def test_stopped_native_host_resumes_exact_thread_and_new_owned_process(stopped_host):
    p=stopped_host;process=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[process.pid]))
    before=p['log'].read_bytes();r=p['open'](True);assert r.returncode==0,r.stderr
    wait_for(lambda:read(p['state']/'herdr-open.json')['phase']=='running',timeout=20)
    after=read(p['runtime']/'providers/codex/worker/session.json')
    assert after['state']['thread_id']==p['old']['state']['thread_id'] and after['pid']!=p['old']['pid']
    assert p['log'].read_bytes().startswith(before)
    assert read(p['state']/'herdr-recovery.json')['phase']=='complete'
    assert mutate_count(p['backend'],['pane','run'])==2 and mutate_count(p['backend'],['workspace','close'])==1

def test_missing_host_history_blocks_before_restarting_server(stopped_host):
    p=stopped_host;p['log'].unlink();r=p['open'](True)
    assert r.returncode!=0 and not server(p['backend'])['server_running']
    assert 'not integrated' not in r.stderr
    assert mutate_count(p['backend'],['workspace','close'])==0

def test_foreign_native_host_session_record_blocks_recovery(stopped_host):
    p=stopped_host;path=p['runtime']/'providers/codex/worker/session.json';saved=read(path);saved['pid']=os.getpid();path.write_text(json.dumps(saved),encoding='utf-8')
    r=p['open'](True);assert r.returncode!=0 and 'another host' in r.stderr
    assert not server(p['backend'])['server_running'] and mutate_count(p['backend'],['workspace','close'])==0
