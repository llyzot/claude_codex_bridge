"""Native cancellation stays pending until the owned Codex turn is verified."""
import json
import time

import pytest
from test_native_codex_rpc import codex_spec
from test_native_daemon import Server, submit, await_watch


def cancel_spec(tmp_path, mode='cancel-normal'):
    config = codex_spec(mode)
    config['env'] = {'CCB_FIXTURE_TRACE': str(tmp_path / 'codex-trace.jsonl')}
    return config


def trace_commands(tmp_path):
    path = tmp_path / 'codex-trace.jsonl'
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines() if line]


def await_interrupt(tmp_path):
    deadline = time.monotonic() + 4
    while time.monotonic() < deadline:
        matches = [c for c in trace_commands(tmp_path) if c['method'] == 'turn/interrupt']
        if matches:
            return matches
        time.sleep(.01)
    pytest.fail('provider never received the correlated interrupt')


@pytest.mark.parametrize('mode', ['cancel-normal', 'cancel-late-accept'])
def test_silent_cancel_is_durable_and_keeps_fifo_until_verified_terminal(tmp_path, mode):
    server = Server(tmp_path / 'state', {'worker': cancel_spec(tmp_path, mode)})
    try:
        receipt = submit(server)
        await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
        second = submit(server)
        request = {'job_id': receipt['job_id'], 'record_reply': False}
        assert server.rpc('cancel', request)['status'] == 'cancellation_requested'
        assert server.rpc('cancel', request)['status'] == 'cancellation_requested'
        marked = server.rpc('records.find_last', dict(stream='jobs', agent_name='worker', fields={'job_id': receipt['job_id']}), version=1)['result']
        assert marked['cancel_requested_at']
        assert marked['provider_options']['native_cancel_intent']['record_reply'] is False
        await_interrupt(tmp_path)
        assert server.rpc('watch', {'target': receipt['job_id']})['status'] == 'running'
        assert server.rpc('queue', {'target': 'worker'})['queues'][0]['active_job_id'] == receipt['job_id']
        assert len([c for c in trace_commands(tmp_path) if c['method'] == 'turn/start']) == 1
        result = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        assert result['status'] == 'cancelled'
        assert result['completion_reason'] == 'codex_turn_interrupted'
        await_watch(server, second['job_id'], lambda r: r['terminal'])
        replies = server.rpc('records.list', {'stream': 'replies'}, version=1)['result']
        attempt = server.rpc('records.find_last', dict(stream='attempts', fields={'job_id': second['job_id']}), version=1)['result']
        assert len(replies) == 1 and replies[0]['attempt_id'] == attempt['attempt_id']
        assert len(await_interrupt(tmp_path)) == 1
    finally:
        server.close()


def test_cancel_intent_reconciles_after_daemon_death_without_replay(tmp_path):
    root = tmp_path / 'state'
    agents = {'worker': cancel_spec(tmp_path, 'cancel-ack-only')}
    server = Server(root, agents)
    try:
        receipt = submit(server)
        await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
        server.rpc('cancel', {'job_id': receipt['job_id'], 'record_reply': False})
        await_interrupt(tmp_path)
        server.proc.kill()
        server.proc.wait(timeout=4)
    finally:
        if server.proc.poll() is None:
            server.close()
    restored = Server(root, agents)
    try:
        await_watch(restored, receipt['job_id'], lambda r:r['terminal'])
        row = restored.rpc('records.find_last', dict(stream='jobs', agent_name='worker', fields={'job_id': receipt['job_id']}), version=1)['result']
        assert row['status'] == 'cancelled'
        assert row['provider_options']['native_cancel_intent']['record_reply'] is False
        assert restored.rpc('queue', {'target': 'worker'})['queues'][0]['active_job_id'] is None
        assert len([c for c in trace_commands(tmp_path) if c['method'] == 'turn/start']) == 1
        assert restored.rpc('records.list', {'stream': 'replies'}, version=1)['result'] == []
    finally:
        restored.close()


@pytest.mark.parametrize('mode', ['cancel-ack-only', 'cancel-foreign', 'cancel-denied'])
def test_interrupt_response_or_foreign_completion_never_releases_task(tmp_path, mode):
    server = Server(tmp_path / 'state', {'worker': cancel_spec(tmp_path, mode)})
    try:
        receipt = submit(server)
        await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
        assert server.rpc('cancel', {'job_id': receipt['job_id']})['status'] == 'cancellation_requested'
        await_interrupt(tmp_path)
        time.sleep(.25)
        assert server.proc.poll() is None
        assert server.rpc('watch', {'target': receipt['job_id']})['status'] == 'running'
        assert server.rpc('records.list', {'stream': 'replies'}, version=1)['result'] == []
        metadata = json.loads((server.root / 'providers/codex/worker/session.json').read_text())
        assert metadata['state']['control']['interrupt_acknowledged'] is (mode != 'cancel-denied')
    finally:
        server.close()


def test_deadline_interrupts_matching_turn_without_restarting_provider(tmp_path):
    config = cancel_spec(tmp_path)
    config['timeout_s'] = .15
    server = Server(tmp_path / 'state', {'worker': config})
    try:
        receipt = submit(server)
        row = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        assert row['status'] == 'cancelled'
        latest = server.rpc('records.find_last', dict(stream='jobs', agent_name='worker', fields={'job_id': receipt['job_id']}), version=1)['result']
        diagnostics = latest['terminal_decision']['diagnostics']
        assert diagnostics['deadline_triggered'] is True
        assert latest['provider_options']['native_cancel_intent']['deadline_triggered'] is True
        assert diagnostics['interrupt_acknowledged'] is True
        assert diagnostics['completion_authority'] == 'codex_app_server_turn_completed'
        before = json.loads((server.root / 'providers/codex/worker/session.json').read_text())
        second = submit(server)
        assert await_watch(server, second['job_id'], lambda r: r['terminal'])['status'] == 'completed'
        after = json.loads((server.root / 'providers/codex/worker/session.json').read_text())
        assert before['pid'] == after['pid']
    finally:
        server.close()


def test_natural_completion_preserves_provider_status_and_silent_policy(tmp_path):
    server = Server(tmp_path / 'state', {'worker': cancel_spec(tmp_path, 'cancel-natural-finish')})
    try:
        receipt = submit(server)
        await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
        server.rpc('cancel', {'job_id': receipt['job_id'], 'record_reply': False})
        row = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        assert row['status'] == 'completed'
        assert server.rpc('records.list', {'stream': 'replies'}, version=1)['result'] == []
    finally:
        server.close()


def test_provider_failure_after_cancel_does_not_automatically_restart_task(tmp_path):
    server = Server(tmp_path / 'state', {'worker': cancel_spec(tmp_path, 'cancel-failed-finish')})
    try:
        receipt = submit(server)
        await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
        server.rpc('cancel', {'job_id': receipt['job_id']})
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'failed'
        time.sleep(.2)
        jobs = server.rpc('records.latest', dict(stream='jobs', agent_name='worker', identity='job_id'), version=1)['result']
        assert len(jobs) == 1
        assert len([c for c in trace_commands(tmp_path) if c['method'] == 'turn/start']) == 1
    finally:
        server.close()
