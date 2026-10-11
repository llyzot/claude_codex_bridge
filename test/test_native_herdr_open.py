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
    options=dict(config=request['config'],command=request['command'],session=request['session'],history_roots={'worker':str(history)},transport_timeout_ms=1500)
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
        ledger=json.loads((native_root/'herdr/herdr-agents.json').read_text())
        change(backend,lambda s:s['agents'][ledger['agents'][name]['herdr_name']].update(agent_status='idle',interactive_ready=True))
    yield dict(root=root,backend=backend,options=options,file=file,native=native_root,open=open,ready=ready,pi=pi,codex=codex,session=session,log=log,prompts=prompts,command=command,idle=idle)
    start=native_root/'herdr/herdr-open-start.json'
    if start.exists():owned_stop(json.loads(start.read_text()).get('process'))
    cli('--state-root',native_root,'stop')
    journal=native_root/'herdr/herdr-open.json'
    if journal.exists():owned_stop(json.loads(journal.read_text()).get('server_process'))
    for process in [pi,codex]:
        if process.poll() is None:process.kill()
        process.wait(timeout=5)

def test_cold_open_composes_server_agents_binding_daemon_and_same_pi_lead_review(project):
    p=project;opened=p['open']();assert opened.returncode==0,opened.stderr
    wait_for(p['ready'],timeout=20)
    root=p['native'];first=p['ready']()
    server_identity=json.loads((root/'herdr/herdr-open.json').read_text())['server_process']
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

def test_short_observation_returns_without_restarting_live_opener(project):
    p=project;fault(p['backend'],['agent','start'],'hang-after')
    before=time.monotonic();first=p['open'](0);assert first.returncode==0,first.stderr
    assert time.monotonic()-before<1.5
    current=json.loads(first.stdout);second=p['open'](0);assert second.returncode==0,second.stderr
    assert json.loads(second.stdout)['opener']==current['opener']
    wait_for(p['ready'],timeout=20)
    assert mutate_count(p['backend'],['agent','start'])==2

def test_hook_authoritative_pi_recovers_its_initial_name_without_relaunch(project):
    p=project;change(p['backend'],lambda s:s.update(pi_hook_authority=True))
    result=p['open']();assert result.returncode==0,result.stderr
    wait_for(p['ready'],timeout=8)
    assert mutate_count(p['backend'],['agent','start'])==2
    assert mutate_count(p['backend'],['agent','rename'])==1
    assert p['open']().returncode==0 and mutate_count(p['backend'],['agent','rename'])==1

def test_initial_pi_name_repair_recovers_lost_ack_without_repeating_rename(project):
    p=project;change(p['backend'],lambda s:s.update(pi_hook_authority=True))
    fault(p['backend'],['agent','rename'],'after-error')
    result=p['open']();assert result.returncode==0,result.stderr;wait_for(p['ready'],timeout=8)
    assert p['open']().returncode==0 and mutate_count(p['backend'],['agent','rename'])==1

def test_initial_pi_name_repair_refuses_a_foreign_extension_owner(project):
    p=project;change(p['backend'],lambda s:s.update(pi_hook_authority=True))
    current=p['native']/'pi-terminal-events/lead/current.json';row=json.loads(current.read_text())
    row['owner']='00000000-0000-0000-0000-000000000000';current.write_text(json.dumps(row),encoding='utf-8')
    result=p['open']();assert result.returncode!=0
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
    worker=json.loads(ledger.read_text())['agents']['worker']['herdr_name']
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
    wait_for(lambda:mutate_count(p['backend'],['pane','report-agent-session'])==1)
    time.sleep(.3);assert p['open'](0).returncode==0
    assert p['ready']() is None and mutate_count(p['backend'],['pane','report-agent-session'])==1

def test_changed_request_does_not_replace_running_agents_or_daemon(project):
    p=project;assert p['open']().returncode==0;before=wait_for(p['ready'],timeout=20)
    p['options']['config']['agents']['worker']['model']='another-model';p['file'].write_text(json.dumps(p['options']),encoding='utf-8')
    refused=p['open']();assert refused.returncode!=0
    assert mutate_count(p['backend'],['agent','start'])==2 and p['ready']()['pid']==before['pid']

def test_opener_crash_after_launch_intent_resumes_without_duplicate_agent_start(project):
    p=project;fault(p['backend'],['agent','start'],'hang-after')
    initial=p['open'](0);assert initial.returncode==0,initial.stderr
    intent=json.loads((p['native']/'herdr/herdr-open-start.json').read_text())
    wait_for(lambda:(p['backend']/'mutation-finished').exists())
    owned_stop(intent['process'])
    wait_for(lambda:cli('process-identity',intent['process']['pid']).returncode!=0)
    resumed=p['open']();assert resumed.returncode==0,resumed.stderr
    wait_for(p['ready'],timeout=20)
    assert mutate_count(p['backend'],['agent','start'])==2

def test_live_daemon_identity_mismatch_is_rejected_before_namespace_commands(project):
    p=project;assert p['open']().returncode==0;before=wait_for(p['ready'],timeout=20)
    record=p['native']/'herdr/herdr-open.json';value=json.loads(record.read_text());value['daemon_fingerprint']='foreign';record.write_text(json.dumps(value),encoding='utf-8')
    calls=(p['backend']/'calls.jsonl').read_text()
    refused=p['open']();assert refused.returncode!=0 and 'existing daemon' in refused.stderr
    assert (p['backend']/'calls.jsonl').read_text()==calls and p['ready']()['pid']==before['pid']

def test_explicit_history_and_windows_path_override_are_preserved(project):
    p=project;p['options']['config']['agents']['worker']['env']={'Path':os.environ['PATH']}
    p['options']['bindings']={'worker':dict(root=str(p['log'].parent),path=str(p['log']),session_id=p['session'],cwd=str(p['root']))}
    p['file'].write_text(json.dumps(p['options']),encoding='utf-8')
    result=p['open']();assert result.returncode==0,result.stderr;wait_for(p['ready'],timeout=20)
    ledger=json.loads((p['native']/'herdr/herdr-open.json').read_text())
    assert Path(ledger['bindings']['worker']['history']['path'])==p['log']

def test_agent_shell_environment_routes_plain_native_ask_from_outside_project(project):
    p=project;assert p['open']().returncode==0;wait_for(p['ready'],timeout=20)
    namespace=json.loads((p['native']/'herdr/namespace.json').read_text())
    env=server(p['backend'])['panes'][namespace['bindings']['lead']['pane_id']]['env']
    assert env['CCB_AGENT_NAME']=='lead' and Path(env['CCB_NATIVE_STATE_ROOT'])==p['native']
    called=cli('ask','worker','automatic sender and root',env={**os.environ,**env},cwd=p['backend'])
    assert called.returncode==0,called.stderr
    jobs=(p['root']/'.ccb/agents/worker/jobs.jsonl').read_text().splitlines()
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
