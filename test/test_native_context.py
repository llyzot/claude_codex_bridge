"""Native management controls share durable Jobs without turning them into prompts."""
import json
import os
import sys
import time
import uuid
from pathlib import Path

import pytest
from test_native_daemon import Server, spec, submit, await_watch
from test_native_restart import ready
from test_native_cli_security import cli
from test_native_project_view import finish

STUB = Path(__file__).parent/'stubs/native_context_provider.py'

def provider(tmp_path, name='worker', kind='pi', mode='normal'):
    directory = tmp_path/name; directory.mkdir(exist_ok=True)
    return dict(provider=kind, driver_kind='pi_rpc' if kind=='pi' else 'codex_app_server',
                profile_family='protocol_turn', command=[sys.executable, '-I', str(STUB), kind, mode, str(directory)])

def control(server, action='clear', names=None, **kwargs):
    return server.rpc(action, dict(agent_names=names or ['worker'], **kwargs))['context']

def settled(server, operation, timeout=8):
    until = time.monotonic()+timeout
    while time.monotonic()<until:
        result = server.rpc('context-status', {'operation_id':operation['operation_id']})['context']
        if result['status'] != 'pending': return result
        time.sleep(.02)
    pytest.fail(f'context operation did not settle: {result}')

def job(operation): return operation['receipts'][0]['job_id']

def wire(tmp_path, name='worker'):
    return [json.loads(line) for line in (tmp_path/name/'wire.jsonl').read_text().splitlines()]

@pytest.mark.parametrize('kind', ['pi', 'codex'])
@pytest.mark.parametrize('action', ['clear', 'compact'])
def test_control_confirms_session_and_keeps_process_and_job_history(tmp_path, kind, action):
    server = Server(tmp_path/'state', {'worker':provider(tmp_path,kind=kind)})
    try:
        first = submit(server)['job_id']; await_watch(server, first, lambda r:r['terminal'])
        before = ready(server)
        metadata = server.root/f'providers/{kind}/worker/session.json'
        before_state = json.loads(metadata.read_text())['state']
        operation = control(server, action)
        result = settled(server, operation)
        assert result['status'] == 'completed', result
        after = ready(server)
        assert after['runtime_instance_id'] == before['runtime_instance_id'] and after['pid'] == before['pid']
        after_state = json.loads(metadata.read_text())['state']
        key = 'sessionId' if kind=='pi' else 'thread_id'
        assert (after_state[key] != before_state[key]) == (action=='clear')
        assert server.rpc('watch', {'target':first})['terminal']
        second = submit(server)['job_id']; assert await_watch(server, second, lambda r:r['terminal'])['reply']=='normal reply'
        assert 'native_empty_context' not in json.loads(metadata.read_text())['state']
        commands = wire(tmp_path)
        assert sum(c.get('type')=='prompt' or c.get('method')=='turn/start' for c in commands)==2
        assert 'PRIVATE_SUMMARY_NOT_FOR_CONTROLLER' not in json.dumps(result)
        assert control(server,action,operation_id=operation['operation_id'])['operation_id']==operation['operation_id']
        assert len(server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result'])==3
    finally: server.close()

@pytest.mark.parametrize('kind', ['pi', 'codex'])
def test_rejected_control_is_terminal_without_auto_or_explicit_retry(tmp_path, kind):
    server = Server(tmp_path/'state', {'worker':provider(tmp_path,kind=kind,mode='reject')})
    try:
        ready(server)
        result = settled(server,control(server,'compact'))
        assert result['status']=='failed'
        assert len(server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result'])==1
        with pytest.raises(AssertionError,match='cannot be replayed'): server.rpc('retry',{'target':job(result)})
        assert ready(server)
    finally: server.close()

def test_pi_extension_veto_is_not_reported_as_cleared(tmp_path):
    server = Server(tmp_path/'state', {'worker':provider(tmp_path,mode='veto')})
    try:
        before = ready(server)
        result = settled(server,control(server))
        assert result['status']=='failed' and result['results'][0]['status']=='cancelled'
        assert result['results'][0]['completion_reason']=='pi_context_clear_vetoed'
        assert ready(server)['runtime_instance_id']==before['runtime_instance_id']
    finally: server.close()

@pytest.mark.parametrize('kind', ['pi', 'codex'])
def test_observation_timeout_does_not_cancel_and_fifo_runs_after_control(tmp_path, kind):
    server = Server(tmp_path/'state', {'worker':provider(tmp_path,kind=kind,mode='delay')})
    try:
        ready(server)
        response = cli('--state-root',server.root,'compact','worker','--wait','--timeout-ms','0')
        assert response.returncode==0,response.stderr
        result = json.loads(response.stdout)
        assert result['observation_timeout']
        operation = result['context']
        await_watch(server,job(operation),lambda r:r['status']=='running')
        pending = submit(server)['job_id']
        assert not server.rpc('watch',{'target':pending})['terminal']
        assert server.rpc('restart',{'agent_names':['worker']})['reload']['status']=='blocked'
        (tmp_path/'worker/release').touch()
        assert settled(server,operation)['status']=='completed'
        assert await_watch(server,pending,lambda r:r['terminal'])['reply']=='normal reply'
    finally: server.close()

@pytest.mark.parametrize('kind', ['pi', 'codex'])
def test_cancelled_compaction_waits_for_provider_terminal_and_idle(tmp_path,kind):
    server = Server(tmp_path/'state', {'worker':provider(tmp_path,kind=kind,mode='cancel')})
    try:
        ready(server); operation=control(server,'compact')
        await_watch(server,job(operation),lambda r:r['status']=='running')
        # Wait until the command itself, rather than only its durable Job, is sent.
        until=time.monotonic()+4
        while time.monotonic()<until:
            if any(c.get('type')=='compact' or c.get('method')=='thread/compact/start' for c in wire(tmp_path)): break
            time.sleep(.02)
        server.rpc('cancel',{'job_id':job(operation)})
        result=settled(server,operation)
        assert result['results'][0]['status']=='cancelled'
        assert ready(server)
    finally: server.close()

def test_codex_events_before_ack_are_verified_and_old_events_do_not_complete(tmp_path):
    server=Server(tmp_path/'first/state',{'worker':provider(tmp_path,kind='codex',mode='ack-late')})
    try:
        ready(server)
        assert settled(server,control(server,'compact'))['status']=='completed'
    finally: server.close()
    other=Server(tmp_path/'second/state',{'worker':provider(tmp_path,'other',kind='codex',mode='old-event')})
    try:
        await_watch(other,submit(other)['job_id'],lambda r:r['terminal'])
        operation=control(other,'compact')
        await_watch(other,job(operation),lambda r:r['status']=='running')
        time.sleep(.2)
        assert other.rpc('context-status',{'operation_id':operation['operation_id']})['context']['status']=='pending'
    finally: other.close()

@pytest.mark.parametrize('kind,mode', [('pi','wrong-state'),('codex','missing-item')])
def test_uncertain_protocol_retains_active_control_and_fault(tmp_path,kind,mode):
    from test_native_provider_faults import fault
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,kind=kind,mode=mode)})
    try:
        ready(server); operation=control(server,'compact')
        observed=fault(server)
        assert observed['status']=='blocked'
        assert not server.rpc('watch',{'target':job(operation)})['terminal']
        assert control(server,'clear')['status']=='blocked'
    finally: server.close()

def test_busy_lead_unsupported_target_and_forged_request_cannot_clear(tmp_path):
    server=Server(tmp_path/'state',{'lead':provider(tmp_path,'lead'),'worker':spec()})
    try:
        ready(server,'lead'); submit(server,actor='lead')
        result=control(server,names=['lead'])
        assert result['status']=='blocked'
        assert any(b['reason']=='outstanding_reply' for b in result['plan']['blockers'])
        assert control(server,names=['all'])['status']=='blocked'
        with pytest.raises(AssertionError,match='management API'):
            server.rpc('submit',dict(project_id='native-test',to_agent='lead',from_actor='user',body='clear',message_type='context_control'))
        assert not (server.root/'context-operations.jsonl').exists()
    finally: server.close()

def test_multi_agent_preflight_and_identity_replay_after_daemon_restart(tmp_path):
    agents={'worker':provider(tmp_path),'second':provider(tmp_path,'second',kind='codex')}
    server=Server(tmp_path/'state',agents)
    try:
        ready(server);ready(server,'second')
        plan=server.rpc('clear',{'agent_names':['all'],'dry_run':True})['plan']
        assert plan['can_apply'] and not (server.root/'context-operations.jsonl').exists()
        operation=control(server,names=['all'])
        assert len(operation['receipts'])==2
        assert settled(server,operation)['status']=='completed'
        with pytest.raises(AssertionError,match='another request'):control(server,'compact',['all'],operation_id=operation['operation_id'])
    finally:server.close()
    restored=Server(server.root,agents)
    try:
        result=control(restored,names=['all'],operation_id=operation['operation_id'])
        assert result['status']=='completed'
        assert sum(len(restored.rpc('records.latest',dict(stream='jobs',agent_name=name,identity='job_id'),version=1)['result']) for name in agents)==2
    finally:restored.close()


@pytest.mark.parametrize('kind', ['pi','codex'])
def test_control_provider_exit_is_incomplete_and_never_replayed(tmp_path,kind):
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,kind=kind,mode='crash')})
    try:
        ready(server); operation=control(server,'compact')
        result=settled(server,operation)
        assert result['status']=='failed'
        assert result['results'][0]['status']=='incomplete'
        assert len(server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result'])==1
        assert control(server,'compact',operation_id=operation['operation_id'])['status']=='failed'
    finally:server.close()


@pytest.mark.skipif(os.name!='nt',reason='owner death process tree evidence requires Windows')
@pytest.mark.parametrize('kind', ['pi','codex'])
def test_daemon_death_keeps_unknown_control_fenced_until_reconciled(tmp_path,kind):
    agents={'worker':provider(tmp_path,kind=kind,mode='delay')}
    server=Server(tmp_path/'state',agents)
    try:
        ready(server); operation=control(server,'compact')
        await_watch(server,job(operation),lambda r:r['status']=='running')
        until=time.monotonic()+4
        while time.monotonic()<until:
            if any(c.get('type')=='compact' or c.get('method')=='thread/compact/start' for c in wire(tmp_path)):break
            time.sleep(.02)
        server.proc.kill();server.proc.wait(timeout=5)
    finally:
        if server.proc.poll() is None:server.close()
    restored=Server(server.root,agents)
    try:
        assert control(restored,'compact',operation_id=operation['operation_id'])['status']=='pending'
        assert control(restored,'clear')['status']=='blocked'
        view=restored.rpc('job-reconcile',{'target':job(operation)})
        assert view['status']=='terminal',view
        assert settled(restored,operation)['results'][0]['status']=='incomplete'
        assert sum(c.get('type')=='compact' or c.get('method')=='thread/compact/start' for c in wire(tmp_path))==1
    finally:restored.close()


def test_context_inputs_and_unverified_provider_are_rejected_before_admission(tmp_path):
    server=Server(tmp_path/'state',{'worker':provider(tmp_path),'external':spec()})
    try:
        ready(server)
        for request in [{'dry_run':'false'},{'operation_id':123},{'operation_id':'x'},
                        {'agent_names':['all','worker']},{'agent_names':[None]},{'agent_names':['']}]:
            with pytest.raises(AssertionError):server.rpc('clear',request)
        result=control(server,names=['all'])
        assert result['status']=='blocked'
        assert any(b['reason']=='native_context_protocol_unsupported' for b in result['plan']['blockers'])
        assert not (server.root/'context-operations.jsonl').exists()
    finally:server.close()


@pytest.mark.parametrize('kind',['pi','codex'])
def test_confirmed_empty_context_can_be_recreated_until_first_new_input(tmp_path,kind):
    agents={'worker':provider(tmp_path,kind=kind,mode='empty-file')}
    server=Server(tmp_path/'state',agents)
    metadata=server.root/f'providers/{kind}/worker/session.json'
    identity='sessionId' if kind=='pi' else 'thread_id'
    try:
        ready(server)
        assert settled(server,control(server))['status']=='completed'
        previous=json.loads(metadata.read_text())['state'][identity]
    finally:server.close()
    for _ in range(2):
        restored=Server(server.root,agents)
        try:
            ready(restored)
            state=json.loads(metadata.read_text())['state']
            assert state[identity] != previous
            assert state['native_empty_context']['recreated_from']==previous
            previous=state[identity]
        finally:restored.close()
    restored=Server(server.root,agents)
    try:
        ready(restored)
        result=await_watch(restored,submit(restored)['job_id'],lambda r:r['terminal'])
        assert result['status']=='completed'
        assert 'native_empty_context' not in json.loads(metadata.read_text())['state']
    finally:restored.close()


@pytest.mark.parametrize('kind',['pi','codex'])
def test_empty_recreation_permission_is_consumed_before_new_prompt_finishes(tmp_path,kind):
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,kind=kind,mode='hold-prompt')})
    metadata=server.root/f'providers/{kind}/worker/session.json'
    try:
        ready(server)
        assert settled(server,control(server))['status']=='completed'
        assert 'native_empty_context' in json.loads(metadata.read_text())['state']
        job_id=submit(server)['job_id']
        await_watch(server,job_id,lambda r:r['status']=='running')
        assert 'native_empty_context' not in json.loads(metadata.read_text())['state']
        time.sleep(.15)
        assert 'native_empty_context' not in json.loads(metadata.read_text())['state']
        assert not server.rpc('watch',{'target':job_id})['terminal']
    finally:server.close()
