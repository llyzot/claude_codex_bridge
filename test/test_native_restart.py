"""Provider restarts use disposable services; no real model or user workspace."""
import json
import os
import time
import uuid
from pathlib import Path

import pytest

from test_native_daemon import Server, spec, submit, await_watch
from test_native_cli_security import cli
from test_native_pi_rpc import pi_spec
from test_native_codex_rpc import codex_spec
from test_native_claude_stream import claude_spec


def ready(server, name='worker', previous=None):
    until = time.monotonic() + 8
    while time.monotonic() < until:
        row = server.rpc('provider-status', {'agent_name': name})['owned']
        if row and row['ready'] and row['runtime_instance_id'] != previous:
            return row
        time.sleep(.02)
    pytest.fail(f'provider did not become ready: {row}')


def settled(server, receipt):
    until = time.monotonic() + 8
    while time.monotonic() < until:
        row = server.rpc('restart-status', {'operation_id': receipt['operation_id']})['reload']
        if row['status'] not in {'preparing', 'retiring'}:
            return row
        time.sleep(.02)
    pytest.fail(f'restart did not settle: {row}')


def restart(server, names=None, **kwargs):
    return server.rpc('restart', dict(agent_names=names or ['worker'], **kwargs))['reload']


@pytest.mark.skipif(os.name != 'nt', reason='owned process tree retirement requires Windows')
@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_restart_replaces_only_target_and_retains_history_and_daemon(tmp_path, provider):
    factories = {'pi': lambda: pi_spec('worker'), 'codex': codex_spec, 'claude': claude_spec}
    server = Server(tmp_path/'state', {'worker': factories[provider](), 'keep': pi_spec('worker')})
    try:
        job = submit(server)['job_id']
        await_watch(server, job, lambda r: r['terminal'])
        old, keep = ready(server), ready(server, 'keep')
        before = server.rpc('ping')
        receipt = restart(server)
        assert receipt['action'] == 'restart'
        assert receipt['plan']['changed'] == []
        assert receipt['plan']['restarted'] == ['worker']
        assert settled(server, receipt)['status'] == 'applied'
        replacement = ready(server, previous=old['runtime_instance_id'])
        from native_kernel import _owner_stopped
        assert _owner_stopped(old['pid'])
        assert replacement['runtime_instance_id'] != old['runtime_instance_id']
        assert ready(server, 'keep')['runtime_instance_id'] == keep['runtime_instance_id']
        after = server.rpc('ping')
        assert after['pid'] == before['pid']
        assert after['config_fingerprint'] == before['config_fingerprint']
        assert server.rpc('watch', {'target': job})['terminal']
        await_watch(server, submit(server)['job_id'], lambda r: r['terminal'])
    finally:
        server.close()


def test_saved_pi_session_and_idempotent_receipt_survive_daemon_restart(tmp_path):
    worker = pi_spec('persist')
    worker['command'].append(str(tmp_path/'pi-session.json'))
    agents = {'worker': worker}
    server = Server(tmp_path/'state', agents)
    operation = str(uuid.uuid4())
    try:
        await_watch(server, submit(server)['job_id'], lambda r: r['terminal'])
        before = json.loads((server.root/'providers/pi/worker/session.json').read_text())
        receipt = restart(server, operation_id=operation)
        final = settled(server, receipt)
        assert final['status'] == 'applied'
        replacement = ready(server, previous=before['runtime_instance_id'])
        await_watch(server, submit(server)['job_id'], lambda r: r['terminal'])
        after = json.loads((server.root/'providers/pi/worker/session.json').read_text())
        assert after['state']['sessionId'] == before['state']['sessionId']
        assert after['state']['messageCount'] == 4
        assert restart(server, operation_id=operation) == final
        assert ready(server)['runtime_instance_id'] == replacement['runtime_instance_id']
        with pytest.raises(AssertionError, match='other targets'):
            restart(server, ['all'], operation_id=operation)
    finally:
        server.close()
    restored = Server(server.root, agents)
    try:
        current = ready(restored)
        assert restart(restored, operation_id=operation) == final
        assert ready(restored)['runtime_instance_id'] == current['runtime_instance_id']
        assert restored.rpc('restart-status')['reload'] == final
    finally:
        restored.close()


def test_all_and_role_selection_dry_run_and_unsupported_targets(tmp_path):
    worker = pi_spec('worker'); worker['role'] = 'example.worker'
    disabled = pi_spec('worker'); disabled['dispatch_disabled'] = True
    server = Server(tmp_path/'state', {'worker': worker, 'external': spec(), 'disabled': disabled})
    try:
        before = ready(server)
        selected = server.rpc('restart', {'agent_names': ['example.worker'], 'dry_run': True})['plan']
        assert selected['restarted'] == ['worker'] and selected['can_apply']
        all_plan = server.rpc('restart', {'dry_run': True})['plan']
        assert all_plan['restarted'] == ['worker']
        assert set(all_plan['skipped']) == {'external', 'disabled'}
        assert not (server.root/'runtime-controls.jsonl').exists()
        assert ready(server)['runtime_instance_id'] == before['runtime_instance_id']
        for names in [['all', 'worker'], [''], ['external'], ['disabled'], ['unknown']]:
            with pytest.raises(AssertionError): restart(server, names)
        final = settled(server, server.rpc('project_restart_panes')['reload'])
        assert final['status'] == 'applied'
    finally:
        server.close()


def test_running_and_queued_work_block_restart_without_stopping_provider(tmp_path):
    server = Server(tmp_path/'state', {'worker': pi_spec('hang')})
    try:
        first = submit(server)['job_id']
        await_watch(server, first, lambda r: r['status'] == 'running')
        second = submit(server)['job_id']
        old = server.rpc('provider-status', {'agent_name': 'worker'})['owned']
        result = restart(server)
        assert result['status'] == 'blocked'
        assert {'unfinished_job', 'pending_mailbox'} <= {b['reason'] for b in result['plan']['blockers']}
        assert server.rpc('provider-status', {'agent_name': 'worker'})['owned']['runtime_instance_id'] == old['runtime_instance_id']
        assert server.rpc('watch', {'target': first})['status'] == 'running'
        assert not server.rpc('watch', {'target': second})['terminal']
        assert not (server.root/'runtime-controls.jsonl').exists()
    finally:
        server.close()


def test_idle_lead_waiting_for_result_is_protected(tmp_path):
    server = Server(tmp_path/'state', {'lead': pi_spec('worker'), 'worker': spec()})
    try:
        old = ready(server, 'lead')
        submit(server, actor='lead')
        result = restart(server, ['lead'])
        assert result['status'] == 'blocked'
        assert any(b['reason'] == 'outstanding_reply' for b in result['plan']['blockers'])
        assert ready(server, 'lead')['runtime_instance_id'] == old['runtime_instance_id']
    finally:
        server.close()


def test_open_callback_blocks_idle_lead_then_continues_automatically(tmp_path):
    server = Server(tmp_path/'state', {'lead': pi_spec('lead'), 'worker': spec()})
    try:
        job = submit(server, 'lead')['job_id']
        until = time.monotonic() + 5
        while time.monotonic() < until:
            edges = server.rpc('records.latest', dict(stream='callbacks', identity='edge_id'), version=1)['result']
            if edges: break
            time.sleep(.02)
        assert edges
        result = restart(server, ['lead'])
        assert result['status'] == 'blocked'
        assert any(b['reason'] == 'open_callback' for b in result['plan']['blockers'])
        work = server.rpc('work', {'agent_name': 'worker'})['job']
        from test_native_project_view import finish
        finish(server, work)
        row = await_watch(server, job, lambda r: r['visible_reply_source'] == 'message_bureau_reply')
        assert row['reply'] == 'persistent Pi reviewed result'
    finally:
        server.close()


def test_provider_fault_is_not_discarded_by_restart(tmp_path):
    from test_native_provider_faults import fault
    worker = pi_spec('worker'); worker['command'] = [str(tmp_path/'absent.exe')]
    server = Server(tmp_path/'state', {'worker': worker})
    try:
        observed = fault(server)
        result = restart(server)
        assert result['status'] == 'blocked'
        assert any(b['reason'] == 'provider_fault' for b in result['plan']['blockers'])
        assert server.rpc('provider-status', {'agent_name': 'worker'})['fault']['fault_id'] == observed['fault_id']
    finally:
        server.close()


def test_cli_wait_timeout_keeps_one_queryable_operation_and_replay(tmp_path):
    server = Server(tmp_path/'state', {'worker': pi_spec('worker')})
    try:
        ready(server)
        operation = str(uuid.uuid4())
        result = cli('--state-root', server.root, 'restart', 'worker', '--operation-id', operation,
                     '--wait', '--timeout-ms', '0')
        assert result.returncode == 0, result.stderr
        response = json.loads(result.stdout)
        assert response['observation_timeout']
        assert response['reload']['operation_id'] == operation
        assert settled(server, response['reload'])['status'] == 'applied'
        current = ready(server)
        retry = cli('--state-root', server.root, 'restart', 'worker', '--operation-id', operation, '--wait')
        assert retry.returncode == 0, retry.stderr
        assert json.loads(retry.stdout)['reload']['status'] == 'applied'
        assert ready(server)['runtime_instance_id'] == current['runtime_instance_id']
        status = cli('--state-root', server.root, 'restart-status', operation)
        assert status.returncode == 0 and json.loads(status.stdout)['reload']['status'] == 'applied'
        assert cli('--state-root', server.root, 'restart', '--wat').returncode != 0
    finally:
        server.close()


def test_cli_reports_blocked_as_error_with_structured_receipt(tmp_path):
    server = Server(tmp_path/'state', {'lead': pi_spec('worker'), 'worker': spec()})
    try:
        ready(server, 'lead'); submit(server, actor='lead')
        result = cli('--state-root', server.root, 'restart', 'lead')
        assert result.returncode != 0
        assert json.loads(result.stdout)['reload']['status'] == 'blocked'
        assert server.rpc('restart-status')['reload'] is None
    finally:
        server.close()


@pytest.mark.parametrize('phase', ['preparing', 'retiring'])
def test_interrupted_intent_is_recorded_and_same_id_never_reapplies(tmp_path, phase):
    agents = {'worker': pi_spec('worker')}
    first = Server(tmp_path/'state', agents)
    first.close()
    operation = str(uuid.uuid4())
    # Simulate a committed receipt at the crash boundary before the next actor tick.
    record = dict(schema_version=1, record_type='native_reload', action='restart',
                  operation_id=operation, status=phase, requested_agents=['worker'])
    (first.root/'reload.json').write_text(json.dumps(record))
    (first.root/'runtime-controls.jsonl').write_text(json.dumps(record)+'\n')
    restored = Server(first.root, agents)
    try:
        before = ready(restored)
        receipt = restart(restored, operation_id=operation)
        assert receipt['status'] == 'interrupted'
        assert restored.rpc('restart-status', {'operation_id': operation})['reload'] == receipt
        assert ready(restored)['runtime_instance_id'] == before['runtime_instance_id']
    finally:
        restored.close()


def test_invalid_control_types_do_not_restart_or_read_unrelated_operation(tmp_path):
    server = Server(tmp_path/'state', {'worker': pi_spec('worker')})
    try:
        before = ready(server)
        for payload in [{'dry_run': 'false'}, {'agent_names': 'worker'}, {'operation_id': 3},
                        {'operation_id': 'bad-id'}, {'agent_names': [None]}]:
            with pytest.raises(AssertionError): server.rpc('restart', payload)
        for operation in [3, 'invalid']:
            with pytest.raises(AssertionError): server.rpc('restart-status', {'operation_id': operation})
        operation = str(uuid.uuid4())
        failed = cli('--state-root', server.root, 'restart', 'absent', '--operation-id', operation)
        assert failed.returncode != 0 and operation in failed.stderr
        assert ready(server)['runtime_instance_id'] == before['runtime_instance_id']
        assert not (server.root/'runtime-controls.jsonl').exists()
    finally:
        server.close()


def test_configuration_reload_cannot_be_reused_as_a_restart(tmp_path):
    from test_native_reload import gated_worktree, reload, settled as reload_settled
    from test_native_workspaces import stop
    project, state, server, started, desired, entered, release = gated_worktree(tmp_path)
    try:
        operation = reload(server, desired)
        until = time.monotonic() + 5
        while not entered.exists() and time.monotonic() < until: time.sleep(.02)
        assert entered.exists()
        with pytest.raises(AssertionError, match='another runtime change'):
            restart(server)
        assert server.rpc('restart-status')['reload'] is None
    finally:
        release.write_text('continue')
        reload_settled(server, operation)
        stop(state)


def test_restart_preserves_dirty_worktree_and_binding_without_materialization(tmp_path):
    from test_native_workspaces import repo, start, stop, git
    from test_native_reload import connect
    project = repo(tmp_path)
    started = start(project)
    state = Path(started['state_root'])
    server = connect(state)
    work = project/'.ccb/workspaces/worker'
    try:
        ready(server)
        source = work/'source.txt'; source.write_text('uncommitted worker changes\n')
        untracked = work/'untracked.txt'; untracked.write_text('preserve')
        binding = work/'.ccb-workspace.json'
        before = (binding.read_bytes(), binding.stat().st_mtime_ns)
        trees = git(project, 'worktree', 'list', '--porcelain').stdout
        operation = restart(server)
        assert settled(server, operation)['status'] == 'applied'
        ready(server)
        assert source.read_text() == 'uncommitted worker changes\n'
        assert untracked.read_text() == 'preserve'
        assert (binding.read_bytes(), binding.stat().st_mtime_ns) == before
        assert git(project, 'worktree', 'list', '--porcelain').stdout == trees
    finally:
        stop(state)
