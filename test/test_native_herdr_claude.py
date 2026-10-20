"""Native Claude transcript dispatch through an offline Herdr control plane."""
import json,os,subprocess,sys,time,uuid
from pathlib import Path
import pytest
from test_native_herdr_namespace import setup,server as backend_state,run as namespace_run,mutate_count
from test_native_herdr_agents import run,change
from test_native_herdr_worker import wait_for
from test_native_daemon import Server,await_watch
from test_native_pi_rpc import pi_spec

pytestmark=pytest.mark.skipif(os.name!='nt',reason='Windows Herdr process identity')

@pytest.fixture
def runtime(setup,request):
    finalize=request.addfinalizer
    project,backend,request=setup
    request['config']['agents']['worker']['provider']='claude'
    request['config']['source_config']['layout']='lead:pi; worker:claude'
    processes=[subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(180)'],creationflags=0x08000000) for _ in range(2)]
    def cleanup():
        for p in processes:
            if p.poll() is None:p.kill()
            p.wait(timeout=5)
    finalize(cleanup)
    state=backend_state(backend);state.update(shell_pid=os.getpid(),provider_pids=[p.pid for p in processes])
    (backend/'server.json').write_text(json.dumps(state),encoding='utf-8')
    namespace_run(request);request['action']='mount';mount=run(request)['mount']
    name=mount['agents']['worker']['herdr_name'];session=str(uuid.uuid4());history=project/'claude-history';history.mkdir();path=history/f'{session}.jsonl'
    def row(kind,ident,parent=None,content=None,stop=None):
        return dict(type=kind,uuid=ident,parentUuid=parent,sessionId=session,cwd=str(project),isSidechain=False,
                    message=dict(role=kind,id='msg-'+ident,content=content,stop_reason=stop))
    def append(*rows):
        with path.open('a',encoding='utf-8') as f:
            for record in rows:f.write(json.dumps(record)+'\n')
    append(row('user','old',content='historical input'))
    change(backend,lambda s:s['agents'][name].update(agent_session=dict(agent='claude',kind='id',value=session,source='fixture')))
    worker=dict(provider='claude',driver_kind='herdr_tui',command=None,workspace_path=str(project),timeout_s=25,
                herdr_history=dict(root=str(history),path=str(path),cwd=str(project),session_id=session))
    lead=pi_spec('lead');lead['env']={'CCB_TEST_PROJECT_ID':request['config']['project_id']}
    agents={'lead':lead,'worker':worker};instances=[]
    def start():
        instance=Server(project/'.ccb/ccbd/daemon',agents,config_extra=dict(project_id=request['config']['project_id'],project_root=str(project),herdr=request))
        instances.append(instance);return instance
    def submit(s,target='worker',body='task'):return s.rpc('submit',dict(project_id=request['config']['project_id'],to_agent=target,from_actor='user',body=body))
    def prompts():
        file=backend/'prompts.jsonl';return [json.loads(line) for line in file.read_text(encoding='utf-8').splitlines()] if file.exists() else []
    def idle():change(backend,lambda s:s['agents'][name].update(agent_status='idle',interactive_ready=True))
    def finish(prompt,text='Claude terminal result',settle=True,wrapped=False):
        body=f'<pasted_content id="fixture">\n{prompt["body"]}\n</pasted_content id="fixture">' if wrapped else prompt['body']
        ident=str(uuid.uuid4());append(row('user',ident,content=body),row('assistant',ident+'-reply',ident,[{'type':'text','text':text}],'end_turn'))
        if settle:idle()
    def defer():
        worker.pop('herdr_history');worker['herdr_discovery']=dict(root=str(history),cwd=str(project));path.unlink()
        change(backend,lambda s:s['agents'][name].update(agent_session=None))
    yield dict(project=project,backend=backend,request=request,agents=agents,worker=worker,path=path,name=name,session=session,
               start=start,submit=submit,prompts=prompts,finish=finish,idle=idle,append=append,row=row,defer=defer)
    for s in instances:
        if s.proc.poll() is None:s.close()
    for p in processes:
        if p.poll() is None:p.kill()
        p.wait(timeout=5)

def test_bound_claude_result_waits_for_idle_then_wakes_same_pi_lead(runtime):
    r=runtime;s=r['start']();task=r['submit'](s,'lead');prompt=wait_for(lambda:r['prompts']())[-1]
    before=json.loads((s.root/'providers/pi/lead/session.json').read_text(encoding='utf-8'))
    r['finish'](prompt,settle=False);time.sleep(.4)
    assert s.rpc('watch',{'target':task['job_id']})['visible_reply_source']!='message_bureau_reply'
    r['idle']();result=await_watch(s,task['job_id'],lambda v:v['visible_reply_source']=='message_bureau_reply',timeout=15)
    assert result['reply']=='persistent Pi reviewed result'
    after=json.loads((s.root/'providers/pi/lead/session.json').read_text(encoding='utf-8'))
    assert after['state']['sessionId']==before['state']['sessionId'] and len(r['prompts']())==1

def test_first_claude_prompt_creates_transcript_and_cache_serves_second(runtime):
    r=runtime;r['defer']();s=r['start']();first=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    assert not r['path'].exists();r['finish'](prompt)
    assert await_watch(s,first['job_id'],lambda v:v['terminal'],timeout=15)['reply']=='Claude terminal result'
    cache=s.root/'providers/claude/worker/herdr-history.json';assert json.loads(cache.read_text())['history']['session_id']==r['session']
    assert not (s.root/'providers/codex/worker/herdr-history.json').exists()
    second=r['submit'](s,body='second');prompt=wait_for(lambda:r['prompts']() if len(r['prompts']())==2 else None)[-1]
    r['finish'](prompt,'second reply')
    assert await_watch(s,second['job_id'],lambda v:v['terminal'],timeout=15)['reply']=='second reply'

@pytest.mark.parametrize('deferred',[False,True])
def test_claude_idle_confirmation_is_durable_and_bound_to_exact_transcript(runtime,deferred):
    r=runtime
    if deferred:r['defer']()
    s=r['start']();task=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    r['finish'](prompt,settle=False);time.sleep(.3)
    current=s.rpc('records.latest',dict(stream='jobs',identity='job_id',agent_name='worker'),version=1)['result'][-1]
    assert current['provider_options'].get('herdr_terminal_observation') is None
    r['idle']()
    try:await_watch(s,task['job_id'],lambda v:v['terminal'])
    except (OSError,ValueError):
        if s.proc.poll() is not None:pytest.fail(s.proc.stderr.read())
        raise
    current=s.rpc('records.latest',dict(stream='jobs',identity='job_id',agent_name='worker'),version=1)['result'][-1]
    observed=current['provider_options']['herdr_terminal_observation']
    assert observed['job_id']==task['job_id']
    assert observed['execution_instance_id']==current['provider_options']['native_execution_instance']
    assert observed['decision']['diagnostics']['herdr_idle_observation']['identity']==current['provider_options']['herdr_dispatch']['identity']
    assert observed['decision']['source_cursor']['offset']==r['path'].stat().st_size
    assert observed['decision']['source_cursor']['digest']

def test_explicit_retry_gets_new_permission_without_inheriting_old_idle_evidence(runtime):
    r=runtime;s=r['start']();task=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    message=s.rpc('records.latest',dict(stream='messages',identity='message_id'),version=1)['result'][-1]
    message['retry_policy']['max_attempts']=1
    s.rpc('records.append',dict(stream='messages',record=message),version=1)
    user=r['row']('user','failed-user',content=prompt['body'])
    assistant=r['row']('assistant','failed-assistant','failed-user',[dict(type='text',text='synthetic API error')],'end_turn')
    assistant['isApiErrorMessage']=True;r['append'](user,assistant);r['idle']()
    assert await_watch(s,task['job_id'],lambda v:v['terminal'])['status']=='failed'
    previous=s.rpc('records.latest',dict(stream='jobs',identity='job_id',agent_name='worker'),version=1)['result'][-1]
    assert previous['provider_options']['herdr_terminal_observation']
    retried=s.rpc('retry',dict(target=task['job_id']))
    next_prompt=wait_for(lambda:r['prompts']() if len(r['prompts']())==2 else None)[-1]
    current=s.rpc('records.latest',dict(stream='jobs',identity='job_id',agent_name='worker'),version=1)['result'][-1]
    assert current['job_id']==retried['job_id']
    assert current['provider_options']['herdr_dispatch']['job_id']==retried['job_id']
    assert current['provider_options']['native_execution_instance']!=previous['provider_options']['native_execution_instance']
    assert current['provider_options'].get('herdr_terminal_observation') is None
    r['finish'](next_prompt,'new retry result')
    assert await_watch(s,retried['job_id'],lambda v:v['terminal'])['reply']=='new retry result'

@pytest.mark.parametrize('deferred',[False,True])
def test_claude_transcript_recovery_never_replays_permitted_prompt(runtime,deferred):
    r=runtime
    if deferred:r['defer']()
    s=r['start']();task=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    s.proc.kill();s.proc.wait(timeout=5)
    r['finish'](prompt);restored=r['start']()
    assert await_watch(restored,task['job_id'],lambda v:v['terminal'],timeout=15)['reply']=='Claude terminal result'
    assert len(r['prompts']())==1

def test_claude_native_session_change_before_idle_is_not_completion(runtime):
    r=runtime;s=r['start']();task=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    r['finish'](prompt,settle=False)
    change(r['backend'],lambda state:state['agents'][r['name']].update(agent_session=dict(agent='claude',kind='id',value=str(uuid.uuid4()),source='fixture'),interactive_ready=True))
    wait_for(lambda:(s.root/'providers/claude/worker/fault.json').exists())
    assert not s.rpc('watch',{'target':task['job_id']})['terminal'] and len(r['prompts']())==1

def test_original_claude_lead_is_resumed_by_worker_result_in_same_session(runtime):
    r=runtime;r['agents']['lead']=pi_spec('worker');s=r['start']();parent=r['submit'](s)
    first=wait_for(lambda:r['prompts']())[-1]
    s.rpc('submit',dict(project_id=r['request']['config']['project_id'],to_agent='lead',from_actor='worker',
                       body='delegated task',message_type='ask',delivery_scope='single',route_options={'mode':'chain'}))
    r['finish'](first,'delegated')
    continuation=wait_for(lambda:r['prompts']() if len(r['prompts']())==2 else None)[-1]
    assert 'CCB result-chain continuation.' in continuation['body'] and 'Child status: completed' in continuation['body']
    r['finish'](continuation,'Claude reviewed Pi result')
    result=await_watch(s,parent['job_id'],lambda v:v['visible_reply_source']=='message_bureau_reply',timeout=15)
    assert result['reply']=='Claude reviewed Pi result' and len(r['prompts']())==2
    assert mutate_count(r['backend'],['agent','start'])==2 # Initial two providers only.

def test_installed_claude_transcript_structure_preserves_actual_parent_chain(runtime):
    r=runtime;s=r['start']();task=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    fixture=json.loads((Path(__file__).parent/'fixtures/claude_transcript_parent_chain.json').read_text(encoding='utf-8'))
    for row in fixture['records']:
        if 'sessionId' in row:row['sessionId']=r['session']
        if 'cwd' in row:row['cwd']=str(r['project'])
        if isinstance(row.get('message',{}).get('content'),str):row['message']['content']=prompt['body']
        r['append'](row)
    r['idle']()
    assert await_watch(s,task['job_id'],lambda v:v['terminal'],timeout=15)['reply']=='final text'

@pytest.mark.parametrize('deferred',[False,True])
def test_native_claude_pasted_prompt_completes_bound_and_first_sessions(runtime,deferred):
    r=runtime
    if deferred:r['defer']()
    s=r['start']();task=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    r['finish'](prompt,wrapped=True)
    assert await_watch(s,task['job_id'],lambda v:v['terminal'],timeout=15)['reply']=='Claude terminal result'
    assert len(r['prompts']())==1

@pytest.mark.parametrize('deferred',[False,True])
def test_claude_working_in_a_workspace_subdirectory_still_completes(runtime,deferred):
    r=runtime
    if deferred:r['defer']()
    s=r['start']();task=r['submit'](s,'lead');prompt=wait_for(lambda:r['prompts']())[-1]
    sub=r['project']/'.trellis'/'spec';sub.mkdir(parents=True)
    ident=str(uuid.uuid4());rows=[r['row']('user',ident,content=prompt['body']),r['row']('assistant',ident+'-tool',ident,[{'type':'text','text':'reading spec'}],'tool_use'),
        r['row']('assistant',ident+'-reply',ident+'-tool',[{'type':'text','text':'Claude terminal result'}],'end_turn')]
    for row in rows:row['cwd']=str(sub)
    r['append'](*rows);r['idle']()
    result=await_watch(s,task['job_id'],lambda v:v['visible_reply_source']=='message_bureau_reply',timeout=15)
    assert result['reply']=='persistent Pi reviewed result' and len(r['prompts']())==1

def test_claude_background_task_notification_does_not_fault_the_turn(runtime):
    # Claude Code resumes a turn with a system-sourced row when a background
    # command finishes, e.g. an ask submitted with run_in_background.
    r=runtime;s=r['start']();task=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    ident=str(uuid.uuid4());notice=r['row']('user',ident+'-notice',ident+'-first',content='<task-notification>\n<status>completed</status>\n</task-notification>')
    notice.update(origin={'kind':'task-notification','producer':'session-task'},turnOrigin='task_notification',promptSource='system')
    r['append'](r['row']('user',ident,content=prompt['body']),r['row']('assistant',ident+'-first',ident,[{'type':'text','text':'submitted'}],'end_turn'),
                notice,r['row']('assistant',ident+'-second',ident+'-notice',[{'type':'text','text':'accepted by codex'}],'end_turn'))
    r['idle']()
    result=await_watch(s,task['job_id'],lambda v:v['terminal'],timeout=15)
    assert result['status']=='completed' and result['reply']=='submitted\n\naccepted by codex'
    assert not (s.root/'providers/claude/worker/fault.json').exists() and len(r['prompts']())==1

def test_claude_auto_compaction_mid_turn_still_completes(runtime):
    r=runtime;s=r['start']();task=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    ident=str(uuid.uuid4())
    boundary=r['row']('system',ident+'-boundary');boundary.update(subtype='compact_boundary',logicalParentUuid=ident+'-elsewhere');boundary.pop('message')
    summary=r['row']('user',ident+'-summary',ident+'-boundary',content='This session is being continued from a previous conversation.')
    summary.update(isCompactSummary=True,isVisibleInTranscriptOnly=True,summarizedTurnPromptUuids=[ident])
    r['append'](r['row']('user',ident,content=prompt['body']),r['row']('assistant',ident+'-work',ident,[{'type':'text','text':'checking'}],'tool_use'),
                boundary,summary,r['row']('assistant',ident+'-final',ident+'-summary',[{'type':'text','text':'done after compaction'}],'end_turn'))
    r['idle']()
    result=await_watch(s,task['job_id'],lambda v:v['terminal'],timeout=15)
    assert result['status']=='completed' and result['reply']=='done after compaction'
    assert not (s.root/'providers/claude/worker/fault.json').exists() and len(r['prompts']())==1

@pytest.mark.parametrize('bound',[False,True])
def test_public_open_mounts_original_claude_and_reconnects_without_relaunch(setup,bound):
    from test_native_herdr_open import cli
    project,backend,request=setup;root=project/'.ccb/ccbd'
    (project/'.ccb').mkdir(exist_ok=True);(project/'.ccb/ccb.config').write_text('worker:claude',encoding='utf-8')
    worker=request['config']['agents']['worker'];worker['provider']='claude'
    worker['model']='configured-test-model';worker['thinking']='low'
    request['config']['agents']={'worker':worker};request['config']['source_config']={'layout':'worker:claude'}
    child=subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(180)'],creationflags=0x08000000)
    history=project/'claude-history';history.mkdir();session=str(uuid.uuid4())
    if bound:
        (history/f'{session}.jsonl').write_text(json.dumps(dict(type='user',uuid='old',sessionId=session,cwd=str(project),message=dict(role='user',content='history')))+'\n',encoding='utf-8')
    state=backend_state(backend);state.update(server_running=True,shell_pid=os.getpid(),provider_pids=[child.pid])
    if bound:state['agent_sessions']={'claude':dict(agent='claude',kind='id',value=session,source='fixture')}
    (backend/'server.json').write_text(json.dumps(state),encoding='utf-8')
    options=dict(config=request['config'],command=request['command'],session=request['session'],history_roots={'worker':str(history)},transport_timeout_ms=5000)
    path=project/'open-options.json';path.write_text(json.dumps(options),encoding='utf-8')
    try:
        def opened():
            result=cli('--project',project,'herdr','open','--options',path,'--no-attach','--timeout-ms',15000)
            assert result.returncode==0,result.stderr
            return json.loads(result.stdout)
        opened()
        def ready():
            result=cli('--state-root',root,'ping')
            return json.loads(result.stdout) if result.returncode==0 else None
        first=wait_for(ready,timeout=20)
        opened();assert ready()['pid']==first['pid'] and mutate_count(backend,['agent','start'])==1
        journal=json.loads((root/'herdr/herdr-open.json').read_text(encoding='utf-8'))
        assert Path(journal['bindings']['worker']['history' if bound else 'discovery']['root'])==history
        calls=[json.loads(s) for s in (backend/'calls.jsonl').read_text(encoding='utf-8').splitlines()]
        launch=next(c for c in calls if c[:2]==['agent','start'])
        assert '--effort' in launch and '--model' in launch and '--input-format' not in launch
        assert '--append-system-prompt-file' in launch
        context=Path(launch[launch.index('--append-system-prompt-file')+1]).read_text(encoding='utf-8')
        assert '<ccb_interactive_coordination>' in context and '--from' in context
    finally:
        cli('--state-root',root,'stop')
        if child.poll() is None:child.kill()
        child.wait(timeout=5)

def context(s,action,*agents):
    return s.rpc(action,{'operation_id':str(uuid.uuid4()),'agent_names':list(agents)})['context']

def await_context(s,operation,timeout=20):
    return wait_for(lambda:(v if (v:=s.rpc('context-status',{'operation_id':operation})['context'])['status']!='pending' else None),timeout=timeout)

def test_terminal_clear_sends_native_command_and_rotates_to_the_new_session(runtime):
    r=runtime;s=r['start']()
    op=context(s,'clear','worker');assert op['status']=='pending'
    prompt=wait_for(lambda:r['prompts']())[-1];assert prompt['body']=='/clear'
    fresh=str(uuid.uuid4());new=r['path'].parent/f'{fresh}.jsonl'
    new.write_text(json.dumps(dict(type='user',uuid='clear-caveat',parentUuid=None,sessionId=fresh,cwd=str(r['project']),isSidechain=False,isMeta=True,
        message=dict(role='user',content='<command-name>/clear</command-name>')))+'\n',encoding='utf-8')
    change(r['backend'],lambda st:st['agents'][r['name']].update(agent_session=dict(agent='claude',kind='id',value=fresh,source='fixture'),agent_status='idle',interactive_ready=True))
    done=await_context(s,op['operation_id'])
    assert done['status']=='completed' and done['results'][0]['completion_reason']=='herdr_context_cleared'
    rotation=json.loads((s.root/'providers/claude/worker/herdr-rotation.json').read_text(encoding='utf-8'))
    assert rotation['from_session']==r['session'] and rotation['to_session']==fresh
    task=r['submit'](s,body='after clear');prompt=wait_for(lambda:r['prompts']() if len(r['prompts']())==2 else None)[-1]
    assert prompt['body'].startswith('CCB_REQ_ID:') and 'after clear' in prompt['body']
    ident=str(uuid.uuid4())
    with new.open('a',encoding='utf-8') as f:
        f.write(json.dumps(dict(type='user',uuid=ident,parentUuid='clear-caveat',sessionId=fresh,cwd=str(r['project']),isSidechain=False,message=dict(role='user',content=prompt['body'])))+'\n')
        f.write(json.dumps(dict(type='assistant',uuid=ident+'-r',parentUuid=ident,sessionId=fresh,cwd=str(r['project']),isSidechain=False,
            message=dict(role='assistant',id='msg-'+ident,content=[{'type':'text','text':'fresh session reply'}],stop_reason='end_turn')))+'\n')
    r['idle']()
    assert await_watch(s,task['job_id'],lambda v:v['terminal'],timeout=15)['reply']=='fresh session reply'
    assert len(r['prompts']())==2

def test_terminal_compact_keeps_session_and_waits_for_idle(runtime):
    r=runtime;s=r['start']()
    op=context(s,'compact','worker');prompt=wait_for(lambda:r['prompts']())[-1];assert prompt['body']=='/compact'
    time.sleep(1);assert s.rpc('context-status',{'operation_id':op['operation_id']})['context']['status']=='pending'
    r['idle']();done=await_context(s,op['operation_id'])
    assert done['status']=='completed' and done['results'][0]['completion_reason']=='herdr_context_compacted'
    assert not (s.root/'providers/claude/worker/herdr-rotation.json').exists()

def test_terminal_context_control_is_blocked_while_the_agent_is_busy(runtime):
    r=runtime;s=r['start']();r['submit'](s);wait_for(lambda:r['prompts']())
    blocked=s.rpc('clear',{'operation_id':str(uuid.uuid4()),'agent_names':['worker']})['context']
    assert blocked['status']=='blocked' and len(r['prompts']())==1

def test_terminal_clear_rotation_survives_daemon_restart(runtime):
    r=runtime;s=r['start']()
    op=context(s,'clear','worker');wait_for(lambda:r['prompts']())
    fresh=str(uuid.uuid4());new=r['path'].parent/f'{fresh}.jsonl'
    new.write_text(json.dumps(dict(type='user',uuid='clear-caveat',parentUuid=None,sessionId=fresh,cwd=str(r['project']),isSidechain=False,isMeta=True,
        message=dict(role='user',content='<command-name>/clear</command-name>')))+'\n',encoding='utf-8')
    change(r['backend'],lambda st:st['agents'][r['name']].update(agent_session=dict(agent='claude',kind='id',value=fresh,source='fixture'),agent_status='idle',interactive_ready=True))
    assert await_context(s,op['operation_id'])['status']=='completed'
    s.close();resumed=r['start']()
    task=r['submit'](resumed,body='after restart');prompt=wait_for(lambda:r['prompts']() if len(r['prompts']())==2 else None)[-1]
    ident=str(uuid.uuid4())
    with new.open('a',encoding='utf-8') as f:
        f.write(json.dumps(dict(type='user',uuid=ident,parentUuid='clear-caveat',sessionId=fresh,cwd=str(r['project']),isSidechain=False,message=dict(role='user',content=prompt['body'])))+'\n')
        f.write(json.dumps(dict(type='assistant',uuid=ident+'-r',parentUuid=ident,sessionId=fresh,cwd=str(r['project']),isSidechain=False,
            message=dict(role='assistant',id='msg-'+ident,content=[{'type':'text','text':'restarted fresh reply'}],stop_reason='end_turn')))+'\n')
    r['idle']()
    assert await_watch(resumed,task['job_id'],lambda v:v['terminal'],timeout=15)['reply']=='restarted fresh reply'

def fresh_transcript(r,fresh,complete=True):
    new=r['path'].parent/f'{fresh}.jsonl'
    rows=[dict(type='mode',mode='normal',sessionId=fresh)]
    if complete:rows.append(dict(type='user',uuid='clear-caveat',parentUuid=None,sessionId=fresh,cwd=str(r['project']),isSidechain=False,isMeta=True,
        message=dict(role='user',content='<command-name>/clear</command-name>')))
    new.write_text(''.join(json.dumps(x)+'\n' for x in rows),encoding='utf-8');return new

def test_terminal_clear_waits_for_the_new_transcript_to_be_written(runtime):
    r=runtime;s=r['start']();op=context(s,'clear','worker');wait_for(lambda:r['prompts']())
    fresh=str(uuid.uuid4());fresh_transcript(r,fresh,complete=False)
    change(r['backend'],lambda st:st['agents'][r['name']].update(agent_session=dict(agent='claude',kind='id',value=fresh,source='fixture'),agent_status='idle',interactive_ready=True))
    time.sleep(3)
    assert s.rpc('context-status',{'operation_id':op['operation_id']})['context']['status']=='pending'
    assert not (s.root/'providers/claude/worker/fault.json').exists()
    fresh_transcript(r,fresh)
    assert await_context(s,op['operation_id'])['status']=='completed'
    assert json.loads((s.root/'providers/claude/worker/herdr-rotation.json').read_text(encoding='utf-8'))['to_session']==fresh

def test_terminal_clear_interrupted_by_daemon_death_rotates_without_resending(runtime):
    r=runtime;s=r['start']();op=context(s,'clear','worker');wait_for(lambda:r['prompts']())
    s.expected_exit=True;s.proc.kill();s.proc.wait(timeout=5)
    fresh=str(uuid.uuid4());fresh_transcript(r,fresh)
    change(r['backend'],lambda st:st['agents'][r['name']].update(agent_session=dict(agent='claude',kind='id',value=fresh,source='fixture'),agent_status='idle',interactive_ready=True))
    resumed=r['start']()
    done=await_context(resumed,op['operation_id'])
    assert done['status']=='completed' and done['results'][0]['completion_reason']=='herdr_context_cleared'
    assert len(r['prompts']())==1
    assert json.loads((resumed.root/'providers/claude/worker/herdr-rotation.json').read_text(encoding='utf-8'))['to_session']==fresh

def test_terminal_clear_with_discovery_binding_serves_the_next_task(runtime):
    r=runtime;r['defer']()
    r['append'](r['row']('user','old',content='historical input'))
    change(r['backend'],lambda st:st['agents'][r['name']].update(agent_session=dict(agent='claude',kind='id',value=r['session'],source='fixture')))
    s=r['start']();op=context(s,'clear','worker');wait_for(lambda:r['prompts']())
    fresh=str(uuid.uuid4());new=fresh_transcript(r,fresh)
    change(r['backend'],lambda st:st['agents'][r['name']].update(agent_session=dict(agent='claude',kind='id',value=fresh,source='fixture'),agent_status='idle',interactive_ready=True))
    assert await_context(s,op['operation_id'])['status']=='completed'
    cache=json.loads((s.root/'providers/claude/worker/herdr-history.json').read_text(encoding='utf-8'))
    assert cache['history']['session_id']==fresh and cache['history']['root']==r['worker']['herdr_discovery']['root']
    task=r['submit'](s,body='after discovery clear');prompt=wait_for(lambda:r['prompts']() if len(r['prompts']())==2 else None)[-1]
    ident=str(uuid.uuid4())
    with new.open('a',encoding='utf-8') as f:
        f.write(json.dumps(dict(type='user',uuid=ident,parentUuid='clear-caveat',sessionId=fresh,cwd=str(r['project']),isSidechain=False,message=dict(role='user',content=prompt['body'])))+'\n')
        f.write(json.dumps(dict(type='assistant',uuid=ident+'-r',parentUuid=ident,sessionId=fresh,cwd=str(r['project']),isSidechain=False,
            message=dict(role='assistant',id='msg-'+ident,content=[{'type':'text','text':'discovery fresh reply'}],stop_reason='end_turn')))+'\n')
    r['idle']()
    assert await_watch(s,task['job_id'],lambda v:v['terminal'],timeout=15)['reply']=='discovery fresh reply'
