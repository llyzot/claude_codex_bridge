"""Claude management requires built-in, lifecycle, reset/boundary and idle evidence."""
import json, sys, time
from pathlib import Path
import pytest
from test_native_daemon import Server,submit,await_watch,spec
from test_native_restart import ready
from test_native_context import control,settled,job
from test_native_provider_faults import fault

STUB=Path(__file__).parent/'stubs/claude_context_provider.py'
def provider(tmp_path,mode='normal'):
    root=tmp_path/'provider';root.mkdir()
    return dict(provider='claude',driver_kind='claude_stream',profile_family='protocol_turn',
                command=[sys.executable,'-I',str(STUB),mode,str(root)])
def wire(tmp_path):
    path=tmp_path/'provider/wire.jsonl'
    if not path.exists():return []
    return [json.loads(line) for line in path.read_text().splitlines(keepends=True) if line.endswith('\n')]

@pytest.mark.parametrize('action',['clear','compact'])
@pytest.mark.parametrize('mode',['normal','lifecycle-first'])
def test_claude_control_stays_in_process_and_uses_actual_session(tmp_path,action,mode):
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,mode)})
    metadata=server.root/'providers/claude/worker/session.json'
    try:
        before=ready(server)
        await_watch(server,submit(server)['job_id'],lambda r:r['terminal'])
        old=json.loads(metadata.read_text())['state']['session_id']
        operation=control(server,action)
        result=settled(server,operation)
        assert result['status']=='completed',result
        state=json.loads(metadata.read_text())['state']
        assert (state['session_id']!=old)==(action=='clear')
        if action=='clear':
            assert state['session_id']==(tmp_path/'provider/actual-session').read_text()
            assert state['session_id']!=(tmp_path/'provider/display-id').read_text()
        assert ready(server)['runtime_instance_id']==before['runtime_instance_id']
        assert await_watch(server,submit(server)['job_id'],lambda r:r['terminal'])['reply']=='normal reply'
        controls=[r for r in wire(tmp_path) if r.get('type')=='user' and r['message']['content']==f'/{action}']
        assert len(controls)==1
        assert control(server,action,operation_id=operation['operation_id'])['status']=='completed'
    finally:server.close()

@pytest.mark.parametrize('mode',['no-boundary','auto-boundary'])
def test_success_without_manual_boundary_is_not_compaction(tmp_path,mode):
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,mode)})
    try:
        ready(server);result=settled(server,control(server,'compact'))
        assert result['status']=='failed'
        assert result['results'][0]['status']=='incomplete'
        assert result['results'][0]['completion_reason']=='claude_context_compaction_not_performed'
        assert ready(server)
    finally:server.close()

@pytest.mark.parametrize('mode',['old-version','shadow','duplicate'])
def test_unverified_command_never_becomes_a_user_prompt(tmp_path,mode):
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,mode)})
    try:
        ready(server);result=settled(server,control(server))
        assert result['status']=='failed'
        assert all(r['type']!='user' for r in wire(tmp_path))
        assert ready(server)
    finally:server.close()

@pytest.mark.parametrize('mode',['foreign-reset','false-success','missing-local','pending-permission','unowned-reset','foreign-start'])
def test_inconsistent_or_unowned_state_keeps_context_fenced(tmp_path,mode):
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,mode)})
    try:
        ready(server);operation=control(server)
        assert fault(server)['status']=='blocked'
        assert not server.rpc('watch',{'target':job(operation)})['terminal']
        assert control(server)['status']=='blocked'
    finally:server.close()

def test_foreign_result_does_not_complete_current_control(tmp_path):
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,'foreign-result')})
    try:
        ready(server);operation=control(server)
        await_watch(server,job(operation),lambda r:r['status']=='running')
        time.sleep(.15)
        assert server.rpc('context-status',{'operation_id':operation['operation_id']})['context']['status']=='pending'
        (tmp_path/'provider/release').touch()
        assert settled(server,operation)['status']=='completed'
    finally:server.close()

@pytest.mark.parametrize('mode',['delay','delay-preflight'])
def test_cancel_is_correlated_and_preflight_cancel_sends_no_interrupt(tmp_path,mode):
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,mode)})
    try:
        ready(server);operation=control(server,'compact')
        await_watch(server,job(operation),lambda r:r['status']=='running')
        if mode=='delay':
            until=time.monotonic()+3
            while time.monotonic()<until and not any(r['type']=='user' for r in wire(tmp_path)):time.sleep(.02)
        server.rpc('cancel',{'job_id':job(operation)})
        if mode=='delay-preflight':(tmp_path/'provider/release').touch()
        result=settled(server,operation)
        assert result['results'][0]['status']=='cancelled'
        if mode=='delay-preflight':
            assert all(r['type']!='user' and r.get('request',{}).get('subtype')!='interrupt' for r in wire(tmp_path))
        assert ready(server)
    finally:server.close()

def test_rejection_has_no_retry_and_role_context_is_not_a_prompt_dependency(tmp_path):
    agent=provider(tmp_path,'reject');agent['role']='test.uninstalled_role'
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        ready(server);result=settled(server,control(server))
        assert result['results'][0]['status']=='failed'
        assert 'PRIVATE_PROVIDER_DETAIL' not in json.dumps(result)
        assert len(server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result'])==1
        assert all(r.get('request',{}).get('subtype')!='reload_skills' for r in wire(tmp_path))
        with pytest.raises(AssertionError,match='cannot be replayed'):server.rpc('retry',{'target':job(result)})
    finally:server.close()


def test_queue_cancel_cannot_override_an_observed_reset(tmp_path):
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,'effect-then-cancel')})
    try:
        ready(server);operation=control(server)
        until=time.monotonic()+4
        while not (tmp_path/'provider/effect-present').exists() and time.monotonic()<until:time.sleep(.02)
        assert (tmp_path/'provider/effect-present').exists()
        server.rpc('cancel',{'job_id':job(operation)})
        assert fault(server)['status']=='blocked'
        assert not server.rpc('watch',{'target':job(operation)})['terminal']
    finally:server.close()


def test_unmanaged_reset_cannot_silently_change_later_task_context(tmp_path):
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,'unowned-followup')})
    try:
        ready(server);assert settled(server,control(server))['status']=='completed'
        followup=submit(server)['job_id']
        assert fault(server)['status']=='blocked'
        assert not server.rpc('watch',{'target':followup})['terminal']
    finally:server.close()


def test_idle_shutdown_flushes_context_before_owned_process_is_reaped(tmp_path):
    agents={'worker':provider(tmp_path,'flush-on-eof')}
    server=Server(tmp_path/'state',agents)
    try:
        ready(server);assert settled(server,control(server))['status']=='completed'
        session=json.loads((server.root/'providers/claude/worker/session.json').read_text())['state']['session_id']
    finally:server.close()
    assert (tmp_path/'provider/flushed-session').is_file(),'Idle provider was killed before its EOF persistence completed'
    assert (tmp_path/'provider/flushed-session').read_text()==session
    shutdown=json.loads((server.root/'lifecycle.json').read_text())['idle_provider_shutdown']
    assert shutdown[0]['status']=='exited' and shutdown[0]['exit_code']==0
    restored=Server(server.root,agents)
    try:
        ready(restored)
        result=await_watch(restored,submit(restored)['job_id'],lambda r:r['terminal'])
        assert result['status']=='completed'
        assert json.loads((restored.root/'providers/claude/worker/session.json').read_text())['state']['session_id']==session
    finally:restored.close()


def test_idle_shutdown_has_one_deadline_for_all_providers(tmp_path):
    agents={}
    for name in ['alpha','beta','gamma']:
        directory=tmp_path/name;directory.mkdir()
        agents[name]=provider(directory,'eof-hang')
    server=Server(tmp_path/'state',agents)
    for name in agents:ready(server,name)
    begin=time.monotonic();server.close();elapsed=time.monotonic()-begin
    rows=json.loads((server.root/'lifecycle.json').read_text())['idle_provider_shutdown']
    assert len(rows)==3 and all(r['status']=='grace_expired' for r in rows)
    assert elapsed<2.5,'Shutdown grace must be shared across providers, not paid once per provider'
    from native_kernel import _owner_stopped
    assert all(_owner_stopped(r['pid']) for r in rows)


def test_shutdown_keeps_busy_provider_on_existing_forced_stop_path(tmp_path):
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,'flush-busy')})
    job_id=None
    try:
        ready(server);job_id=submit(server)['job_id']
        await_watch(server,job_id,lambda r:r['status']=='running')
    finally:server.close()
    assert not (tmp_path/'provider/flushed-session').exists()
    assert not json.loads((server.root/'lifecycle.json').read_text()).get('idle_provider_shutdown')
    rows=[json.loads(line) for line in (server.root.parent/'agents/worker/jobs.jsonl').read_text().splitlines()]
    assert next(r for r in reversed(rows) if r['job_id']==job_id)['status']=='running'


@pytest.mark.parametrize('method',['restart','reload'])
def test_restart_flushes_idle_claude_before_rebinding_saved_context(tmp_path,method):
    from test_native_restart import settled as restart_settled
    from test_native_reload import reload,config,settled as reload_settled
    agents={'worker':provider(tmp_path,'flush-on-eof')}
    server=Server(tmp_path/'state',agents)
    try:
        ready(server);assert settled(server,control(server))['status']=='completed'
        if method=='restart':
            operation=server.rpc('restart',{'agent_names':['worker']})['reload']
            result=restart_settled(server,operation)
        else:
            import copy
            desired=config(copy.deepcopy(agents));desired['agents']['worker']['timeout_s']=61
            operation=reload(server,desired);result=reload_settled(server,operation)
        assert result['status']=='applied'
        assert result['idle_provider_shutdown']['worker']['status']=='exited'
        assert (tmp_path/'provider/flushed-session').is_file(),'Restart killed the idle provider before EOF persistence'
        assert await_watch(server,submit(server)['job_id'],lambda r:r['terminal'])['status']=='completed'
    finally:server.close()


def test_restart_eof_grace_keeps_actor_responsive_and_has_forced_fallback(tmp_path):
    from test_native_restart import settled as restart_settled
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,'eof-hang')})
    try:
        old=ready(server)
        operation=server.rpc('restart',{'agent_names':['worker']})['reload']
        until=time.monotonic()+4
        while time.monotonic()<until:
            row=server.rpc('restart-status',{'operation_id':operation['operation_id']})['reload']
            if row['status']=='retiring':break
            time.sleep(.02)
        assert row['status']=='retiring'
        begin=time.monotonic();server.rpc('ping');assert time.monotonic()-begin<.5
        result=restart_settled(server,operation)
        assert result['status']=='applied'
        assert result['idle_provider_shutdown']['worker']['status']=='grace_expired'
        assert ready(server,previous=old['runtime_instance_id'])
    finally:server.close()
