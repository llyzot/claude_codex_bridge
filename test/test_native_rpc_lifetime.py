"""Provider exit must preserve evidence and isolate its FIFO from other Agents."""
import json
from pathlib import Path
import sys
import time

import pytest
from test_native_daemon import Server, await_watch, spec, submit
from test_native_pi_rpc import pi_spec

STUB = Path(__file__).parent / 'stubs' / 'native_rpc_lifetime.py'

def config(provider, mode, tmp_path):
    return dict(provider=provider, driver_kind=dict(pi='pi_rpc', codex='codex_app_server', claude='claude_stream')[provider],
                profile_family='protocol_turn', command=[sys.executable, '-I', str(STUB),
                provider, mode, str(tmp_path / 'launches')])

def metadata(server, provider, agent='worker'):
    path = server.root / f'providers/{provider}/{agent}/session.json'
    try: return json.loads(path.read_text(encoding='utf-8'))
    except (OSError, ValueError): return None

def await_closed(server, provider, agent='worker'):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        assert server.proc.poll() is None, 'provider exit killed the shared daemon'
        row = metadata(server, provider, agent)
        if row and row['state'].get('process_state') == 'exited': return row
        time.sleep(.025)
    pytest.fail('provider exit was not recorded')

@pytest.mark.parametrize('provider', ['pi', 'codex'])
def test_idle_exit_keeps_daemon_alive_and_restarts_only_for_new_work(tmp_path, provider):
    server = Server(tmp_path / 'state', {'worker': config(provider, 'idle-exit', tmp_path), 'healthy': spec('worker')})
    try:
        before = await_closed(server, provider)
        time.sleep(.2)
        assert len((tmp_path / 'launches').read_text().splitlines()) == 1
        healthy = submit(server, 'healthy')
        assert await_watch(server, healthy['job_id'], lambda r: r['terminal'])['status'] == 'completed'
        receipt = submit(server)
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['reply'] == 'lifetime result'
        assert metadata(server, provider)['runtime_instance_id'] != before['runtime_instance_id']
        assert len((tmp_path / 'launches').read_text().splitlines()) == 2
    finally: server.close()

@pytest.mark.parametrize('provider', ['pi', 'codex'])
def test_active_exit_finishes_one_attempt_and_continues_fifo_on_new_process(tmp_path, provider):
    server = Server(tmp_path / 'state', {'worker': config(provider, 'active-exit', tmp_path), 'healthy': spec('worker')})
    try:
        first = submit(server)
        second = submit(server, body='queued second task')
        row = await_watch(server, first['job_id'], lambda r: r['terminal'])
        assert row['status'] == 'incomplete'
        assert row['completion_reason'] == f'{provider}_process_exit'
        assert await_watch(server, second['job_id'], lambda r: r['terminal'])['reply'] == 'lifetime result'
        assert server.rpc('queue', {'target': 'worker'})['queues'][0]['active_job_id'] is None
        assert len((tmp_path / 'launches').read_text().splitlines()) == 2
        healthy = submit(server, 'healthy')
        assert await_watch(server, healthy['job_id'], lambda r: r['terminal'])['status'] == 'completed'
    finally: server.close()


@pytest.mark.parametrize('provider',['pi','codex','claude'])
def test_stdout_closure_during_process_exit_keeps_natural_exit_classification(tmp_path,provider):
    record=config(provider,'closing-exit',tmp_path)
    record['command'][0]=sys._base_executable
    server=Server(tmp_path/'state',{'worker':record})
    try:
        first=submit(server)['job_id'];second=submit(server)['job_id']
        result=await_watch(server,first,lambda r:r['terminal'])
        assert result['status']=='incomplete' and result['completion_reason']==f'{provider}_process_exit'
        assert await_watch(server,second,lambda r:r['terminal'])['status']=='completed'
        assert server.rpc('provider-status',{'agent_name':'worker'})['fault'] is None
    finally:server.close()

@pytest.mark.parametrize('provider', ['pi', 'codex'])
@pytest.mark.parametrize('mode', ['final-exit', 'buffered-final-exit'])
def test_verified_result_survives_process_exit_and_bounded_reader_batches(tmp_path, provider, mode):
    server = Server(tmp_path / 'state', {'worker': config(provider, mode, tmp_path)})
    try:
        receipt = submit(server)
        row = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        assert row['status'] == 'completed'
        assert row['reply'] == 'lifetime result'
        assert row['completion_reason'] == ('pi_agent_settled' if provider == 'pi' else 'codex_turn_completed')
        await_closed(server, provider)
        assert server.rpc('ping')['runtime'] == 'rust'
    finally: server.close()

@pytest.mark.parametrize('provider', ['pi', 'codex'])
def test_exit_without_stdout_eof_does_not_release_or_restart_active_execution(tmp_path, provider):
    server = Server(tmp_path / 'state', {'worker': config(provider, 'held-stdout', tmp_path)})
    try:
        receipt = submit(server)
        await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
        await_closed(server, provider)
        assert server.rpc('watch', {'target': receipt['job_id']})['status'] == 'running'
        assert len((tmp_path / 'launches').read_text().splitlines()) == 1
        (tmp_path / 'release-holder').touch()
        row = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        assert row['status'] == 'incomplete'
        assert row['completion_reason'] == f'{provider}_process_exit'
    finally:
        (tmp_path / 'release-holder').touch()
        server.close()

def latest_job(server, job_id):
    rows = server.rpc('records.latest', dict(stream='jobs', agent_name='worker', identity='job_id'), version=1)['result']
    return next(row for row in rows if row['job_id'] == job_id)

def test_pi_silent_cancel_is_durable_idempotent_and_survives_provider_exit(tmp_path):
    server = Server(tmp_path / 'state', {'worker': config('pi', 'cancel-exit', tmp_path)})
    try:
        receipt = submit(server)
        await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
        request = dict(job_id=receipt['job_id'], record_reply=False)
        server.rpc('cancel', request)
        first = latest_job(server, receipt['job_id'])
        assert first['provider_options']['native_cancel_intent']['record_reply'] is False
        server.rpc('cancel', request)
        assert latest_job(server, receipt['job_id'])['updated_at'] == first['updated_at']
        with pytest.raises(AssertionError, match='reply policy already fixed'):
            server.rpc('cancel', dict(job_id=receipt['job_id'], record_reply=True))
        row = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        assert row['status'] == 'incomplete' and row['completion_reason'] == 'pi_process_exit'
        assert server.rpc('records.list', {'stream': 'replies'}, version=1)['result'] == []
        assert len((tmp_path / 'aborts').read_text().splitlines()) == 1
        proof = json.loads((tmp_path / 'abort-proof.json').read_text())
        assert proof['intent']['record_reply'] is False and proof['event_count'] == 1
        jobs = server.rpc('records.latest', dict(stream='jobs', agent_name='worker', identity='job_id'), version=1)['result']
        assert len(jobs) == 1
        assert jobs[0]['terminal_decision']['diagnostics']['cancel_requested'] is True
    finally: server.close()

def test_pi_cancel_intent_reconciles_after_daemon_death_without_replay(tmp_path):
    root = tmp_path / 'state'
    agents = {'worker': config('pi', 'cancel-ack-only', tmp_path)}
    server = Server(root, agents)
    try:
        receipt = submit(server)
        await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
        server.rpc('cancel', dict(job_id=receipt['job_id'], record_reply=False))
        before = latest_job(server, receipt['job_id'])
        assert before['provider_options']['native_cancel_intent']['record_reply'] is False
    finally:
        server.proc.kill(); server.proc.wait(timeout=5)
    restored = Server(root, agents)
    try:
        assert latest_job(restored, receipt['job_id'])['provider_options']['native_cancel_intent'] == before['provider_options']['native_cancel_intent']
        assert await_watch(restored, receipt['job_id'], lambda r:r['terminal'])['status'] == 'cancelled'
        assert restored.rpc('queue', {'target': 'worker'})['queues'][0]['active_job_id'] is None
        assert restored.rpc('records.list', {'stream': 'replies'}, version=1)['result'] == []
    finally: restored.close()

def test_pi_deadline_intent_is_committed_before_abort_and_process_exit(tmp_path):
    record = config('pi', 'cancel-exit', tmp_path)
    record['timeout_s'] = .2
    server = Server(tmp_path / 'state', {'worker': record})
    try:
        receipt = submit(server)
        row = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        assert row['status'] == 'incomplete' and row['completion_reason'] == 'pi_process_exit'
        job = latest_job(server, receipt['job_id'])
        assert job['provider_options']['native_cancel_intent']['deadline_triggered'] is True
        assert job['terminal_decision']['diagnostics']['deadline_triggered'] is True
        proof = json.loads((tmp_path / 'abort-proof.json').read_text())
        assert proof['intent']['deadline_triggered'] is True and proof['event_count'] == 1
        events = server.rpc('records.list', dict(stream='job_events', agent_name='worker'), version=1)['result']
        assert len([e for e in events if e['type'] == 'job_cancel_requested']) == 1
    finally: server.close()

@pytest.mark.parametrize('provider', ['pi', 'codex'])
def test_worker_process_failure_automatically_resumes_persistent_pi_lead(tmp_path, provider):
    server = Server(tmp_path / 'state', {'lead': pi_spec('lead'), 'worker': config(provider, 'active-exit', tmp_path)})
    try:
        receipt = submit(server, 'lead')
        row = await_watch(server, receipt['job_id'], lambda r: r['visible_reply_source'] == 'message_bureau_reply')
        assert row['reply'] == 'persistent Pi reviewed result'
        edges = server.rpc('records.latest', dict(stream='callbacks', identity='edge_id'), version=1)['result']
        assert len(edges) == 1 and edges[0]['state'] == 'done'
        jobs = server.rpc('records.latest', dict(stream='jobs', agent_name='worker', identity='job_id'), version=1)['result']
        assert len(jobs) == 1 and jobs[0]['terminal_decision']['reason'] == f'{provider}_process_exit'
        lead = json.loads((server.root / 'providers/pi/lead/session.json').read_text())
        assert lead['state']['messageCount'] == 4
        assert len((tmp_path / 'launches').read_text().splitlines()) == 1
    finally: server.close()

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_ordinary_reply_mailbox_wakes_a_closed_lead_without_a_queued_job(tmp_path, provider):
    server = Server(tmp_path / 'state', {'lead': config(provider, 'idle-exit', tmp_path), 'worker': spec('worker')})
    try:
        before = await_closed(server, provider, 'lead')
        receipt = submit(server, actor='lead')
        await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            jobs = server.rpc('records.latest', dict(stream='jobs', agent_name='lead', identity='job_id'), version=1)['result']
            if jobs and jobs[-1]['status'] == 'completed': break
            time.sleep(.025)
        assert len(jobs) == 1 and jobs[0]['status'] == 'completed', 'reply did not wake the closed Lead'
        assert jobs[0]['request']['message_type'] == 'reply_delivery'
        assert server.rpc('mailbox-head', {'agent_name': 'lead'})['head'] is None
        assert metadata(server, provider, 'lead')['runtime_instance_id'] != before['runtime_instance_id']
        assert len((tmp_path / 'launches').read_text().splitlines()) == 2
    finally: server.close()
