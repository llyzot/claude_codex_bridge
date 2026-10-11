"""Recover only sealed owned Windows executions; never infer completion from age."""
import json
import os
import time
import ctypes
from ctypes import wintypes
import subprocess
import sys
import uuid

import pytest
from test_native_daemon import Server, submit, await_watch, spec
from test_native_provider_faults import provider_spec, healthy_task
from test_native_provider_faults import status, fault, recover
from test_native_pi_rpc import pi_spec
from test_native_cli_security import cli

pytestmark = pytest.mark.skipif(os.name != 'nt', reason='Windows ownership proof')

def jobs(server, agent='worker'):
    return server.rpc('records.latest', dict(stream='jobs', agent_name=agent, identity='job_id'), version=1)['result']

def wait_input(directory, server):
    deadline = time.monotonic() + 8
    while not (directory / 'observed-owner.json').exists() and time.monotonic() < deadline:
        assert server.proc.poll() is None
        time.sleep(.025)
    assert (directory / 'observed-owner.json').exists()

def replace_job(server, row):
    server.rpc('records.append', dict(stream='jobs', agent_name='worker', record=row), version=1)

def creation_time():
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.GetCurrentProcess.restype = wintypes.HANDLE
    kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    values = [wintypes.FILETIME() for _ in range(4)]
    assert kernel.GetProcessTimes(kernel.GetCurrentProcess(), *(ctypes.byref(v) for v in values))
    return str((values[0].dwHighDateTime << 32) | values[0].dwLowDateTime)

def job_api():
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
    kernel.CreateJobObjectW.restype = wintypes.HANDLE
    kernel.OpenJobObjectW.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR]
    kernel.OpenJobObjectW.restype = wintypes.HANDLE
    kernel.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    kernel.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    return kernel

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_daemon_crash_reconciles_owned_execution_without_replay_and_runs_next_job(tmp_path, provider):
    root = tmp_path / 'state'
    agents = {'worker':provider_spec(provider, 'hold', tmp_path), 'healthy':spec('worker')}
    server = Server(root, agents)
    try:
        first = submit(server)
        wait_input(tmp_path, server)
        second = submit(server, body='distinct queued work')
    finally:
        server.proc.kill(); server.proc.wait(timeout=5)
    restored = Server(root, agents)
    try:
        row = await_watch(restored, first['job_id'], lambda r:r['terminal'])
        assert row['status'] == 'incomplete'
        assert row['completion_reason'] == 'native_owner_stopped'
        assert await_watch(restored, second['job_id'], lambda r:r['terminal'])['reply'] == 'fault fixture result'
        assert len(jobs(restored)) == 2
        proof = json.loads((tmp_path / 'observed-owner.json').read_text())
        assert proof['job_id'] == first['job_id'] and proof['kind'] == 'windows_job'
        assert proof['owner_pid'] == server.proc.pid
        recovered = jobs(restored)[0]
        assert recovered['terminal_decision']['diagnostics']['automatic_replay'] is False
        assert restored.rpc('job-reconcile', {'target':first['job_id']})['status'] == 'terminal'
        healthy_task(restored)
    finally: restored.close()

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_silent_cancel_recovers_as_cancelled_without_a_reply_or_repeated_prompt(tmp_path, provider):
    root = tmp_path / 'state'
    agents = {'worker':provider_spec(provider, 'hold', tmp_path)}
    server = Server(root, agents)
    try:
        receipt = submit(server)
        wait_input(tmp_path, server)
        server.rpc('cancel', dict(job_id=receipt['job_id'], record_reply=False))
    finally:
        server.proc.kill(); server.proc.wait(timeout=5)
    restored = Server(root, agents)
    try:
        row = await_watch(restored, receipt['job_id'], lambda r:r['terminal'])
        assert row['status'] == 'cancelled'
        assert len(jobs(restored)) == 1
        assert restored.rpc('records.list', {'stream':'replies'}, version=1)['result'] == []
        assert jobs(restored)[0]['terminal_decision']['diagnostics']['cancel_requested'] is True
    finally: restored.close()

@pytest.mark.parametrize('mode', ['live-owner', 'pid-reused', 'invalid-execution'])
def test_recovery_uses_creation_identity_and_never_age_or_pid_alone(tmp_path, mode):
    root = tmp_path / 'state'
    agents = {'worker':provider_spec('codex', 'hold', tmp_path), 'healthy':spec('worker')}
    server = Server(root, agents)
    try:
        receipt = submit(server)
        wait_input(tmp_path, server)
        row = jobs(server)[0]
        proof = row['provider_options']['native_scope_proof']
        if mode == 'invalid-execution': proof['execution_instance_id'] = 'foreign'
        else:
            proof['owner_pid'] = os.getpid()
            if mode == 'live-owner': proof['owner_created'] = creation_time()
            else: assert proof['owner_created'] != creation_time()
        replace_job(server, row)
    finally:
        server.proc.kill(); server.proc.wait(timeout=5)
    restored = Server(root, agents)
    try:
        if mode == 'pid-reused':
            assert await_watch(restored, receipt['job_id'], lambda r:r['terminal'])['status'] == 'incomplete'
        else:
            view = restored.rpc('job-reconcile', {'target':receipt['job_id']})
            assert view['status'] == ('pending' if mode == 'live-owner' else 'unavailable')
            assert view['reason'] == ('owner_alive' if mode == 'live-owner' else 'ownership_proof_invalid')
            healthy_task(restored)
            assert restored.rpc('watch', {'target':receipt['job_id']})['status'] == 'running'
            assert len((tmp_path / 'fault-launches').read_text().splitlines()) == 1
    finally: restored.close()

@pytest.mark.parametrize('matching', [True, False])
def test_exact_durable_terminal_snapshot_wins_and_foreign_snapshot_is_ignored(tmp_path, matching):
    root = tmp_path / 'state'
    agents = {'worker':provider_spec('codex', 'hold', tmp_path)}
    server = Server(root, agents)
    try:
        receipt = submit(server)
        wait_input(tmp_path, server)
        row = jobs(server)[0]
        proof = row['provider_options']['native_scope_proof']
        decision = dict(schema_version=2, record_type='completion_decision', terminal=True, status='completed',
                        reason='saved_verified_result', confidence='exact', reply='already completed',
                        anchor_seen=True, reply_started=True, reply_stable=True, provider_turn_ref='saved-turn', source_cursor=None,
                        finished_at=row['updated_at'], diagnostics=dict(runtime_instance_id=proof['runtime_instance_id'],
                        native_execution_instance=proof['execution_instance_id'] if matching else 'foreign'))
        snapshot = dict(schema_version=2, record_type='completion_snapshot', job_id=receipt['job_id'], agent_name='worker',
                        profile_family='protocol_turn', state=dict(schema_version=2, record_type='completion_state'),
                        latest_decision=decision, latest_reply_preview='already completed', updated_at=row['updated_at'])
        server.rpc('completion_snapshot.save', dict(job_id=receipt['job_id'], record=snapshot), version=1)
    finally:
        server.proc.kill(); server.proc.wait(timeout=5)
    restored = Server(root, agents)
    try:
        result = await_watch(restored, receipt['job_id'], lambda r:r['terminal'])
        assert result['status'] == ('completed' if matching else 'incomplete')
        assert (result['reply'] == 'already completed') is matching
        assert jobs(restored)[0]['terminal_decision']['diagnostics']['reused_terminal_snapshot'] is matching
    finally: restored.close()

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_interrupted_reply_delivery_is_retained_without_automatic_replay(tmp_path, provider):
    lead_directory = tmp_path / 'lead'
    lead_directory.mkdir()
    root = tmp_path / 'state'
    agents = {'lead':provider_spec(provider, 'hold', lead_directory), 'worker':spec('worker')}
    server = Server(root, agents)
    try:
        submit(server, actor='lead')
        wait_input(lead_directory, server)
        old = jobs(server, 'lead')[0]
        assert old['request']['message_type'] == 'reply_delivery'
        event_id = old['provider_options']['reply_delivery_inbound_event_id']
    finally:
        server.proc.kill(); server.proc.wait(timeout=5)
    restored = Server(root, agents)
    try:
        assert await_watch(restored, old['job_id'], lambda r:r['terminal'])['status'] == 'incomplete'
        blocked = fault(restored, 'lead')
        assert blocked['reason'] == 'result_delivery_uncertain'
        assert restored.rpc('mailbox-head', {'agent_name':'lead'})['head']['inbound_event_id'] == event_id
        assert len(jobs(restored, 'lead')) == 1
        assert len((lead_directory / 'fault-launches').read_text().splitlines()) == 1
    finally: restored.close()
    again = Server(root, agents)
    try:
        assert fault(again, 'lead')['fault_id'] == blocked['fault_id']
        assert status(again, 'lead')['owned'] is None
        recover(again, blocked)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            rows = jobs(again, 'lead')
            if len(rows) == 2 and rows[-1]['status'] == 'completed': break
            time.sleep(.025)
        assert len(rows) == 2 and rows[-1]['status'] == 'completed'
        assert again.rpc('mailbox-head', {'agent_name':'lead'})['head'] is None
    finally: again.close()

def test_owned_worker_recovery_automatically_resumes_the_saved_pi_lead_session(tmp_path):
    lead = pi_spec('persist-lead')
    lead['command'].append(str(tmp_path / 'lead-session.json'))
    agents = {'lead':lead, 'worker':provider_spec('codex', 'hold', tmp_path)}
    root = tmp_path / 'state'
    server = Server(root, agents)
    try:
        parent = submit(server, 'lead')
        wait_input(tmp_path, server)
        deadline = time.monotonic() + 8
        while server.rpc('queue', {'target':'lead'})['queues'][0]['active_job_id'] is not None and time.monotonic() < deadline: time.sleep(.025)
        before = json.loads((root / 'providers/pi/lead/session.json').read_text())
        assert before['state']['messageCount'] == 2
    finally:
        server.proc.kill(); server.proc.wait(timeout=5)
    restored = Server(root, agents)
    try:
        result = await_watch(restored, parent['job_id'], lambda r:r['visible_reply_source'] == 'message_bureau_reply')
        assert result['reply'] == 'persistent Pi reviewed result'
        after = json.loads((root / 'providers/pi/lead/session.json').read_text())
        assert after['state']['sessionId'] == before['state']['sessionId']
        assert after['state']['messageCount'] == 4
        assert len(jobs(restored)) == 1
        edges = restored.rpc('records.latest', dict(stream='callbacks', identity='edge_id'), version=1)['result']
        assert len(edges) == 1 and edges[0]['state'] == 'done'
    finally: restored.close()

@pytest.mark.parametrize('live_scope', [False, True])
def test_named_scope_observation_requires_zero_live_members(tmp_path, live_scope):
    root = tmp_path / 'state'
    agents = {'worker':provider_spec('codex', 'hold', tmp_path), 'healthy':spec('worker')}
    api = job_api()
    handle = None
    child = None
    restored = None
    server = Server(root, agents)
    try:
        receipt = submit(server)
        wait_input(tmp_path, server)
        row = jobs(server)[0]
        proof = row['provider_options']['native_scope_proof']
        if live_scope:
            identity = str(uuid.uuid4())
            name = 'Local\\CCB.Native.Provider.' + identity
            handle = api.CreateJobObjectW(None, name)
            assert handle
            child = subprocess.Popen([sys.executable, '-I', '-c', 'import time; time.sleep(60)'], creationflags=0x08000000)
            assert api.AssignProcessToJobObject(handle, int(child._handle))
            proof['scope_id'] = identity
            proof['scope_name'] = name
            replace_job(server, row)
        else:
            # Keep the old named object discoverable after all its processes die.
            handle = api.OpenJobObjectW(4, False, proof['scope_name'])
            assert handle
        server.proc.kill(); server.proc.wait(timeout=5)
        restored = Server(root, agents)
        if live_scope:
            view = restored.rpc('job-reconcile', {'target':receipt['job_id']})
            assert view['status'] == 'pending' and view['reason'] == 'scope_has_live_processes'
            healthy_task(restored)
            assert child.poll() is None
            assert restored.rpc('watch', {'target':receipt['job_id']})['status'] == 'running'
            assert api.TerminateJobObject(handle, 1)
            child.wait(timeout=5)
        result = await_watch(restored, receipt['job_id'], lambda r:r['terminal'])
        assert result['status'] == 'incomplete'
        assert jobs(restored)[0]['terminal_decision']['diagnostics']['owner_observation']['reason'] == 'owned_scope_empty'
    finally:
        if child is not None and child.poll() is None:
            api.TerminateJobObject(handle, 1); child.wait(timeout=5)
        if handle: api.CloseHandle(handle)
        if restored is not None: restored.close()
        if server.proc.poll() is None: server.close()

def test_cli_recovery_observes_current_execution_without_mutating_or_restarting_it(tmp_path):
    server = Server(tmp_path / 'state', {'worker':provider_spec('codex', 'hold', tmp_path)})
    try:
        receipt = submit(server)
        wait_input(tmp_path, server)
        for command in ('job-recovery', 'job-reconcile'):
            result = cli('--state-root', server.root, command, receipt['job_id'])
            assert result.returncode == 0, result.stderr
            view = json.loads(result.stdout)
            assert view['status'] == 'pending' and view['reason'] == 'current_owner'
        assert server.rpc('watch', {'target':receipt['job_id']})['status'] == 'running'
        assert len((tmp_path / 'fault-launches').read_text().splitlines()) == 1
    finally: server.close()

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_missing_legacy_ownership_proof_stays_fenced_without_spawning_replacement(tmp_path, provider):
    root = tmp_path / 'state'
    agents = {'worker':provider_spec(provider, 'hold', tmp_path), 'healthy':spec('worker')}
    server = Server(root, agents)
    try:
        first = submit(server)
        wait_input(tmp_path, server)
        row = jobs(server)[0]
        row['provider_options'].pop('native_scope_proof', None)
        server.rpc('records.append', dict(stream='jobs', agent_name='worker', record=row), version=1)
    finally:
        server.proc.kill(); server.proc.wait(timeout=5)
    restored = Server(root, agents)
    try:
        view = restored.rpc('job-recovery', {'target':first['job_id']})
        assert view['status'] == 'unavailable' and view['reason'] == 'ownership_proof_missing'
        assert restored.rpc('job-reconcile', {'target':first['job_id']})['status'] == 'unavailable'
        healthy_task(restored)
        assert restored.rpc('watch', {'target':first['job_id']})['status'] == 'running'
        assert len((tmp_path / 'fault-launches').read_text().splitlines()) == 1
    finally: restored.close()
