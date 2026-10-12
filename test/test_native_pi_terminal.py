"""Actual generated Pi extension + native daemon; Herdr/model are offline fixtures."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import pytest
from test_native_herdr_namespace import setup, run as namespace_run, server as state, mutate_count
from test_native_herdr_agents import run as mount, change
from test_native_cli_security import BIN
from test_native_daemon import Server, await_watch
from test_native_herdr_worker import wait_for

NODE=shutil.which('node')
HOST=Path(__file__).parent/'stubs/pi_terminal_host.mjs'
pytestmark=pytest.mark.skipif(os.name!='nt' or not NODE,reason='Windows Herdr identities and Node extension API fixture')

@pytest.fixture
def terminal(setup):
    project,backend,request=setup
    provision=dict(schema_version=1,project_root=str(project),agent_name='lead')
    def install():
        p=subprocess.run([str(BIN),'pi-terminal-bridge'],input=json.dumps(provision),text=True,capture_output=True,encoding='utf-8',timeout=10)
        assert p.returncode==0,p.stderr
        return json.loads(p.stdout)['bridge']
    bridge=install();assert install()['owner']==bridge['owner']
    root=project/'.ccb/ccbd/daemon'
    native_session=project/'native-pi.jsonl'
    config=dict(extension=bridge['extension'],project=str(project),session_id='pi-test-session',native_session_path=str(native_session),native=str(BIN),state_root=str(root))
    pi=subprocess.Popen([NODE,str(HOST),json.dumps(config)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8',creationflags=0x08000000)
    assert json.loads(pi.stdout.readline())['ready']
    worker=subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(180)'],creationflags=0x08000000)
    instances=[]
    try:
        s=state(backend);s['shell_pid']=os.getpid();s['provider_pids']=[pi.pid,worker.pid]
        (backend/'server.json').write_text(json.dumps(s),encoding='utf-8')
        namespace_run(request);request['action']='mount';rows=mount(request)['mount']['agents']
        change(backend,lambda s:s['agents'][rows['lead']['herdr_name']].update(agent_session={'agent':'pi','kind':'path','value':str(native_session),'source':'fixture'}))
        change(backend,lambda s:s['agents'][rows['worker']['herdr_name']].update(agent_session={'agent':'codex','kind':'id','value':'codex-test-session','source':'fixture'}))
        history=project/'codex-history';history.mkdir();codex_log=history/'rollout.jsonl'
        codex_log.write_text(json.dumps({'type':'session_meta','payload':{'id':'codex-test-session','cwd':str(project)}})+'\n',encoding='utf-8')
        agents={
            'lead':dict(provider='pi',driver_kind='herdr_tui',command=None,workspace_path=str(project),timeout_s=20,pi_bridge_owner=bridge['owner'],
                herdr_history=dict(root=bridge['event_root'],path=str(Path(bridge['event_root'])/'current.json'),cwd=str(project),session_id='pi-test-session')),
            'worker':dict(provider='codex',driver_kind='herdr_tui',command=None,workspace_path=str(project),timeout_s=20,
                herdr_history=dict(root=str(history),path=str(codex_log),cwd=str(project),session_id='codex-test-session'))}
        def start():
            s=Server(root,agents,config_extra=dict(project_id=request['config']['project_id'],project_root=str(project),herdr=request));instances.append(s);return s
        def submit(s,target='lead'):
            return s.rpc('submit',dict(project_id=request['config']['project_id'],to_agent=target,from_actor='user',body='task'))
        def prompts():
            path=backend/'prompts.jsonl'
            return [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines()] if path.exists() else []
        def command(**data):
            pi.stdin.write(json.dumps(data)+'\n');pi.stdin.flush();line=pi.stdout.readline()
            assert line,pi.stderr.read()
            return json.loads(line)
        def idle(agent='lead'):
            change(backend,lambda s:s['agents'][rows[agent]['herdr_name']].update(agent_status='idle',interactive_ready=True))
        yield dict(project=project,backend=backend,bridge=bridge,pi=pi,agents=agents,start=start,submit=submit,prompts=prompts,command=command,idle=idle,codex_log=codex_log,rows=rows)
    finally:
        for s in instances:
            if not getattr(s,'expected_exit',False):s.close()
        for process in [pi,worker]:
            if process.poll() is None:process.kill()
            process.wait(timeout=5)

def prompt(r,count=1):return wait_for(lambda:r['prompts']() if len(r['prompts']())>=count else None)[count-1]
def bridge_state(r):return json.loads((Path(r['bridge']['event_root'])/'current.json').read_text(encoding='utf-8'))

def test_original_pi_lead_dispatches_native_ask_yields_and_receives_worker_continuation(terminal):
    r=terminal;s=r['start']();receipt=r['submit'](s);initial=bridge_state(r)
    r['command'](action='prompt',prompt=prompt(r)['body'],delegate=True);r['idle']()
    worker=prompt(r,2)
    assert worker['name']==r['rows']['worker']['herdr_name']
    wait_for(lambda:s.rpc('records.latest',dict(stream='jobs',identity='job_id',agent_name='lead'),version=1)['result'][0]['status']=='completed')
    assert not s.rpc('watch',{'target':receipt['job_id']})['terminal']
    with r['codex_log'].open('a',encoding='utf-8') as out:
        for payload in [dict(type='task_started',turn_id='worker-turn'),dict(type='user_message',message=worker['body']),dict(type='task_complete',turn_id='worker-turn',last_agent_message='worker result')]:
            out.write(json.dumps(dict(type='event_msg',payload=payload))+'\n')
    r['idle']('worker')
    continuation=prompt(r,3);assert 'CCB result-chain continuation.' in continuation['body']
    r['command'](action='prompt',prompt=continuation['body'],reply='Pi terminal reviewed result');r['idle']()
    result=await_watch(s,receipt['job_id'],lambda row:row['visible_reply_source']=='message_bureau_reply')
    assert result['reply']=='Pi terminal reviewed result'
    assert bridge_state(r)['bridge']==initial['bridge'] and r['pi'].poll() is None
    assert mutate_count(r['backend'],['agent','prompt'])==3

def test_agent_end_alone_is_not_completion_and_exact_settlement_finishes(terminal):
    r=terminal;s=r['start']();receipt=r['submit'](s)
    r['command'](action='prompt',prompt=prompt(r)['body'],reply='settled result',hold=True);r['idle']()
    time.sleep(.35);assert not s.rpc('watch',{'target':receipt['job_id']})['terminal']
    r['command'](action='event',event='agent_settled',payload={'aborted':False})
    row=await_watch(s,receipt['job_id'],lambda row:row['terminal'])
    assert row['reply']=='settled result' and row['completion_reason']=='pi_agent_settled'


@pytest.mark.parametrize('raw,canonical', [('\u00a74\u00a7 result','result'), ('raw text','\u00a74\u00a7 literal user text')])
def test_settlement_uses_canonical_branch_after_other_extension_transform(terminal,raw,canonical):
    r=terminal;s=r['start']();receipt=r['submit'](s)
    r['command'](action='prompt',prompt=prompt(r)['body'],reply=raw,canonical_reply=canonical);r['idle']()
    row=await_watch(s,receipt['job_id'],lambda row:row['terminal'])
    assert row['reply']==canonical
    current=bridge_state(r)
    events=[json.loads(line) for line in Path(current['path']).read_text(encoding='utf-8').splitlines()]
    final=next(e['payload'] for e in reversed(events) if e.get('payload',{}).get('type')=='agent_settled')
    assert final['assistant']['canonical_entry_id'].startswith('assistant-')


def test_canonical_branch_from_another_request_cannot_complete_active_job(terminal):
    r=terminal;s=r['start']();receipt=r['submit'](s)
    r['command'](action='prompt',prompt=prompt(r)['body'],reply='wrong result',canonical_prompt='unrelated user input');r['idle']()
    wait_for(lambda:(s.root/'providers/pi/lead/fault.json').exists())
    assert not s.rpc('watch',{'target':receipt['job_id']})['terminal']

@pytest.mark.parametrize('mode',['reload','session','supersede','same-anchor'])
def test_active_generation_or_session_change_holds_instead_of_completing(terminal,mode):
    r=terminal;s=r['start']();receipt=r['submit'](s)
    r['command'](action='prompt',prompt=prompt(r)['body'],hold=True)
    if mode=='reload':r['command'](action='reload')
    elif mode=='session':r['command'](action='switch',session_id='foreign')
    else:r['command'](action='event',event='input',payload={'text':prompt(r)['body'] if mode=='same-anchor' else 'unmanaged user turn'})
    wait_for(lambda:(s.root/'providers/pi/lead/fault.json').exists())
    assert not s.rpc('watch',{'target':receipt['job_id']})['terminal'] and r['pi'].poll() is None

def test_idle_extension_reload_keeps_same_process_and_new_tasks_use_new_generation(terminal):
    r=terminal;s=r['start']();first=r['submit'](s)
    r['command'](action='prompt',prompt=prompt(r)['body']);r['idle']();await_watch(s,first['job_id'],lambda row:row['terminal'])
    before=bridge_state(r);r['command'](action='reload');after=bridge_state(r)
    assert before['bridge']['process']==after['bridge']['process'] and before['bridge']['instance']!=after['bridge']['instance']
    second=r['submit'](s);r['command'](action='prompt',prompt=prompt(r,2)['body'],reply='after reload');r['idle']()
    assert await_watch(s,second['job_id'],lambda row:row['terminal'])['reply']=='after reload'


def test_reload_reloads_implementation_bytes_not_just_cached_factory(terminal):
    r=terminal;before=bridge_state(r)
    assert before['adapter_revision']=='canonical-session-branch-v1'
    path=Path(r['bridge']['implementation']);source=path.read_text(encoding='utf-8')
    path.write_text(source.replace('canonical-session-branch-v1','canonical-session-branch-test-v2'),encoding='utf-8')
    r['command'](action='reload');after=bridge_state(r)
    assert after['adapter_revision']=='canonical-session-branch-test-v2'
    assert after['bridge']['process']==before['bridge']['process']
    assert after['session_id']==before['session_id']
    assert after['bridge']['instance']!=before['bridge']['instance']
    assert Path(before['path']).exists()

def test_daemon_restart_observes_settled_pi_without_resending(terminal):
    r=terminal;s=r['start']();receipt=r['submit'](s);body=prompt(r)['body']
    r['command'](action='prompt',prompt=body,hold=True)
    s.expected_exit=True;s.proc.kill();s.proc.wait(timeout=5)
    r['command'](action='event',event='agent_settled',payload={'aborted':False});r['idle']()
    resumed=r['start']()
    assert await_watch(resumed,receipt['job_id'],lambda row:row['terminal'])['reply']=='Pi terminal result'
    assert len(r['prompts']())==1 and r['pi'].poll() is None

@pytest.mark.parametrize('field',['owner','process','creation','path'])
def test_foreign_current_state_is_rejected_before_prompt_permission(terminal,field):
    r=terminal;current=bridge_state(r)
    if field=='owner':current['bridge']['owner']='00000000-0000-0000-0000-000000000000'
    elif field=='process':current['bridge']['process']['pid']=os.getpid()
    elif field=='creation':current['bridge']['process']['created']='1'
    else:current['path']=str(r['project']/'unrelated.jsonl')
    (Path(r['bridge']['event_root'])/'current.json').write_text(json.dumps(current),encoding='utf-8')
    s=r['start']();receipt=r['submit'](s)
    row=await_watch(s,receipt['job_id'],lambda row:row['terminal'])
    assert row['status']=='failed' and row['completion_reason']=='herdr_preflight_failed'
    assert not r['prompts']() and r['pi'].poll() is None

def test_discontinuous_event_log_holds_execution_and_does_not_use_later_result(terminal):
    r=terminal;s=r['start']();receipt=r['submit'](s)
    r['command'](action='prompt',prompt=prompt(r)['body'],hold=True)
    current=bridge_state(r)
    with Path(current['path']).open('a',encoding='utf-8') as out:
        out.write(json.dumps({'type':'pi_event','bridge':current['bridge'],'seq':50,'payload':{'type':'agent_end','turn_id':'foreign'}})+'\n')
    r['command'](action='event',event='agent_settled',payload={'aborted':False});r['idle']()
    wait_for(lambda:(s.root/'providers/pi/lead/fault.json').exists())
    assert not s.rpc('watch',{'target':receipt['job_id']})['terminal']
    assert r['pi'].poll() is None

def test_unloaded_extension_is_not_reused_from_its_old_current_state(terminal):
    r=terminal;r['command'](action='event',event='session_shutdown',payload={})
    assert bridge_state(r)['ready'] is False
    s=r['start']();receipt=r['submit'](s)
    row=await_watch(s,receipt['job_id'],lambda row:row['terminal'])
    assert row['status']=='failed' and row['completion_reason']=='herdr_preflight_failed'
    assert not r['prompts']() and r['pi'].poll() is None

@pytest.mark.skipif(os.environ.get('CCB_RUN_REAL_PI_BRIDGE')!='1',reason='Explicit installed-Pi no-model probe')
def test_installed_pi_loads_bridge_and_session_switch_rotates_log_without_a_model(tmp_path):
    from test_native_real_models import PI,NODE as REAL_NODE
    from test_native_restart import ready
    from test_native_context import control,settled
    if not PI.exists() or not REAL_NODE.exists():pytest.skip('Installed Pi is unavailable')
    project=tmp_path/'project';project.mkdir();home=tmp_path/'empty-pi-home';home.mkdir()
    result=subprocess.run([str(BIN),'pi-terminal-bridge'],input=json.dumps(dict(schema_version=1,project_root=str(project),agent_name='lead')),
                          text=True,encoding='utf-8',capture_output=True,timeout=10)
    assert result.returncode==0,result.stderr
    bridge=json.loads(result.stdout)['bridge'];state_file=Path(bridge['event_root'])/'current.json'
    spec=dict(provider='pi',driver_kind='pi_rpc',command=[str(REAL_NODE),str(PI),'--mode','rpc','--no-extensions','--no-skills','--no-prompt-templates',
        '--no-themes','--session-dir',str(home/'sessions'),'-e',bridge['extension']],workspace_path=str(project),
        env={'PI_CODING_AGENT_DIR':str(home),'PI_SKIP_VERSION_CHECK':'1','PI_TELEMETRY':'0'})
    instance=Server(tmp_path/'runtime/ccbd',{'worker':spec})
    try:
        actual=ready(instance);wait_for(state_file.exists)
        before=json.loads(state_file.read_text(encoding='utf-8'));assert before['bridge']['process']['pid']==actual['pid']
        assert settled(instance,control(instance,'clear'))['status']=='completed'
        after=json.loads(state_file.read_text(encoding='utf-8'))
        assert before['session_id']!=after['session_id'] and before['bridge']['instance']!=after['bridge']['instance']
        assert before['bridge']['process']==after['bridge']['process'] and Path(before['path']).exists()
        auth=home/'auth.json';assert not auth.exists() or json.loads(auth.read_text())=={}
    finally:instance.close()
