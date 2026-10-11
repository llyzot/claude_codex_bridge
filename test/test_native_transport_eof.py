"""Observable EOF is a durable fault, not completion or replay permission."""
import json
import sys
import time
import pytest
from test_native_daemon import Server, spec, submit, await_watch
from test_native_provider_faults import provider_spec, status, recover


def config(provider, mode, root):
    value = provider_spec(provider, mode, root)
    # Avoid a Windows venv redirector retaining the interpreter's stdout handle.
    value['command'][0] = sys._base_executable
    value['resume_session'] = False
    return value


def wait_for(predicate, timeout=5):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        value = predicate()
        if value: return value
        time.sleep(.025)
    pytest.fail('expected EOF transition was not observed')


def fault_record(server, provider, agent='worker'):
    path = server.root / f'providers/{provider}/{agent}/fault.json'
    wait_for(path.exists)
    value = json.loads(path.read_text(encoding='utf-8'))
    assert value['phase'] == 'poll' and value['transport_closed'] == 'stdout'
    return value


def ready(server, agent='worker'):
    # Claude can accept input before its init frame arrives. This scenario
    # specifically closes an already initialized, idle protocol connection.
    def observed():
        row = server.rpc('ping', {'target': agent})['diagnostics']
        return row.get('protocol_observed') is True and row.get('protocol_idle_last_observed') is True
    wait_for(observed)


@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_idle_eof_publishes_fault_and_requires_explicit_recovery(tmp_path, provider):
    server = Server(tmp_path / 'state', {'worker': config(provider, 'idle-eof', tmp_path)})
    try:
        ready(server)
        (tmp_path / 'release-error').touch()
        record = fault_record(server, provider)
        assert record['job_id'] is None and status(server)['owned']['closed'] is False
        with pytest.raises(AssertionError, match='faulted'): submit(server)
        with pytest.raises(AssertionError, match='not confirmed closed'): recover(server, record)
        assert len((tmp_path / 'fault-launches').read_text().splitlines()) == 1
        (tmp_path / 'release-fault').touch()
        wait_for(lambda: status(server)['process_state'] == 'closed')
        recover(server, record)
        result = submit(server)
        assert await_watch(server, result['job_id'], lambda r: r['terminal'])['status'] == 'completed'
    finally:
        (tmp_path / 'release-error').touch(); (tmp_path / 'release-fault').touch()
        server.close()


@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_active_eof_preserves_uncertain_execution_and_pending_fifo(tmp_path, provider):
    server = Server(tmp_path / 'state', {'worker': config(provider, 'active-eof', tmp_path)})
    try:
        first = submit(server)['job_id']
        wait_for(lambda: (tmp_path / 'prompt-seen').exists())
        second = submit(server, body='not yet sent')['job_id']
        (tmp_path / 'release-error').touch()
        record = fault_record(server, provider)
        assert record['job_id'] == first
        assert server.rpc('watch', {'target': first})['status'] == 'running'
        assert server.rpc('watch', {'target': second})['status'] == 'queued'
        assert server.rpc('records.list', {'stream': 'replies'}, version=1)['result'] == []
        with pytest.raises(AssertionError, match='active execution'): recover(server, record)
        (tmp_path / 'release-fault').touch()
        assert await_watch(server, first, lambda r: r['terminal'])['status'] == 'failed'
        assert await_watch(server, second, lambda r: r['terminal'])['status'] == 'failed'
        jobs = server.rpc('records.latest', {'stream': 'jobs', 'agent_name': 'worker', 'identity': 'job_id'}, version=1)['result']
        assert next(j for j in jobs if j['job_id'] == second)['terminal_decision']['diagnostics']['provider_prompt_sent'] is False
        assert len((tmp_path / 'fault-launches').read_text().splitlines()) == 1
    finally:
        (tmp_path / 'release-error').touch(); (tmp_path / 'release-fault').touch()
        server.close()


@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
@pytest.mark.parametrize('mode', ['idle-eof', 'completed-eof'])
def test_completed_result_is_not_rewritten_by_later_eof(tmp_path, provider, mode):
    server = Server(tmp_path / 'state', {'worker': config(provider, mode, tmp_path)})
    try:
        receipt = submit(server)
        before = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        assert before['status'] == 'completed'
        (tmp_path / 'release-error').touch()
        fault_record(server, provider)
        after = server.rpc('watch', {'target': receipt['job_id']})
        assert after['status'] == before['status'] and after['reply'] == before['reply']
    finally:
        (tmp_path / 'release-error').touch(); (tmp_path / 'release-fault').touch()
        server.close()


@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_idle_eof_fault_survives_daemon_restart_without_prompt_replay(tmp_path, provider):
    agents = {'worker': config(provider, 'idle-eof', tmp_path)}
    root = tmp_path / 'state'; server = Server(root, agents)
    try:
        ready(server)
        (tmp_path / 'release-error').touch(); record = fault_record(server, provider)
    finally:
        (tmp_path / 'release-error').touch(); (tmp_path / 'release-fault').touch(); server.close()
    restored = Server(root, agents)
    try:
        observed = status(restored)['fault']
        assert observed['fault_id'] == record['fault_id'] and observed['transport_closed'] == 'stdout'
        with pytest.raises(AssertionError, match='faulted'): submit(restored)
        assert len((tmp_path / 'fault-launches').read_text().splitlines()) == 1
    finally:
        restored.close()


@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_idle_receiver_eof_keeps_reply_until_explicit_recovery(tmp_path, provider):
    server = Server(tmp_path / 'state', {'lead': config(provider, 'idle-eof', tmp_path), 'worker': spec('slow-input')})
    try:
        ready(server, 'lead')
        receipt = submit(server, actor='lead')
        (tmp_path / 'release-error').touch()
        record = fault_record(server, provider, 'lead')
        await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        head = server.rpc('inbox', {'agent_name': 'lead'})['head']
        assert head['event_type'] == 'task_reply'
        assert server.rpc('records.list', {'stream': 'jobs', 'agent_name': 'lead'}, version=1)['result'] == []
        (tmp_path / 'release-fault').touch()
        wait_for(lambda: status(server, 'lead')['process_state'] == 'closed')
        recover(server, record)
        wait_for(lambda: server.rpc('inbox', {'agent_name': 'lead'})['item_count'] == 0)
        jobs = server.rpc('records.latest', {'stream': 'jobs', 'agent_name': 'lead', 'identity': 'job_id'}, version=1)['result']
        assert len(jobs) == 1 and jobs[0]['request']['message_type'] == 'reply_delivery' and jobs[0]['status'] == 'completed'
    finally:
        (tmp_path / 'release-error').touch(); (tmp_path / 'release-fault').touch()
        server.close()


@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_failed_first_stdin_write_is_reported_without_another_enqueue(tmp_path, provider):
    server = Server(tmp_path / 'state', {'worker': config(provider, 'stdin-closed', tmp_path)})
    try:
        ready(server)
        wait_for(lambda: (tmp_path / 'stdin-closed').exists())
        receipt = submit(server, body='x' * 4090)
        path = server.root / f'providers/{provider}/worker/fault.json'
        wait_for(path.exists)
        record = json.loads(path.read_text(encoding='utf-8'))
        assert record['transport_closed'] == 'stdin' and record['job_id'] == receipt['job_id']
        assert server.rpc('watch', {'target': receipt['job_id']})['status'] == 'running'
        with pytest.raises(AssertionError, match='faulted'): submit(server)
        with pytest.raises(AssertionError, match='active execution'): recover(server, record)
        assert len((tmp_path / 'fault-launches').read_text().splitlines()) == 1
        (tmp_path / 'release-fault').touch()
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'failed'
        recover(server, record)
        new = submit(server)
        assert await_watch(server, new['job_id'], lambda r: r['terminal'])['status'] == 'completed'
    finally:
        (tmp_path / 'release-fault').touch()
        server.close()
