"""Same-turn followups have durable identity, bounded observation and no replay."""
import json
import time
import uuid
import pytest
from test_native_daemon import Server, spec, submit, await_watch
from test_native_codex_rpc import codex_spec
from test_native_cli_security import cli


def start(tmp_path, mode='followup-hold'):
    trace = tmp_path / 'wire.jsonl'
    agent = codex_spec(mode)
    agent['env'] = {'CCB_FIXTURE_TRACE': str(trace)}
    server = Server(tmp_path / 'state', {'worker': agent})
    receipt = submit(server)
    await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
    return server, receipt['job_id'], trace, agent


def admit(server, job, message='additional instruction', operation=None):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        op = operation or str(uuid.uuid4())
        result = server.rpc('followup', {'job_id': job, 'message': message, 'operation_id': op})
        if result['reason'] not in ['active provider turn unbound', 'no active provider turn']:
            return result
        assert operation is None, result
        time.sleep(.025)
    pytest.fail(f'provider turn never bound: {result}')


def settled(server, receipt, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = server.rpc('followup-status', {'followup_id': receipt['followup_id']})
        if result['status'] not in ['accepted', 'sending']:
            return result
        time.sleep(.025)
    pytest.fail(f'followup never settled: {result}')


def wire(path):
    return [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines()]


def test_injected_followup_keeps_job_running_and_deduplicates(tmp_path):
    server, job, trace, _ = start(tmp_path)
    try:
        receipt = admit(server, job)
        done = settled(server, receipt)
        assert done['status'] == 'injected'
        assert 'message' not in done and 'message_sha256' not in done
        assert server.rpc('watch', {'target': job})['status'] == 'running'
        assert server.rpc('records.list', {'stream': 'replies'}, version=1)['result'] == []
        repeated = admit(server, job, operation=receipt['operation_id'])
        assert repeated['status'] == 'injected' and repeated['followup_id'] == done['followup_id']
        with pytest.raises(AssertionError, match='another request'):
            admit(server, job, message='different text', operation=receipt['operation_id'])
        commands = [r for r in wire(trace) if r.get('method') == 'turn/steer']
        assert len(commands) == 1
        assert commands[0]['params']['expectedTurnId'] == done['binding']['turn_id']
        assert commands[0]['params']['input'] == [{'type': 'text', 'text': 'additional instruction'}]
        traced = server.rpc('trace', {'target': done['followup_id']})
        assert traced['resolved_kind'] == 'active_followup' and traced['job_id'] == job
        assert traced['active_followups'][0]['status'] == 'injected'
    finally:
        server.close()


def test_followup_cli_and_changed_final_result_survive_receipt_restart(tmp_path):
    server, job, trace, agent = start(tmp_path, 'followup-finish')
    root = server.root
    try:
        # The fixture records turn/start before returning its bound turn identity.
        deadline = time.monotonic() + 5
        while (not trace.exists() or not any(r.get('method') == 'turn/start' for r in wire(trace))) and time.monotonic() < deadline:
            time.sleep(.025)
        time.sleep(.1)
        result = cli('--state-root', root, 'followup', job, '--message', 'revised final result')
        assert result.returncode == 0, result.stderr
        receipt = json.loads(result.stdout)
        assert receipt['status'] == 'injected'
        assert await_watch(server, job, lambda r: r['terminal'])['reply'] == 'revised final result'
    finally:
        server.close()
    restored = Server(root, {'worker': agent})
    try:
        result = admit(restored, job, 'revised final result', receipt['operation_id'])
        assert result['status'] == 'injected'
        assert len([r for r in wire(trace) if r.get('method') == 'turn/steer']) == 1
        assert admit(restored, job)['status'] == 'too_late'
    finally:
        restored.close()


@pytest.mark.parametrize('mode,status,reason', [
    ('followup-denied', 'rejected', 'provider_steer_rejected'),
    ('followup-mismatch', 'uncertain', 'provider_steer_turn_mismatch'),
    ('followup-no-ack', 'uncertain', 'provider_steer_response_not_observed'),
])
def test_provider_ack_is_required_and_timeout_does_not_restart_task(tmp_path, mode, status, reason):
    server, job, trace, _ = start(tmp_path, mode)
    try:
        receipt = admit(server, job)
        result = settled(server, receipt)
        assert result['status'] == status and result['reason'] == reason
        assert 'SECRET_FOLLOWUP_ERROR' not in json.dumps(result)
        assert server.rpc('watch', {'target': job})['status'] == 'running'
        assert len([r for r in wire(trace) if r.get('method') == 'turn/start']) == 1
    finally:
        server.close()


def test_followups_are_sent_in_admission_order(tmp_path):
    server, job, trace, _ = start(tmp_path, 'followup-delay')
    try:
        receipts = [admit(server, job, message) for message in ['first', 'second', 'third']]
        assert all(settled(server, r)['status'] == 'injected' for r in receipts)
        commands = [r for r in wire(trace) if r.get('method') == 'turn/steer']
        assert [r['params']['input'][0]['text'] for r in commands] == ['first', 'second', 'third']
        traced = server.rpc('trace', {'target': job})
        assert traced['active_followup_count'] >= 3
        assert [r['followup_id'] for r in traced['active_followups'] if r['status'] == 'injected'] == [r['followup_id'] for r in receipts]
    finally:
        server.close()


def test_daemon_crash_after_send_never_replays_uncertain_followup(tmp_path):
    server, job, trace, agent = start(tmp_path, 'followup-no-ack')
    try:
        receipt = admit(server, job)
        deadline = time.monotonic() + 5
        while not any(r.get('method') == 'turn/steer' for r in wire(trace)) and time.monotonic() < deadline:
            time.sleep(.025)
        assert any(r.get('method') == 'turn/steer' for r in wire(trace))
    finally:
        server.proc.kill()
        server.proc.wait(timeout=5)
    restored = Server(server.root, {'worker': agent})
    try:
        result = settled(restored, receipt)
        assert result['status'] == 'uncertain'
        assert result['reason'] == 'provider_binding_lost_after_send'
        assert admit(restored, job, operation=receipt['operation_id'])['status'] == 'uncertain'
        assert len([r for r in wire(trace) if r.get('method') == 'turn/steer']) == 1
    finally:
        restored.close()


def test_queued_and_external_jobs_are_not_steered(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec()})
    try:
        job = submit(server)['job_id']
        assert admit(server, job)['status'] == 'rejected'
        server.rpc('work', {'agent_name': 'worker'})
        assert admit(server, job)['reason'] == 'provider_active_followup_unsupported'
        assert server.rpc('watch', {'target': job})['status'] == 'running'
    finally:
        server.close()


def test_cli_observation_timeout_does_not_duplicate_or_cancel(tmp_path):
    server, job, trace, _ = start(tmp_path, 'followup-delay')
    try:
        first = admit(server, job)
        assert settled(server, first)['status'] == 'injected'
        operation = str(uuid.uuid4())
        result = cli('--state-root', server.root, 'followup', job, '--message', 'second instruction', '--operation-id', operation, '--timeout-ms', '0')
        assert result.returncode != 0
        receipt = json.loads(result.stdout)
        assert receipt['status'] == 'accepted'
        assert settled(server, receipt)['status'] == 'injected'
        assert admit(server, job, 'second instruction', operation)['status'] == 'injected'
        assert len([r for r in wire(trace) if r.get('method') == 'turn/steer']) == 2
        assert server.rpc('watch', {'target': job})['status'] == 'running'
    finally:
        server.close()


def test_cancelled_turn_refuses_new_followup(tmp_path):
    server, job, trace, _ = start(tmp_path)
    try:
        assert settled(server, admit(server, job))['status'] == 'injected'
        server.rpc('cancel', {'job_id': job})
        assert admit(server, job)['status'] in ['rejected', 'too_late']
        await_watch(server, job, lambda r: r['terminal'])
        assert len([r for r in wire(trace) if r.get('method') == 'turn/steer']) == 1
    finally:
        server.close()


@pytest.mark.parametrize('mode,status', [('followup-hold', 'injected'), ('followup-denied', 'rejected'), ('followup-no-ack', 'uncertain')])
def test_compaction_retires_payload_but_preserves_receipt_after_restart(tmp_path, mode, status):
    server, job, trace, agent = start(tmp_path, mode)
    message = 'private payload to retire'
    try:
        receipt = admit(server, job, message)
        done = settled(server, receipt)
        assert done['status'] == status
        path = server.root / 'native-followups.jsonl'
        before = path.read_bytes()
        assert message.encode() in before
        # Populate public trace/index caches before replacing their history.
        server.rpc('trace', {'target': receipt['followup_id']})
        preview = cli('--state-root', server.root, 'followup-compact', '--dry-run')
        assert preview.returncode == 0, preview.stderr
        assert json.loads(preview.stdout)['records_removed'] >= 1
        assert path.read_bytes() == before
        result = cli('--state-root', server.root, 'followup-compact')
        assert result.returncode == 0, result.stderr
        compacted = path.read_bytes()
        assert message.encode() not in compacted and len(compacted) < len(before)
        assert server.rpc('followup-status', {'followup_id': receipt['followup_id']})['status'] == status
        assert server.rpc('trace', {'target': receipt['followup_id']})['active_followups'][0]['status'] == status
        assert not server.rpc('followup-compact')['changed']
        assert path.read_bytes() == compacted
    finally:
        server.close()
    restored = Server(server.root, {'worker': agent})
    try:
        assert admit(restored, job, message, receipt['operation_id'])['status'] == status
        with pytest.raises(AssertionError, match='another request'):
            admit(restored, job, 'different payload', receipt['operation_id'])
        assert len([r for r in wire(trace) if r.get('method') == 'turn/steer']) == 1
    finally:
        restored.close()


def test_compaction_empty_and_invalid_options_do_not_create_history(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec()})
    try:
        assert not server.rpc('followup-compact')['changed']
        for payload in [{'dry_run': 'false'}, {'erase_receipts': True}]:
            with pytest.raises(AssertionError):
                server.rpc('followup-compact', payload)
        result = cli('--state-root', server.root, 'followup-compact', '--unexpected')
        assert result.returncode != 0
        assert not (server.root / 'native-followups.jsonl').exists()
    finally:
        server.close()
