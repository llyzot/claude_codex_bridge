"""Wait for actual quiescence without stopping queue or callback progress."""
import copy
import json
import time
import uuid

import pytest
from test_native_daemon import Server, spec, submit, await_watch
from test_native_reload import config
from test_native_project_view import finish
from test_native_cli_security import cli
from test_native_followup import start as start_followup, admit, settled as followup_settled
from test_native_restart import ready


def defer(server, desired):
    return server.rpc('reload', {'config':desired, 'expected_fingerprint':server.rpc('ping')['config_fingerprint'], 'when_idle':True})['reload']


def applied(server, receipt):
    end=time.monotonic()+10
    while time.monotonic()<end:
        row=server.rpc('reload-status')['reload']
        assert row['operation_id']==receipt['operation_id']
        if row['status'] not in ['waiting_for_idle','preparing','retiring']:
            assert row['status']=='applied',row
            return row
        time.sleep(.025)
    pytest.fail(f'deferred reload not applied: {row}')


def test_queued_work_drains_in_order_and_repeated_request_keeps_identity(tmp_path):
    agents={'worker':spec()};server=Server(tmp_path/'state',agents)
    try:
        jobs=[submit(server,body=body)['job_id'] for body in ['one','two']]
        desired=config(copy.deepcopy(agents));desired['agents']['worker']['timeout_s']=31
        receipt=defer(server,desired)
        assert receipt['status']=='waiting_for_idle'
        assert defer(server,desired)['operation_id']==receipt['operation_id']
        assert server.rpc('ping')['config_fingerprint']==receipt['current_fingerprint']
        view=server.rpc('project_view',{'schema_version':1})['view']
        agent=next(row for row in view['agents'] if row['name']=='worker')
        assert agent['reload_drain']['status']=='waiting_for_idle'
        assert agent['reload_drain']['admission_paused'] is False
        # Waiting does not silently discard new admissions; they extend the wait.
        jobs.append(submit(server,body='three')['job_id'])
        for expected in jobs:
            work=server.rpc('work',{'agent_name':'worker'})['job']
            assert work['job_id']==expected
            assert server.rpc('reload-status')['reload']['status']=='waiting_for_idle'
            finish(server,work)
        final=applied(server,receipt)
        assert final['waiting_since']==receipt['started_at']
        assert server.rpc('ping')['config_fingerprint']==receipt['desired_fingerprint']
        assert all(server.rpc('watch',{'target':job})['status']=='completed' for job in jobs)
    finally:server.close()


def test_deferred_reload_preserves_worker_result_and_automatic_lead_continuation(tmp_path):
    agents={'worker':spec('slow-input'),'lead':spec('lead')}
    server=Server(tmp_path/'state',agents)
    try:
        job=submit(server,'lead')['job_id']
        await_watch(server,job,lambda r:r['status']=='running')
        desired=config(copy.deepcopy(agents));desired['agents']['lead']['timeout_s']=35
        receipt=defer(server,desired);assert receipt['status']=='waiting_for_idle'
        done=await_watch(server,job,lambda r:r['visible_reply_source']=='message_bureau_reply')
        assert done['reply']=='reviewed: worker result'
        applied(server,receipt)
        children=server.rpc('records.latest',{'stream':'jobs','agent_name':'worker','identity':'job_id'},version=1)['result']
        assert len(children)==1 and children[0]['status']=='completed'
    finally:server.close()


def test_cancel_waiting_change_is_fenced_idempotent_and_keeps_work(tmp_path):
    agents={'worker':spec()};server=Server(tmp_path/'state',agents)
    try:
        job=submit(server)['job_id'];desired=config(copy.deepcopy(agents));desired['agents']['worker']['timeout_s']=31
        receipt=defer(server,desired)
        with pytest.raises(AssertionError,match='identity mismatch'):
            server.rpc('reload-cancel',{'operation_id':str(uuid.uuid4())})
        response=cli('--state-root',server.root,'reload-cancel',receipt['operation_id'])
        assert response.returncode==0,response.stderr
        cancelled=json.loads(response.stdout)['reload'];assert cancelled['status']=='cancelled'
        assert server.rpc('reload-cancel',{'operation_id':receipt['operation_id']})['reload']==cancelled
        assert server.rpc('watch',{'target':job})['status']=='accepted'
        assert server.rpc('ping')['config_fingerprint']==receipt['current_fingerprint']
        work=server.rpc('work',{'agent_name':'worker'})['job'];finish(server,work)
        time.sleep(.3)
        assert server.rpc('reload-status')['reload']['status']=='cancelled'
    finally:server.close()


@pytest.mark.parametrize('abrupt', [False, True])
def test_restart_interrupts_waiting_change_without_leaking_or_applying_desired_config(tmp_path, abrupt):
    agents={'worker':spec()};server=Server(tmp_path/'state',agents)
    try:
        job=submit(server)['job_id'];desired=config(copy.deepcopy(agents))
        desired['agents']['worker']['env']={'TEST_SECRET':'NOT_FOR_RELOAD_JOURNAL'}
        receipt=defer(server,desired)
        assert 'NOT_FOR_RELOAD_JOURNAL' not in (server.root/'reload.json').read_text(encoding='utf-8')
        if abrupt:
            server.proc.kill();server.proc.wait(timeout=5)
        else:
            server.close()
    finally:
        if server.proc.poll() is None:server.close()
    restored=Server(server.root,agents)
    try:
        row=restored.rpc('reload-status')['reload']
        assert row['operation_id']==receipt['operation_id'] and row['status']=='interrupted'
        assert restored.rpc('ping')['config_fingerprint']==receipt['current_fingerprint']
        assert restored.rpc('watch',{'target':job})['status']=='accepted'
        work=restored.rpc('work',{'agent_name':'worker'})['job'];finish(restored,work)
        assert restored.rpc('reload-status')['reload']['status']=='interrupted'
    finally:restored.close()


def test_cli_observation_timeout_does_not_cancel_the_waiting_operation(tmp_path):
    agents={'worker':spec()};server=Server(tmp_path/'state',agents)
    try:
        submit(server);desired=config(copy.deepcopy(agents));desired['agents']['worker']['timeout_s']=31
        path=tmp_path/'desired.json';path.write_text(json.dumps(desired),encoding='utf-8')
        response=cli('--state-root',server.root,'reload','--config',path,'--when-idle','--wait','--timeout-ms','0')
        assert response.returncode==0,response.stderr
        result=json.loads(response.stdout);assert result['observation_timeout']
        receipt=result['reload'];assert receipt['status']=='waiting_for_idle'
        work=server.rpc('work',{'agent_name':'worker'})['job'];finish(server,work)
        applied(server,receipt)
        with pytest.raises(AssertionError,match='only a reload still waiting'):
            server.rpc('reload-cancel',{'operation_id':receipt['operation_id']})
    finally:server.close()


def test_active_codex_can_finish_and_is_replaced_only_afterward(tmp_path):
    server,job,trace,worker=start_followup(tmp_path,'followup-finish')
    try:
        before=server.rpc('provider-status',{'agent_name':'worker'})['owned']
        desired=config({'worker':copy.deepcopy(worker)});desired['agents']['worker']['timeout_s']=70
        receipt=defer(server,desired);assert receipt['status']=='waiting_for_idle'
        assert followup_settled(server,admit(server,job,'finished before reload'))['status']=='injected'
        assert await_watch(server,job,lambda r:r['terminal'])['reply']=='finished before reload'
        applied(server,receipt)
        assert ready(server)['runtime_instance_id']!=before['runtime_instance_id']
    finally:server.close()


def test_invalid_defer_flag_does_not_mutate_runtime(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        with pytest.raises(AssertionError,match='when_idle must be boolean'):
            server.rpc('reload',{'config':config({'worker':spec()}),'when_idle':'yes'})
        assert not (server.root/'reload.json').exists()
    finally:server.close()
