"""The public native open command against a persistent offline Herdr fixture."""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time
import uuid
import pytest
from test_native_herdr_namespace import setup, server, mutate_count, fault
from test_native_herdr_agents import change
from test_native_cli_security import BIN
from test_native_herdr_worker import wait_for
from test_native_pi_terminal import HOST

NODE=shutil.which('node')
pytestmark=pytest.mark.skipif(os.name!='nt' or not NODE,reason='Windows original-terminal workflow')

def cli(*args,timeout=20,**kwargs):
    return subprocess.run([str(BIN),*map(str,args)],capture_output=True,text=True,encoding='utf-8',timeout=timeout,**kwargs)

def read(path):
    for attempt in range(11):
        try:return json.loads(path.read_text(encoding='utf-8'))
        except (PermissionError,FileNotFoundError):
            if attempt==10:raise
            time.sleep(.02)
def owned_stop(identity):
    if not identity:return
    observed=cli('process-identity',identity['pid'])
    if observed.returncode==0 and json.loads(observed.stdout)==identity:os.kill(identity['pid'],signal.SIGTERM)

@pytest.fixture
def project(setup):
    root,backend,request=setup
    (root/'.ccb').mkdir();(root/'.ccb/ccb.config').write_text('lead:pi; worker:codex',encoding='utf-8')
    native_root=root/'.ccb/ccbd'
    generated=subprocess.run([str(BIN),'pi-terminal-bridge'],input=json.dumps(dict(schema_version=1,project_root=str(root),agent_name='lead')),capture_output=True,text=True,encoding='utf-8',timeout=10)
    assert generated.returncode==0,generated.stderr
    bridge=json.loads(generated.stdout)['bridge'];native_session=root/'pi-native.jsonl'
    config=dict(extension=bridge['extension'],project=str(root),session_id='open-pi-session',native_session_path=str(native_session),native=str(BIN),state_root=str(native_root))
    pi=subprocess.Popen([NODE,str(HOST),json.dumps(config)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8',creationflags=0x08000000)
    assert json.loads(pi.stdout.readline())['ready']
    codex=subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(180)'],creationflags=0x08000000)
    session=str(uuid.uuid4());history=root/'codex-history';history.mkdir();log=history/f'rollout-{session}.jsonl'
    log.write_text(json.dumps({'type':'session_meta','payload':{'id':session,'cwd':str(root)}})+'\n',encoding='utf-8')
    state=server(backend);state.update(server_running=False,shell_pid=os.getpid(),provider_pids=[pi.pid,codex.pid],
        agent_sessions={'pi':{'agent':'pi','kind':'path','value':str(native_session),'source':'fixture'},'codex':{'agent':'codex','kind':'id','value':session,'source':'fixture'}})
    (backend/'server.json').write_text(json.dumps(state),encoding='utf-8')
    options=dict(config=request['config'],command=request['command'],session=request['session'],history_roots={'worker':str(history)},transport_timeout_ms=request['timeout_ms'])
    file=root/'open-options.json';file.write_text(json.dumps(options),encoding='utf-8')
    def open(timeout=10000):return cli('--project',root,'herdr','open','--options',file,'--no-attach','--timeout-ms',timeout)
    def ready():
        result=cli('--state-root',native_root,'ping')
        return json.loads(result.stdout) if result.returncode==0 else None
    def prompts():
        path=backend/'prompts.jsonl'
        return [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines()] if path.exists() else []
    def command(**data):
        pi.stdin.write(json.dumps(data)+'\n');pi.stdin.flush();line=pi.stdout.readline();assert line,pi.stderr.read();return json.loads(line)
    def idle(name):
        ledger=read(native_root/'herdr/herdr-agents.json')
        change(backend,lambda s:s['agents'][ledger['agents'][name]['herdr_name']].update(agent_status='idle',interactive_ready=True))
    yield dict(root=root,backend=backend,options=options,file=file,native=native_root,open=open,ready=ready,pi=pi,codex=codex,session=session,log=log,prompts=prompts,command=command,idle=idle)
    start=native_root/'herdr/herdr-open-start.json'
    if start.exists():owned_stop(json.loads(start.read_text(encoding='utf-8')).get('process'))
    cli('--state-root',native_root,'stop')
    journal=native_root/'herdr/herdr-open.json'
    if journal.exists():owned_stop(json.loads(journal.read_text(encoding='utf-8')).get('server_process'))
    for process in [pi,codex]:
        if process.poll() is None:process.kill()
        process.wait(timeout=5)

def test_public_open_supplies_pi_coordination_before_interactive_input(project):
    p=project;assert p['open']().returncode==0
    wait_for(p['ready'],timeout=30)
    p['command'](action='reload')
    context=p['command'](action='context')['systemPrompt']
    assert context.startswith('USER_SYSTEM_SENTINEL')
    assert '<ccb_interactive_coordination>' in context
    assert 'pi-subagents' in context and 'worker' in context
    assert '--from' in context and 'lead' in context
    assert 'plain ask' in context


def test_cold_open_composes_server_agents_binding_daemon_and_same_pi_lead_review(project):
    p=project;opened=p['open']();assert opened.returncode==0,opened.stderr
    wait_for(p['ready'],timeout=20)
    root=p['native'];first=p['ready']()
    server_identity=read(root/'herdr/herdr-open.json')['server_process']
    assert json.loads(cli('process-identity',server_identity['pid']).stdout)==server_identity
    assert mutate_count(p['backend'],['agent','start'])==2
    asked=cli('--state-root',root,'ask','lead','initial task');assert asked.returncode==0,asked.stderr
    receipt=json.loads(asked.stdout)
    lead=wait_for(lambda:p['prompts']())[-1];p['command'](action='prompt',prompt=lead['body'],delegate=True);p['idle']('lead')
    worker=wait_for(lambda:p['prompts']() if len(p['prompts']())>=2 else None)[1]
    extension=root/'herdr/providers/pi/lead/terminal-bridge.mjs';before_write=extension.stat().st_mtime_ns
    repeated=p['open']();assert repeated.returncode==0,repeated.stderr
    assert extension.stat().st_mtime_ns==before_write,'Reconnecting must not rewrite a live extension'
    assert p['ready']()['pid']==first['pid'] and mutate_count(p['backend'],['agent','start'])==2
    with p['log'].open('a',encoding='utf-8') as out:
        for payload in [dict(type='task_started',turn_id='task1'),dict(type='user_message',message=worker['body']),dict(type='task_complete',turn_id='task1',last_agent_message='native open worker result')]:
            out.write(json.dumps(dict(type='event_msg',payload=payload))+'\n')
    p['idle']('worker');continuation=wait_for(lambda:p['prompts']() if len(p['prompts']())>=3 else None)[2]
    p['command'](action='prompt',prompt=continuation['body'],reply='reviewed through native open');p['idle']('lead')
    result=cli('--state-root',root,'watch',receipt['job_id'],'--wait','--timeout-ms','5000');assert result.returncode==0,result.stderr
    assert json.loads(result.stdout)['reply']=='reviewed through native open'
    assert p['pi'].poll() is None and p['codex'].poll() is None

@pytest.mark.parametrize('lost_ack',[False,True])
def test_open_mounts_pi_before_native_codex_host_then_attaches_without_prompt_injection(project,lost_ack):
    from test_native_codex_tui_host import CODEX
    if not CODEX.exists():pytest.skip('Installed Codex required for no-model host initialization')
    p=project;home=p['root']/'empty-codex-home';home.mkdir()
    worker=p['options']['config']['agents']['worker']
    worker.update(command=[str(CODEX),'app-server','--stdio'],driver_kind='codex_app_server',profile_family='protocol_turn',
                  tui_host_root=str(p['root']/'native-hosts'),thread_options={},env={'CODEX_HOME':str(home)})
    p['file'].write_text(json.dumps(p['options']),encoding='utf-8')
    change(p['backend'],lambda s:s.update(shell_name='pwsh.exe'))
    if lost_ack:fault(p['backend'],['pane','run'],'after-error')
    result=p['open']();assert result.returncode==0,result.stderr
    wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=20)
    assert mutate_count(p['backend'],['agent','start'])==1
    assert mutate_count(p['backend'],['pane','run'])==1

    assert mutate_count(p['backend'],['agent','rename'])==1
    assert not p['prompts']()
    session=read(p['native']/'providers/codex/worker/session.json')
    assert session['state']['tui_host']['input_transport']=='authenticated_websocket'
    assert Path(session['state']['path']).is_file(), 'Empty native thread must be resumable without a model warm-up'
    assert p['open']().returncode==0
    assert mutate_count(p['backend'],['pane','run'])==1

def test_host_starts_while_pi_waits_and_later_binding_keeps_same_host(project):
    from test_native_codex_tui_host import CODEX
    if not CODEX.exists():pytest.skip('Installed Codex required for no-model host initialization')
    p=project;home=p['root']/'empty-codex-home';home.mkdir()
    worker=p['options']['config']['agents']['worker']
    worker.update(command=[str(CODEX),'app-server','--stdio'],driver_kind='codex_app_server',profile_family='protocol_turn',
                  tui_host_root=str(p['root']/'native-hosts'),thread_options={},env={'CODEX_HOME':str(home)})
    p['options']['startup_timeout_ms']=120000
    p['file'].write_text(json.dumps(p['options']),encoding='utf-8')
    change(p['backend'],lambda s:s.update(shell_name='pwsh.exe',start_status='blocked'))
    result=p['open'](200);assert result.returncode==0,result.stderr
    wait_for(lambda:mutate_count(p['backend'],['pane','run'])==1,timeout=35)
    wait_for(lambda:(lambda rows:rows.get('worker',{}).get('status')=='ready' and rows.get('lead',{}).get('status')=='blocked')(read(p['native']/'herdr/herdr-open.json').get('agents',{})),timeout=15)
    status=json.loads(cli('--state-root',p['native'],'ping','worker').stdout)
    assert status['ok'] and status['diagnostics']['driver_kind']=='codex_app_server'
    opened=read(p['native']/'herdr/herdr-open.json')
    assert 'lead' not in opened['bindings']
    assert opened['agents']['lead']['status']=='blocked'
    initial=read(p['native']/'providers/codex/worker/session.json')
    before=read(p['native']/'lease.json')['process']
    p['idle']('lead')
    wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=45)
    after=read(p['native']/'providers/codex/worker/session.json')
    assert after['runtime_instance_id']==initial['runtime_instance_id']
    assert read(p['native']/'lease.json')['process']==before
    assert mutate_count(p['backend'],['pane','run'])==1 and not p['prompts']()


def test_kill_cancels_an_active_initial_open(project):
    p=project
    (p['native']/'pi-terminal-events/lead/current.json').unlink()
    assert p['open'](0).returncode==0
    wait_for(lambda:(p['native']/'herdr/namespace.json').exists(),timeout=10)
    assert (p['native']/'herdr/open-configuration.json').exists()
    result=cli('--project',p['root'],'kill','--timeout-ms','10000')
    assert result.returncode==0,result.stderr
    assert json.loads(result.stdout)['status']=='closed'
    intent=read(p['native']/'herdr/herdr-open-start.json')
    assert intent['cancel_requested'] and intent['result']['status']=='cancelled'
    assert mutate_count(p['backend'],['workspace','close'])==1
    assert 'external' in server(p['backend'])['workspaces']


@pytest.mark.parametrize('root_value,ephemeral',[('relative-host',False),(None,True)])
def test_invalid_native_host_open_is_rejected_before_mutation(setup,root_value,ephemeral):
    root,backend,request=setup;(root/'.ccb').mkdir();(root/'.ccb/ccb.config').write_text('lead:pi; worker:codex',encoding='utf-8')
    worker=request['config']['agents']['worker']
    worker.update(command=[str(BIN),'app-server','--stdio'],driver_kind='codex_app_server',
                  tui_host_root=root_value or str(root/'hosts'),thread_options={'ephemeral':ephemeral})
    options=root/'options.json';options.write_text(json.dumps({'config':request['config'],'command':request['command'],'session':request['session']}),encoding='utf-8')
    result=cli('--project',root,'herdr','open','--options',options,'--no-attach','--timeout-ms','1000')
    assert result.returncode!=0
    assert not (backend/'calls.jsonl').exists()
    assert not (root/'.ccb/ccbd/herdr/namespace.json').exists()

def test_public_open_discovers_programs_and_hosts_codex_without_compiled_options(project):
    from test_native_codex_tui_host import CODEX
    from test_native_real_models import PI
    if not CODEX.exists() or not PI.exists():pytest.skip('Installed providers required for no-model initialization')
    p=project;home=p['root']/'default-codex-home';home.mkdir()
    config='version=2\n[windows]\nmain="lead:pi; worker:codex"\n[agents.worker.env]\nCODEX_HOME='+json.dumps(str(home))+'\n'
    (p['root']/'.ccb/ccb.config').write_text(config,encoding='utf-8')
    p['options'].pop('config')
    p['file'].write_text(json.dumps(p['options']),encoding='utf-8')
    change(p['backend'],lambda s:s.update(shell_name='pwsh.exe'))
    result=p['open']();assert result.returncode==0,result.stderr
    wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=20)
    session=read(p['native']/'providers/codex/worker/session.json')
    assert session['state']['tui_host']['input_transport']=='authenticated_websocket'
    assert Path(session['state']['tui_host']['path']).parent.samefile(p['native']/'herdr/codex-hosts/worker')
    assert mutate_count(p['backend'],['agent','start'])==1
    assert mutate_count(p['backend'],['pane','run'])==1
    assert not p['prompts']()
    assert p['open']().returncode==0
    assert mutate_count(p['backend'],['pane','run'])==1

def test_short_observation_returns_without_restarting_live_opener(project):
    p=project;fault(p['backend'],['agent','start'],'hang-after')
    before=time.monotonic();first=p['open'](0);assert first.returncode==0,first.stderr
    assert time.monotonic()-before<1.5
    current=json.loads(first.stdout);second=p['open'](0);assert second.returncode==0,second.stderr
    assert json.loads(second.stdout)['opener']==current['opener']
    wait_for(p['ready'],timeout=20)
    assert mutate_count(p['backend'],['agent','start'])==2

def test_background_budget_is_independent_from_foreground_observation(project):
    p=project
    result=p['open'](0);assert result.returncode==0,result.stderr
    intent=read(p['native']/'herdr/herdr-open-start.json')
    assert intent['startup_timeout_ms']==60000
    wait_for(p['ready'],timeout=20)

@pytest.mark.parametrize('budget',[0,999,600001])
def test_invalid_background_budget_does_not_start_server_or_agents(project,budget):
    p=project
    result=cli('--project',p['root'],'herdr','open','--options',p['file'],
        '--startup-timeout-ms',budget,'--no-attach')
    assert result.returncode!=0 and 'budget' in result.stderr
    assert mutate_count(p['backend'],['agent','start'])==0

def test_partial_close_and_reopen_preserves_history_and_allocates_new_owned_namespace(project):
    p=project;current=p['native']/'pi-terminal-events/lead/current.json'
    change(p['backend'],lambda state:state.update(start_status='blocked'))
    current.unlink();p['options']['startup_timeout_ms']=20000;p['file'].write_text(json.dumps(p['options']))
    opened=p['open']();assert opened.returncode==0,opened.stderr
    assert not (p['native']/'configuration.json').exists()
    opener=read(p['native']/'herdr/herdr-open-start.json')['process']
    wait_for(lambda:cli('process-identity',opener['pid']).returncode!=0,timeout=30)
    old=read(p['native']/'herdr/namespace.json');history=p['log'].read_bytes()
    ledger=read(p['native']/'herdr/herdr-agents.json')
    assert all(row['binding'] for row in ledger['agents'].values()),'precondition: both original provider lifetimes were observed'
    closed=cli('--project',p['root'],'kill');assert closed.returncode==0,closed.stderr
    blocked=p['open']();assert blocked.returncode!=0 and 'still live' in blocked.stderr
    opener=read(p['native']/'herdr/herdr-open-start.json')['process']
    wait_for(lambda:cli('process-identity',opener['pid']).returncode!=0,timeout=30)
    p['pi'].terminate();p['pi'].wait(timeout=5);p['codex'].terminate();p['codex'].wait(timeout=5)
    replacement_pi=subprocess.Popen(p['pi'].args,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
        text=True,encoding='utf-8',creationflags=0x08000000)
    replacement_worker=subprocess.Popen(p['codex'].args,creationflags=0x08000000)
    try:
        assert json.loads(replacement_pi.stdout.readline())['ready']
        change(p['backend'],lambda state:state.update(provider_pids=[replacement_pi.pid,replacement_worker.pid],start_status='idle'))
        p['options']['startup_timeout_ms']=60000;p['file'].write_text(json.dumps(p['options']))
        ready=p['open']();assert ready.returncode==0,ready.stderr
        wait_for(p['ready'],timeout=20)
        new=read(p['native']/'herdr/namespace.json')
        assert new['owner_id']!=old['owner_id'] and new['workspace_id']!=old['workspace_id']
        assert (p['native']/'herdr/archived-partial'/old['owner_id']/'namespace.json').exists()
        assert p['log'].read_bytes()==history
    finally:
        cli('--state-root',p['native'],'stop')
        for process in [replacement_pi,replacement_worker]:
            if process.poll() is None:process.kill()
            process.wait(timeout=5)

def test_hook_authoritative_pi_recovers_its_initial_name_without_relaunch(project):
    p=project;change(p['backend'],lambda s:s.update(pi_hook_authority=True))
    result=p['open']();assert result.returncode==0,result.stderr
    wait_for(p['ready'],timeout=30)
    assert mutate_count(p['backend'],['agent','start'])==2
    assert mutate_count(p['backend'],['agent','rename'])==1
    assert p['open']().returncode==0 and mutate_count(p['backend'],['agent','rename'])==1

def test_initial_pi_name_repair_recovers_lost_ack_without_repeating_rename(project):
    p=project;change(p['backend'],lambda s:s.update(pi_hook_authority=True))
    fault(p['backend'],['agent','rename'],'after-error')
    result=p['open']();assert result.returncode==0,result.stderr;wait_for(p['ready'],timeout=30)
    assert p['open']().returncode==0 and mutate_count(p['backend'],['agent','rename'])==1

def test_initial_pi_name_repair_refuses_a_foreign_extension_owner(project):
    p=project;change(p['backend'],lambda s:s.update(pi_hook_authority=True))
    current=p['native']/'pi-terminal-events/lead/current.json';row=json.loads(current.read_text(encoding='utf-8'))
    row['owner']='00000000-0000-0000-0000-000000000000';current.write_text(json.dumps(row),encoding='utf-8')
    result=p['open']()
    if result.returncode==0:
        wait_for(lambda:read(p['native']/'herdr/herdr-open-start.json')['status']=='failed',timeout=20)
        result=p['open']()
    assert result.returncode!=0
    assert mutate_count(p['backend'],['agent','rename'])==0 and p['pi'].poll() is None

def test_delayed_native_session_binding_continues_in_background(project):
    p=project
    change(p['backend'],lambda s:s['agent_sessions'].pop('codex'))
    result=p['open'](200);assert result.returncode==0,result.stderr
    ledger=p['native']/'herdr/herdr-agents.json'
    wait_for(lambda:ledger.exists() and len(server(p['backend']).get('agents',{}))==2)
    # Fresh Codex can publish its first rollout only after a real input. The
    # daemon admits that input using its mounted process and a unique anchor.
    wait_for(p['ready'],timeout=20)
    worker=json.loads(ledger.read_text(encoding='utf-8'))['agents']['worker']['herdr_name']
    change(p['backend'],lambda s:s['agents'][worker].update(agent_session={'agent':'codex','kind':'id','value':p['session'],'source':'fixture'}))
    wait_for(p['ready'],timeout=20)
    assert mutate_count(p['backend'],['agent','start'])==2

def test_unknown_server_status_does_not_launch_or_replace_anything(project):
    p=project;fault(p['backend'],['status','server'],'before-error')
    result=p['open']();assert result.returncode!=0
    assert not server(p['backend']).get('server_pid') and mutate_count(p['backend'],['agent','start'])==0

def test_incompatible_running_server_is_rejected_before_namespace_changes(project):
    p=project;change(p['backend'],lambda s:s.update(server_running=True,server_compatible=False))
    refused=p['open']();assert refused.returncode!=0 and 'incompatible' in refused.stderr
    assert len(server(p['backend'])['workspaces'])==1 and mutate_count(p['backend'],['agent','start'])==0

def test_pi_session_is_reported_from_verified_extension_without_duplicate_after_lost_ack(project):
    p=project;change(p['backend'],lambda s:s['agent_sessions'].pop('pi'))
    fault(p['backend'],['pane','report-agent-session'],'after-error')
    opened=p['open']();assert opened.returncode==0,opened.stderr;wait_for(p['ready'],timeout=20)
    assert mutate_count(p['backend'],['pane','report-agent-session'])==1
    assert p['open']().returncode==0
    assert mutate_count(p['backend'],['pane','report-agent-session'])==1

def test_uncertain_missing_pi_session_report_is_not_replayed(project):
    p=project;change(p['backend'],lambda s:s['agent_sessions'].pop('pi'))
    fault(p['backend'],['pane','report-agent-session'],'before-error')
    assert p['open'](0).returncode==0
    wait_for(lambda:mutate_count(p['backend'],['pane','report-agent-session'])==1,timeout=30)
    time.sleep(.3);assert p['open'](0).returncode==0
    assert cli('--state-root',p['native'],'ping','lead').returncode!=0
    assert mutate_count(p['backend'],['pane','report-agent-session'])==1

def test_changed_request_does_not_replace_running_agents_or_daemon(project):
    p=project;assert p['open']().returncode==0;before=wait_for(p['ready'],timeout=20)
    p['options']['config']['agents']['worker']['model']='another-model';p['file'].write_text(json.dumps(p['options']),encoding='utf-8')
    refused=p['open']();assert refused.returncode!=0
    assert mutate_count(p['backend'],['agent','start'])==2 and p['ready']()['pid']==before['pid']

def test_reconnect_does_not_repeat_live_binding_queries_or_rewrite_unchanged_ledgers(project):
    from test_native_herdr_namespace import calls
    p=project;assert p['open']().returncode==0
    wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=60)
    before=p['ready']()['pid'];offset=len(calls(p['backend']))
    ledgers=[p['native']/'herdr'/name for name in ['namespace.json','herdr-agents.json']]
    versions=[(path.read_bytes(),path.stat().st_mtime_ns) for path in ledgers]
    result=p['open']();assert result.returncode==0,result.stderr
    wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=60)
    queried=calls(p['backend'])[offset:]
    assert sum(c[:2]==['pane','process-info'] for c in queried)==2,queried
    assert sum(c[:2]==['api','snapshot'] for c in queried)==2,queried
    assert sum(c[:2]==['pane','list'] for c in queried)==2,queried
    assert [(path.read_bytes(),path.stat().st_mtime_ns) for path in ledgers]==versions
    assert p['ready']()['pid']==before and mutate_count(p['backend'],['agent','start'])==2
    assert not p['prompts']()


def test_reconnect_rejects_lost_agent_even_when_disk_binding_exists(project):
    p=project;assert p['open']().returncode==0
    wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=60)
    ledger=read(p['native']/'herdr/herdr-agents.json')
    worker=ledger['agents']['worker'];assert worker['binding']
    change(p['backend'],lambda s:s['agents'].pop(worker['herdr_name']))
    result=p['open']()
    if result.returncode==0:
        wait_for(lambda:read(p['native']/'herdr/herdr-open-start.json')['status']=='failed',timeout=30)
        error=read(p['native']/'herdr/herdr-open-start.json')['error']
    else:error=result.stderr
    assert 'no longer present' in error
    assert mutate_count(p['backend'],['agent','start'])==2


def test_reconnect_from_another_powershell_path_preserves_running_environment(project):
    p=project;assert p['open']().returncode==0;before=wait_for(p['ready'],timeout=20)
    ledger=read(p['native']/'herdr/herdr-open.json')
    argv=cli('--project',p['root'],'herdr','open','--options',p['file'],'--no-attach','--timeout-ms','10000',
             env={**os.environ,'PATH':str(p['root']/'another-shell-bin')+os.pathsep+os.environ['PATH']})
    assert argv.returncode==0,argv.stderr
    wait_for(lambda:read(p['native']/'herdr/herdr-open.json')['phase']=='running',timeout=20)
    after=read(p['native']/'herdr/herdr-open.json')
    assert after['request_hash']==ledger['request_hash'] and after['daemon_fingerprint']==ledger['daemon_fingerprint']
    assert p['ready']()['pid']==before['pid'] and mutate_count(p['backend'],['agent','start'])==2

def test_reconnect_does_not_accept_a_changed_explicit_agent_path(project):
    p=project;p['options']['config']['agents']['worker']['env']={'Path':os.environ['PATH']}
    p['file'].write_text(json.dumps(p['options']),encoding='utf-8')
    assert p['open']().returncode==0;before=wait_for(p['ready'],timeout=20)
    p['options']['config']['agents']['worker']['env']['Path']=str(p['root']/'changed-agent-tools')+os.pathsep+os.environ['PATH']
    p['file'].write_text(json.dumps(p['options']),encoding='utf-8')
    refused=p['open']();assert refused.returncode!=0 and 'configuration changed' in refused.stderr
    assert p['ready']()['pid']==before['pid'] and mutate_count(p['backend'],['agent','start'])==2

def test_opener_crash_after_launch_intent_resumes_without_duplicate_agent_start(project):
    p=project;fault(p['backend'],['agent','start'],'hang-after')
    initial=p['open'](0);assert initial.returncode==0,initial.stderr
    intent=read(p['native']/'herdr/herdr-open-start.json')
    wait_for(lambda:(p['backend']/'mutation-finished').exists())
    owned_stop(intent['process'])
    wait_for(lambda:cli('process-identity',intent['process']['pid']).returncode!=0)
    resumed=p['open']();assert resumed.returncode==0,resumed.stderr
    wait_for(p['ready'],timeout=20)
    assert mutate_count(p['backend'],['agent','start'])==2

def test_live_daemon_identity_mismatch_is_rejected_before_namespace_commands(project):
    p=project;assert p['open']().returncode==0;before=wait_for(p['ready'],timeout=20)
    record=p['native']/'herdr/herdr-open.json';value=json.loads(record.read_text(encoding='utf-8'));value['daemon_fingerprint']='foreign';record.write_text(json.dumps(value),encoding='utf-8')
    calls=(p['backend']/'calls.jsonl').read_text(encoding='utf-8')
    refused=p['open']();assert refused.returncode!=0 and 'existing daemon' in refused.stderr
    assert (p['backend']/'calls.jsonl').read_text(encoding='utf-8')==calls and p['ready']()['pid']==before['pid']

def test_explicit_history_and_windows_path_override_are_preserved(project):
    p=project;p['options']['config']['agents']['worker']['env']={'Path':os.environ['PATH']}
    p['options']['bindings']={'worker':dict(root=str(p['log'].parent),path=str(p['log']),session_id=p['session'],cwd=str(p['root']))}
    p['file'].write_text(json.dumps(p['options']),encoding='utf-8')
    result=p['open']();assert result.returncode==0,result.stderr;wait_for(p['ready'],timeout=20)
    ledger=read(p['native']/'herdr/herdr-open.json')
    assert Path(ledger['bindings']['worker']['history']['path'])==p['log']

def test_agent_shell_environment_routes_plain_native_ask_from_outside_project(project):
    p=project;assert p['open']().returncode==0;wait_for(p['ready'],timeout=20)
    namespace=read(p['native']/'herdr/namespace.json')
    env=server(p['backend'])['panes'][namespace['bindings']['lead']['pane_id']]['env']
    assert env['CCB_AGENT_NAME']=='lead' and Path(env['CCB_NATIVE_STATE_ROOT'])==p['native']
    called=cli('ask','worker','automatic sender and root',env={**os.environ,**env},cwd=p['backend'])
    assert called.returncode==0,called.stderr
    jobs=(p['root']/'.ccb/agents/worker/jobs.jsonl').read_text(encoding='utf-8').splitlines()
    assert json.loads(jobs[-1])['request']['from_actor']=='lead'

def test_dry_run_is_read_only_and_omits_environment_values(setup):
    root,backend,request=setup;(root/'.ccb').mkdir();(root/'.ccb/ccb.config').write_text('worker:codex',encoding='utf-8')
    request['config']['agents']['worker']['env']={'PRIVATE_TEST':'do-not-emit'}
    options=root/'options.json';options.write_text(json.dumps(dict(config=request['config'],command=request['command'],session=request['session'])),encoding='utf-8')
    result=cli('--project',root,'herdr','open','--options',options,'--dry-run','--no-attach')
    assert result.returncode==0,result.stderr
    assert json.loads(result.stdout)['mutations'] is False and 'do-not-emit' not in result.stdout
    assert not (root/'.ccb/ccbd').exists() and not (backend/'calls.jsonl').exists()

@pytest.mark.parametrize('data',[True,[],{'dry_run':'true'},{'session':123},{'transport_timeout_ms':'100'}])
def test_invalid_options_are_rejected_without_starting_background_work(setup,data):
    root,backend,_=setup;(root/'.ccb').mkdir();(root/'.ccb/ccb.config').write_text('worker:codex',encoding='utf-8')
    options=root/'options.json';options.write_text(json.dumps(data),encoding='utf-8')
    result=cli('--project',root,'herdr','open','--options',options,'--no-attach')
    assert result.returncode!=0 and not (root/'.ccb/ccbd').exists() and not (backend/'calls.jsonl').exists()
