"""Claude cancellation has durable intent and independent terminal evidence."""
import json
from pathlib import Path
import sys
import time

import pytest
from test_native_daemon import Server, submit, await_watch

STUB = Path(__file__).parent / 'stubs' / 'claude_control_worker.py'


def spec(tmp_path, mode='normal'):
    return dict(provider='claude', driver_kind='claude_stream', profile_family='structured_result',
                command=[sys.executable, '-I', str(STUB), mode],
                env={'CCB_FIXTURE_TRACE': str(tmp_path / 'claude-wire.jsonl')})


def controls(tmp_path):
    path = tmp_path / 'claude-wire.jsonl'
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line]


def await_interrupt(tmp_path):
    deadline = time.monotonic() + 4
    while time.monotonic() < deadline:
        if any(row.get('request', {}).get('subtype') == 'interrupt' for row in controls(tmp_path)):
            return
        time.sleep(.01)
    pytest.fail('Claude never received interrupt')


def job(server, job_id):
    return server.rpc('records.find_last', dict(stream='jobs', agent_name='worker', fields={'job_id': job_id}), version=1)['result']


def test_silent_cancel_is_durable_and_result_releases_fifo_same_process(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path)})
    try:
        first = submit(server)
        await_watch(server, first['job_id'], lambda r: r['status'] == 'running')
        second = submit(server)
        payload = {'job_id': first['job_id'], 'record_reply': False}
        server.rpc('cancel', payload)
        server.rpc('cancel', payload)
        await_interrupt(tmp_path)
        marked = job(server, first['job_id'])
        assert marked['provider_options']['native_cancel_intent']['record_reply'] is False
        assert marked['cancel_requested_at']
        assert server.rpc('watch', {'target': first['job_id']})['status'] == 'running'
        assert server.rpc('queue', {'target': 'worker'})['queues'][0]['active_job_id'] == first['job_id']
        before = json.loads((server.root / 'providers/claude/worker/session.json').read_text())
        row = await_watch(server, first['job_id'], lambda r: r['terminal'])
        assert row['status'] == 'cancelled' and row['completion_reason'] == 'claude_interrupted'
        assert await_watch(server, second['job_id'], lambda r: r['terminal'])['status'] == 'completed'
        after = json.loads((server.root / 'providers/claude/worker/session.json').read_text())
        assert before['pid'] == after['pid']
        replies = server.rpc('records.list', {'stream': 'replies'}, version=1)['result']
        assert len(replies) == 1
        assert len([r for r in controls(tmp_path) if r.get('request', {}).get('subtype') == 'interrupt']) == 1
    finally:
        server.close()


@pytest.mark.parametrize('mode', ['ack-only', 'foreign-ack', 'foreign-result', 'denied'])
def test_control_ack_error_or_foreign_result_does_not_finish_task(tmp_path, mode):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, mode)})
    try:
        receipt = submit(server)
        await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
        server.rpc('cancel', {'job_id': receipt['job_id']})
        await_interrupt(tmp_path)
        time.sleep(.15)
        assert server.rpc('watch', {'target': receipt['job_id']})['status'] == 'running'
        assert server.rpc('records.list', {'stream': 'replies'}, version=1)['result'] == []
    finally:
        server.close()


@pytest.mark.parametrize('mode', ['queue-cancel', 'queue-fallback'])
def test_uuid_specific_queue_cancel_completes_before_echo_and_fences_late_old_result(tmp_path, mode):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, mode)})
    try:
        first = submit(server)
        await_watch(server, first['job_id'], lambda r: r['status'] == 'running')
        server.rpc('cancel', {'job_id': first['job_id'], 'record_reply': False})
        row = await_watch(server, first['job_id'], lambda r: r['terminal'])
        assert row['status'] == 'cancelled' and row['completion_reason'] == 'claude_queued_cancelled'
        second = submit(server)
        await_watch(server, second['job_id'], lambda r: r['status'] == 'running')
        time.sleep(.15)
        assert server.rpc('watch', {'target': second['job_id']})['status'] == 'running'
        assert await_watch(server, second['job_id'], lambda r: r['terminal'])['reply'] == 'Claude completed normally'
        assert job(server, first['job_id'])['terminal_decision']['diagnostics']['completion_authority'] == 'claude_control_queue_cancel_receipt'
        requests = [r['request']['subtype'] for r in controls(tmp_path) if r['type'] == 'control_request']
        assert requests == (['interrupt', 'cancel_async_message'] if mode == 'queue-fallback' else ['interrupt'])
    finally:
        server.close()


def test_deadline_is_durable_and_waits_for_result(tmp_path):
    configured = spec(tmp_path)
    configured['timeout_s'] = .15
    server = Server(tmp_path / 'state', {'worker': configured})
    try:
        receipt = submit(server)
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'cancelled'
        row = job(server, receipt['job_id'])
        assert row['provider_options']['native_cancel_intent']['deadline_triggered'] is True
        assert row['terminal_decision']['diagnostics']['deadline_triggered'] is True
    finally:
        server.close()


@pytest.mark.parametrize('mode,expected', [('natural', 'completed'), ('failed', 'failed')])
def test_result_outcome_is_preserved_after_cancel_and_failure_is_not_retried(tmp_path, mode, expected):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, mode)})
    try:
        receipt = submit(server)
        await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
        server.rpc('cancel', {'job_id': receipt['job_id']})
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == expected
        time.sleep(.15)
        rows = server.rpc('records.latest', dict(stream='jobs', agent_name='worker', identity='job_id'), version=1)['result']
        assert len(rows) == 1
    finally:
        server.close()


def test_confirmed_process_exit_waits_for_eof_and_keeps_daemon_available(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, 'exit')})
    try:
        receipt = submit(server)
        await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
        server.rpc('cancel', {'job_id': receipt['job_id'], 'record_reply': False})
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['completion_reason'] == 'claude_process_exit'
        assert server.rpc('ping')['runtime'] == 'rust'
        assert server.rpc('records.list', {'stream': 'replies'}, version=1)['result'] == []
    finally:
        server.close()


def test_cancel_intent_reconciles_after_daemon_death_without_replay(tmp_path):
    root = tmp_path / 'state'
    configured = {'worker': spec(tmp_path, 'ack-only')}
    first = Server(root, configured)
    try:
        receipt = submit(first)
        await_watch(first, receipt['job_id'], lambda r: r['status'] == 'running')
        first.rpc('cancel', {'job_id': receipt['job_id'], 'record_reply': False})
        await_interrupt(tmp_path)
        first.proc.kill()
        first.proc.wait(timeout=4)
    finally:
        if first.proc.poll() is None:
            first.close()
    restored = Server(root, configured)
    try:
        assert await_watch(restored, receipt['job_id'], lambda r:r['terminal'])['status'] == 'cancelled'
        assert job(restored, receipt['job_id'])['provider_options']['native_cancel_intent']['record_reply'] is False
        assert restored.rpc('queue', {'target': 'worker'})['queues'][0]['active_job_id'] is None
        assert len([r for r in controls(tmp_path) if r['type'] == 'user']) == 1
        assert restored.rpc('records.list', {'stream': 'replies'}, version=1)['result'] == []
    finally:
        restored.close()


def test_result_attribution_requirement_survives_repeated_clean_restarts(tmp_path):
    root = tmp_path / 'state'
    configured = {'worker': spec(tmp_path, 'queue-cancel')}
    first = Server(root, configured)
    try:
        receipt = submit(first)
        await_watch(first, receipt['job_id'], lambda r: r['status'] == 'running')
        first.rpc('cancel', {'job_id': receipt['job_id'], 'record_reply': False})
        await_watch(first, receipt['job_id'], lambda r: r['terminal'])
    finally:
        first.close()
    for _ in range(2):
        path = root / 'providers/claude/worker/session.json'
        prior_runtime = json.loads(path.read_text())['runtime_instance_id']
        restored = Server(root, configured)
        try:
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                assert restored.proc.poll() is None, restored.proc.stderr.read()
                try:
                    metadata = json.loads(path.read_text())
                except OSError:
                    # Atomic Windows replacement can transiently reject an open.
                    time.sleep(.025)
                    continue
                if metadata['runtime_instance_id'] != prior_runtime:
                    break
                time.sleep(.025)
            assert metadata['runtime_instance_id'] != prior_runtime
            assert metadata['state']['require_result_identity'] is True
        finally:
            restored.close()


def test_valid_result_survives_process_exit_behind_bounded_output_backlog(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, 'exit-buffered-result')})
    try:
        receipt = submit(server)
        row = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        assert row['status'] == 'completed'
        assert row['reply'] == 'Claude completed normally'
        assert server.rpc('ping')['runtime'] == 'rust'
    finally:
        server.close()
