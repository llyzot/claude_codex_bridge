"""Authoritative faults isolate one provider; uncertain live work is never replayed."""
import json
from pathlib import Path
import sys
import time

import pytest
from test_native_daemon import Server, spec, submit, await_watch
from test_native_pi_rpc import pi_spec
from test_native_cli_security import cli

STUB = Path(__file__).parent / 'stubs/native_rpc_fault.py'
SECRET = 'do-not-persist-protocol-error-secret'

def provider_spec(provider, mode, directory):
    return dict(provider=provider, driver_kind=dict(pi='pi_rpc', codex='codex_app_server', claude='claude_stream')[provider],
                profile_family='protocol_turn', command=[sys.executable, '-I', str(STUB), provider, mode, str(directory)])

def status(server, agent='worker'): return server.rpc('provider-status', {'agent_name': agent})

def fault(server, agent='worker'):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        assert server.proc.poll() is None, 'one bad provider killed the authority daemon'
        row = status(server, agent)
        if row['fault'] is not None: return row['fault']
        time.sleep(.025)
    pytest.fail('provider fault was not recorded')

def recover(server, record):
    return server.rpc('provider-recover', dict(agent_name=record['agent_name'], fault_id=record['fault_id']))

def healthy_task(server):
    receipt = submit(server, 'healthy')
    assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'completed'

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_spawn_failure_isolated_reported_and_recovered_without_daemon_restart(tmp_path, provider):
    record = provider_spec(provider, 'normal', tmp_path)
    record['workspace_path'] = str(tmp_path / 'missing-directory')
    server = Server(tmp_path / 'state', {'worker': record, 'healthy': spec('worker')})
    try:
        receipt = submit(server)
        time.sleep(.2)
        assert server.proc.poll() is None, 'spawn failure killed the authority daemon'
        healthy_task(server)
        row = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        assert row['status'] == 'failed' and row['completion_reason'] == 'provider_start_failed'
        before = fault(server)
        assert before['pid'] is None and before['phase'] == 'startup'
        assert not (tmp_path / 'fault-launches').exists()
        record_path = server.root / f'providers/{provider}/worker/fault.json'
        assert json.loads(record_path.read_text())['fault_id'] == before['fault_id']
        with pytest.raises(AssertionError, match='provider is faulted'):
            submit(server)
        (tmp_path / 'missing-directory').mkdir()
        with pytest.raises(AssertionError, match='fault identity mismatch'):
            server.rpc('provider-recover', dict(agent_name='worker', fault_id='foreign'))
        assert recover(server, before)['status'] == 'recovered'
        receipt = submit(server)
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['reply'] == 'fault fixture result'
        assert status(server)['fault'] is None
        assert server.rpc('ping')['pid'] == server.proc.pid
    finally: server.close()

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_invalid_initialization_does_not_stop_other_agents_or_restart_live_provider(tmp_path, provider):
    server = Server(tmp_path / 'state', {'worker': provider_spec(provider, 'init', tmp_path), 'healthy': spec('worker')})
    try:
        deadline = time.monotonic() + 8
        while not (tmp_path / 'fault-emitted').exists() and time.monotonic() < deadline: time.sleep(.025)
        time.sleep(.15)
        assert server.proc.poll() is None, 'invalid initialization killed the authority daemon'
        healthy_task(server)
        before = fault(server)
        assert before['reason'] == 'provider_protocol_error'
        assert before['pid'] is not None
        with pytest.raises(AssertionError, match='not confirmed closed'):
            recover(server, before)
        assert len((tmp_path / 'fault-launches').read_text().splitlines()) == 1
        assert SECRET not in json.dumps(before)
        (tmp_path / 'release-fault').touch()
        deadline = time.monotonic() + 8
        while status(server)['process_state'] != 'closed' and time.monotonic() < deadline: time.sleep(.025)
        recover(server, before)
        receipt = submit(server)
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['reply'] == 'fault fixture result'
    finally:
        (tmp_path / 'release-fault').touch()
        server.close()

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_blocked_startup_fault_survives_clean_restart_until_explicit_recovery(tmp_path, provider):
    record = provider_spec(provider, 'normal', tmp_path)
    record['workspace_path'] = str(tmp_path / 'missing-directory')
    agents = {'worker': record, 'healthy': spec('worker')}
    root = tmp_path / 'state'
    server = Server(root, agents)
    try: before = fault(server)
    finally: server.close()
    restored = Server(root, agents)
    try:
        assert fault(restored)['fault_id'] == before['fault_id']
        assert restored.rpc('ping')['degraded'] is True
        healthy_task(restored)
        assert not (tmp_path / 'fault-launches').exists()
        (tmp_path / 'missing-directory').mkdir()
        recover(restored, before)
        receipt = submit(restored)
        assert await_watch(restored, receipt['job_id'], lambda r: r['terminal'])['reply'] == 'fault fixture result'
        assert restored.rpc('ping')['degraded'] is False
    finally: restored.close()

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_preaccepted_fifo_tasks_fail_without_sending_prompts_after_active_fault(tmp_path, provider):
    server = Server(tmp_path / 'state', {'worker': provider_spec(provider, 'active-delayed', tmp_path), 'healthy': spec('worker')})
    try:
        first = submit(server)
        await_watch(server, first['job_id'], lambda r: r['status'] == 'running')
        second = submit(server, body='second preaccepted request')
        (tmp_path / 'release-error').touch()
        fault(server)
        assert server.rpc('watch', {'target': second['job_id']})['status'] == 'queued'
        healthy_task(server)
        (tmp_path / 'release-fault').touch()
        assert await_watch(server, first['job_id'], lambda r: r['terminal'])['status'] == 'failed'
        assert await_watch(server, second['job_id'], lambda r: r['terminal'])['status'] == 'failed'
        rows = server.rpc('records.latest', dict(stream='jobs', agent_name='worker', identity='job_id'), version=1)['result']
        assert len(rows) == 2
        pending = next(row for row in rows if row['job_id'] == second['job_id'])
        assert pending['terminal_decision']['diagnostics']['provider_prompt_sent'] is False
        assert len((tmp_path / 'fault-launches').read_text().splitlines()) == 1
    finally:
        (tmp_path / 'release-error').touch()
        (tmp_path / 'release-fault').touch()
        server.close()

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_provider_protocol_failure_returns_result_and_resumes_same_pi_lead(tmp_path, provider):
    server = Server(tmp_path / 'state', {'lead': pi_spec('lead'), 'worker': provider_spec(provider, 'active', tmp_path)})
    try:
        receipt = submit(server, 'lead')
        fault(server)
        (tmp_path / 'release-fault').touch()
        row = await_watch(server, receipt['job_id'], lambda r: r['visible_reply_source'] == 'message_bureau_reply')
        assert row['reply'] == 'persistent Pi reviewed result'
        edges = server.rpc('records.latest', dict(stream='callbacks', identity='edge_id'), version=1)['result']
        assert len(edges) == 1 and edges[0]['state'] == 'done'
        lead = json.loads((server.root / 'providers/pi/lead/session.json').read_text())
        assert lead['state']['messageCount'] == 4
        assert SECRET not in json.dumps(server.rpc('records.list', {'stream': 'replies'}, version=1))
        for path in tmp_path.rglob('*.json*'):
            assert SECRET not in path.read_text(encoding='utf-8'), str(path)
    finally:
        (tmp_path / 'release-fault').touch()
        server.close()

def test_native_cli_exposes_status_and_identity_fenced_recovery(tmp_path):
    record = provider_spec('codex', 'normal', tmp_path)
    record['workspace_path'] = str(tmp_path / 'missing-directory')
    server = Server(tmp_path / 'state', {'worker': record})
    try:
        before = fault(server)
        result = cli('--state-root', server.root, 'provider-status', 'worker')
        assert result.returncode == 0, result.stderr
        assert json.loads(result.stdout)['fault']['fault_id'] == before['fault_id']
        (tmp_path / 'missing-directory').mkdir()
        result = cli('--state-root', server.root, 'provider-recover', 'worker', before['fault_id'])
        assert result.returncode == 0, result.stderr
        assert json.loads(result.stdout)['status'] == 'recovered'
        receipt = submit(server)
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['reply'] == 'fault fixture result'
    finally: server.close()

def test_recovery_rechecks_durable_fault_instead_of_clearing_changed_identity(tmp_path):
    record = provider_spec('codex', 'normal', tmp_path)
    record['workspace_path'] = str(tmp_path / 'missing-directory')
    server = Server(tmp_path / 'state', {'worker': record})
    try:
        before = fault(server)
        path = server.root / 'providers/codex/worker/fault.json'
        changed = dict(before, phase='start')
        path.write_text(json.dumps(changed), encoding='utf-8')
        with pytest.raises(AssertionError, match='durable provider fault identity changed'):
            recover(server, before)
        assert json.loads(path.read_text()) == changed
        assert not (tmp_path / 'fault-launches').exists()
    finally: server.close()

def test_legacy_faulted_active_execution_without_proof_remains_fenced_after_daemon_death(tmp_path):
    root = tmp_path / 'state'
    agents = {'worker': provider_spec('codex', 'active', tmp_path), 'healthy': spec('worker')}
    server = Server(root, agents)
    try:
        receipt = submit(server)
        before = fault(server)
        legacy = server.rpc('records.latest', dict(stream='jobs', agent_name='worker', identity='job_id'), version=1)['result'][0]
        legacy['provider_options'].pop('native_scope_proof', None)
        server.rpc('records.append', dict(stream='jobs', agent_name='worker', record=legacy), version=1)

    finally:
        server.proc.kill(); server.proc.wait(timeout=5)
    restored = Server(root, agents)
    try:
        assert fault(restored)['fault_id'] == before['fault_id']
        assert restored.rpc('watch', {'target': receipt['job_id']})['status'] == 'running'
        with pytest.raises(AssertionError, match='active execution'):
            recover(restored, before)
        healthy_task(restored)
        assert len((tmp_path / 'fault-launches').read_text().splitlines()) == 1
    finally:
        (tmp_path / 'release-fault').touch()
        restored.close()

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_faulted_lead_retains_result_mailbox_head_until_recovery(tmp_path, provider):
    record = provider_spec(provider, 'idle-late-fault', tmp_path)
    server = Server(tmp_path / 'state', {'lead': record, 'worker': spec('slow-input')})
    try:
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            owned = status(server, 'lead')['owned']
            if owned and owned['ready']: break
            time.sleep(.025)
        assert owned and owned['ready']
        # Delegate while the Lead is healthy; a quarantined caller cannot ask anew.
        receipt = submit(server, actor='lead')
        (tmp_path / 'release-error').touch()
        before = fault(server, 'lead')
        (tmp_path / 'release-fault').touch()
        deadline = time.monotonic() + 8
        while status(server, 'lead')['process_state'] != 'closed' and time.monotonic() < deadline: time.sleep(.025)
        await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        head = server.rpc('mailbox-head', {'agent_name': 'lead'})['head']
        assert head is not None
        time.sleep(.2)
        assert server.rpc('mailbox-head', {'agent_name': 'lead'})['head']['inbound_event_id'] == head['inbound_event_id']
        prior = server.rpc('records.latest', dict(stream='jobs', agent_name='lead', identity='job_id'), version=1)['result']
        assert prior == []
        recover(server, before)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            jobs = server.rpc('records.latest', dict(stream='jobs', agent_name='lead', identity='job_id'), version=1)['result']
            if len(jobs) == 1 and jobs[-1]['status'] == 'completed': break
            time.sleep(.025)
        assert len(jobs) == 1 and jobs[-1]['status'] == 'completed'
        assert jobs[-1]['request']['message_type'] == 'reply_delivery'
        assert server.rpc('mailbox-head', {'agent_name': 'lead'})['head'] is None
    finally:
        (tmp_path / 'release-error').touch()
        (tmp_path / 'release-fault').touch()
        server.close()

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
@pytest.mark.parametrize('mode', ['active', 'malformed'])
def test_active_protocol_fault_fences_live_execution_and_completes_only_after_exit(tmp_path, provider, mode):
    server = Server(tmp_path / 'state', {'worker': provider_spec(provider, mode, tmp_path), 'healthy': spec('worker')})
    try:
        receipt = submit(server)
        await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
        before = fault(server)
        healthy_task(server)
        assert server.rpc('watch', {'target': receipt['job_id']})['status'] == 'running'
        with pytest.raises(AssertionError, match='active execution'):
            recover(server, before)
        assert len((tmp_path / 'fault-launches').read_text().splitlines()) == 1
        (tmp_path / 'release-fault').touch()
        row = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        assert row['status'] == 'failed' and row['completion_reason'] == 'provider_protocol_error'
        assert SECRET not in json.dumps(row)
        assert len((tmp_path / 'fault-launches').read_text().splitlines()) == 1
        recover(server, before)
        receipt = submit(server)
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['reply'] == 'fault fixture result'
        jobs = server.rpc('records.latest', dict(stream='jobs', agent_name='worker', identity='job_id'), version=1)['result']
        assert len(jobs) == 2
    finally:
        (tmp_path / 'release-fault').touch()
        server.close()
