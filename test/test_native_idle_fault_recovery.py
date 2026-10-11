"""Idle faults retain provider lifetime proof independently of any task."""
import json
import os
import subprocess
import sys
import uuid
import pytest
from test_native_daemon import Server, submit, await_watch
from test_native_transport_eof import config, ready, fault_record, wait_for
from test_native_provider_faults import status, recover

pytestmark = pytest.mark.skipif(os.name != 'nt', reason='Windows process creation and Job identity')


def retired_fault(tmp_path, provider='pi'):
    agents = {'worker': config(provider, 'idle-eof', tmp_path)}
    root = tmp_path / 'state'; server = Server(root, agents)
    try:
        ready(server)
        (tmp_path / 'release-error').touch()
        row = fault_record(server, provider)
        proof = row['provider_scope']
        assert row['job_id'] is None and proof['provider_pid'] == row['pid']
        assert proof['runtime_instance_id'] == row['runtime_instance_id']
        assert proof['project_id'] == 'native-test' and proof['agent_name'] == 'worker'
        assert 'execution_instance_id' not in proof and 'job_id' not in proof
        (tmp_path / 'release-fault').touch()
        wait_for(lambda: status(server)['process_state'] == 'closed')
    finally:
        (tmp_path / 'release-error').touch(); (tmp_path / 'release-fault').touch(); server.close()
    return root, agents, row


@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_idle_fault_can_recover_after_controller_restart(tmp_path, provider):
    root, agents, row = retired_fault(tmp_path, provider)
    server = Server(root, agents)
    try:
        assert status(server)['fault']['fault_id'] == row['fault_id']
        assert status(server)['recovery_observation']['status'] == 'stopped'
        recover(server, row)
        task = submit(server)
        assert await_watch(server, task['job_id'], lambda r: r['terminal'])['status'] == 'completed'
    finally:
        server.close()


def test_reused_provider_pid_does_not_block_recovery_or_kill_unrelated_process(tmp_path):
    root, agents, row = retired_fault(tmp_path)
    child = subprocess.Popen([sys._base_executable, '-c', 'import time; time.sleep(60)'], creationflags=0x08000000)
    try:
        row['pid'] = child.pid
        row['provider_scope']['provider_pid'] = child.pid
        row['provider_scope']['provider_created'] = '1'
        path = root / 'providers/pi/worker/fault.json'
        path.write_text(json.dumps(row), encoding='utf-8')
        server = Server(root, agents)
        try:
            snapshot = status(server)
            assert snapshot['process_state'] == 'closed'
            assert snapshot['recovery_observation']['status'] == 'stopped'
            recover(server, row)
            assert child.poll() is None
            task = submit(server)
            assert await_watch(server, task['job_id'], lambda r: r['terminal'])['status'] == 'completed'
            assert child.poll() is None
        finally:
            server.close()
    finally:
        child.kill(); child.wait(timeout=5)


@pytest.mark.parametrize('field,value', [('project_id', 'foreign'), ('agent_name', 'peer'), ('provider_pid', 0), ('provider_created', 'invalid'), ('scope_name', 'Local\\unrelated')])
def test_invalid_idle_lifetime_proof_cannot_downgrade_to_pid_only_recovery(tmp_path, field, value):
    root, agents, row = retired_fault(tmp_path)
    row['provider_scope'][field] = value
    (root / 'providers/pi/worker/fault.json').write_text(json.dumps(row), encoding='utf-8')
    before = (tmp_path / 'fault-launches').read_bytes()
    with pytest.raises(AssertionError, match='provider scope'):
        Server(root, agents)
    assert (tmp_path / 'fault-launches').read_bytes() == before


def test_wrong_windows_session_stays_unverified(tmp_path):
    root, agents, row = retired_fault(tmp_path)
    row['provider_scope']['windows_session'] += 1
    (root / 'providers/pi/worker/fault.json').write_text(json.dumps(row), encoding='utf-8')
    server = Server(root, agents)
    try:
        observation = status(server)['recovery_observation']
        assert observation == {'status': 'pending', 'reason': 'windows_namespace_unverified'}
        with pytest.raises(AssertionError, match='not confirmed stopped'): recover(server, row)
        assert len((tmp_path / 'fault-launches').read_text().splitlines()) == 1
    finally:
        server.close()


def test_live_named_scope_member_blocks_recovery_without_termination(tmp_path):
    from test_native_execution_recovery import job_api
    root, agents, row = retired_fault(tmp_path)
    api = job_api(); identity = str(uuid.uuid4()); name = 'Local\\CCB.Native.Provider.' + identity
    handle = api.CreateJobObjectW(None, name); assert handle
    child = None; server = None
    try:
        child = subprocess.Popen([sys._base_executable, '-c', 'import time; time.sleep(60)'], creationflags=0x08000000)
        assert api.AssignProcessToJobObject(handle, int(child._handle))
        row['provider_scope'].update(scope_id=identity, scope_name=name)
        (root / 'providers/pi/worker/fault.json').write_text(json.dumps(row), encoding='utf-8')
        server = Server(root, agents)
        assert status(server)['recovery_observation']['reason'] == 'scope_has_live_processes'
        with pytest.raises(AssertionError, match='not confirmed stopped'): recover(server, row)
        assert child.poll() is None
        child.kill(); child.wait(timeout=5)
        # Waiting on the root process does not synchronously update Job accounting.
        wait_for(lambda: status(server)['recovery_observation']['reason'] == 'owned_scope_empty')
        recover(server, row)
    finally:
        if server: server.close()
        if child and child.poll() is None: child.kill(); child.wait(timeout=5)
        api.CloseHandle(handle)


def test_legacy_pid_only_fault_remains_conservative(tmp_path):
    root, agents, row = retired_fault(tmp_path)
    child = subprocess.Popen([sys._base_executable, '-c', 'import time; time.sleep(60)'], creationflags=0x08000000)
    try:
        row.pop('provider_scope'); row['pid'] = child.pid
        (root / 'providers/pi/worker/fault.json').write_text(json.dumps(row), encoding='utf-8')
        server = Server(root, agents)
        try:
            assert status(server)['recovery_observation']['reason'] == 'provider_scope_missing'
            with pytest.raises(AssertionError, match='not confirmed closed'): recover(server, row)
            assert child.poll() is None
        finally:
            server.close()
    finally:
        child.kill(); child.wait(timeout=5)
