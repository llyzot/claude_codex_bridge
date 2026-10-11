"""Service reachability never substitutes for provider-runtime observations."""
import json
import os
import sys
import time
import pytest
from test_native_daemon import Server, spec, submit, await_watch
from test_native_role_memory import FACTORIES
from test_native_cli_security import cli
from test_native_project_view import data_hashes
from test_native_provider_faults import provider_spec, fault


def wait_ping(server, target, predicate):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        row = server.rpc('ping', {'target': target})
        if predicate(row): return row
        assert server.proc.poll() is None
        time.sleep(.025)
    pytest.fail(f'expected health did not arrive: {row}')


def test_default_ping_is_service_only_and_all_reports_unobserved_agents(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(), 'disabled': dict(spec(), dispatch_disabled=True)})
    try:
        baseline = data_hashes(server)
        daemon = server.rpc('ping')
        assert daemon['runtime'] == 'rust' and daemon['status'] == 'running'
        assert daemon['diagnostics']['scope'] == 'controller_service_only'
        nonce = 'a' * 32
        assert server.rpc('ping', {'target': 'ccbd', 'bootstrap_probe_nonce': nonce})['bootstrap_probe_nonce'] == nonce
        assert 'bootstrap_probe_nonce' not in server.rpc('ping', {'bootstrap_probe_nonce': 'bad'})
        result = cli('--state-root', server.root, 'ping', 'all')
        assert result.returncode == 0, result.stderr
        rows = {r['agent_name']: r for r in json.loads(result.stdout)['agents']}
        assert rows['worker']['health'] == 'unknown' and rows['disabled']['health'] == 'disabled'
        assert rows['worker']['diagnostics']['pid'] is None
        assert data_hashes(server) == baseline
        assert cli('--state-root', server.root, 'ping', 'missing').returncode != 0
        assert cli('--state-root', server.root, 'ping', 'worker', 'extra').returncode != 0
    finally:
        server.close()


@pytest.mark.skipif(os.name != 'nt', reason='Fresh process identity probe is Windows-specific')
@pytest.mark.parametrize('provider', FACTORIES)
def test_owned_ready_protocol_is_observed_without_prompt_or_state_writes(tmp_path, provider):
    trace = tmp_path / 'wire.jsonl'
    server = Server(tmp_path / 'state', {'worker': dict(FACTORIES[provider](), env={'CCB_FIXTURE_TRACE': str(trace)})})
    try:
        ready = wait_ping(server, 'worker', lambda r: r['health'] == 'healthy')
        assert ready['runtime_state'] == 'idle' and ready['diagnostics']['protocol_observed']
        assert ready['diagnostics']['process_observation'] == 'live'
        assert ready['diagnostics']['model_access'] == 'not_checked'
        before = data_hashes(server); wire = trace.read_bytes() if trace.exists() else b''
        for _ in range(5):
            result = server.rpc('ping', {'target': 'worker'})
            assert result['diagnostics']['pid'] == ready['diagnostics']['pid']
        assert data_hashes(server) == before
        assert (trace.read_bytes() if trace.exists() else b'') == wire
    finally:
        server.close()


@pytest.mark.skipif(os.name != 'nt', reason='Owned process identity scope')
def test_active_slot_and_provider_execution_must_match(tmp_path):
    from test_native_codex_rpc import codex_spec
    server = Server(tmp_path / 'state', {'worker': codex_spec('cancel-normal')})
    try:
        job = submit(server)['job_id']
        wait_ping(server, 'worker', lambda r: r['runtime_state'] == 'busy' and r['health'] == 'healthy')
        row = server.rpc('ping', {'target': 'worker'})
        assert row['active_job_id'] == row['diagnostics']['protocol_job_id'] == job
        server.rpc('cancel', {'job_id': job})
        await_watch(server, job, lambda r: r['terminal'])
        wait_ping(server, 'worker', lambda r: r['runtime_state'] == 'idle')
    finally:
        server.close()


def test_faulted_live_provider_does_not_become_healthy_or_restart_on_ping(tmp_path):
    server = Server(tmp_path / 'state', {'worker': provider_spec('pi', 'idle-late-fault', tmp_path), 'external': spec()})
    try:
        wait_ping(server, 'worker', lambda r: r['diagnostics'].get('protocol_idle_last_observed') is True)
        (tmp_path / 'release-error').touch()
        record = fault(server)
        before = data_hashes(server); launches = (tmp_path / 'fault-launches').read_bytes()
        for _ in range(5):
            row = server.rpc('ping', {'target': 'worker'})
            assert row['health'] == 'faulted' and row['diagnostics']['fault_id'] == record['fault_id']
            assert 'do-not-persist-protocol-error-secret' not in json.dumps(row)
        assert server.rpc('ping')['status'] == 'running' and server.rpc('ping')['degraded']
        assert data_hashes(server) == before and (tmp_path / 'fault-launches').read_bytes() == launches
        all_rows = server.rpc('ping', {'target': 'all'})
        assert all_rows['health_counts'] == {'faulted': 1, 'unknown': 1}
    finally:
        server.close()


def test_interactive_request_is_blocked_without_implicit_answer(tmp_path):
    from test_native_codex_requests import spec as interactive_spec, await_request, pending
    server = Server(tmp_path / 'state', {'worker': interactive_spec(tmp_path, 'input')})
    try:
        job = submit(server)['job_id']; request = await_request(server)
        before = data_hashes(server)
        row = server.rpc('ping', {'target': 'worker'})
        assert row['health'] == 'blocked' and row['diagnostics']['pending_request_count'] == 1
        assert row['diagnostics']['reason'] == 'provider_user_input'
        assert pending(server)[0]['request_id'] == request['request_id']
        assert server.rpc('watch', {'target': job})['status'] == 'running' and data_hashes(server) == before
        assert 'params' not in json.dumps(row)
    finally:
        server.close()


def test_observed_untracked_codex_activity_is_not_reported_idle(tmp_path):
    from test_native_codex_rpc import codex_spec
    trace = tmp_path / 'wire.jsonl'; idle = tmp_path / 'external-idle'
    config = dict(codex_spec('external-active'), env={'CCB_FIXTURE_TRACE': str(trace), 'CCB_EXTERNAL_IDLE_FILE': str(idle)})
    server = Server(tmp_path / 'state', {'worker': config})
    try:
        row = wait_ping(server, 'worker', lambda r: r['diagnostics']['reason'] == 'untracked_provider_activity')
        assert row['health'] == 'unknown' and row['runtime_state'] == 'busy'
        assert row['active_job_id'] is None and row['diagnostics']['untracked_provider_activity']
        assert server.rpc('records.list', {'stream': 'jobs', 'agent_name': 'worker'}, version=1)['result'] == []
        receipt = submit(server)
        time.sleep(.15)
        assert server.rpc('watch', {'target': receipt['job_id']})['status'] == 'accepted'
        assert not any(json.loads(line).get('method') == 'turn/start' for line in trace.read_text(encoding='utf-8').splitlines())
        idle.touch()
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'completed'
        assert server.rpc('ping', {'target': 'worker'})['diagnostics']['pid'] == row['diagnostics']['pid']
    finally:
        server.close()


@pytest.mark.skipif(os.name != 'nt', reason='Owned Windows process identity')
def test_foreign_thread_activity_does_not_block_owned_thread(tmp_path):
    from test_native_codex_rpc import codex_spec
    server = Server(tmp_path / 'state', {'worker': codex_spec('external-foreign')})
    try:
        wait_ping(server, 'worker', lambda r: r['health'] == 'healthy')
        receipt = submit(server)
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'completed'
    finally:
        server.close()


def test_bad_agent_lineage_does_not_hide_other_agents_or_mutate_jobs(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(), 'peer': spec()})
    try:
        submit(server)
        job = server.rpc('work', {'agent_name': 'worker'})['job']
        job['request']['project_id'] = 'foreign-project'
        server.rpc('records.append', {'stream': 'jobs', 'agent_name': 'worker', 'record': job}, version=1)
        before = data_hashes(server)
        rows = {r['agent_name']: r for r in server.rpc('ping', {'target': 'all'})['agents']}
        assert rows['worker']['health'] == 'unknown' and rows['worker']['mount_state'] == 'unknown'
        assert rows['worker']['diagnostics']['reason'] == 'agent_observation_failed'
        assert rows['peer']['diagnostics']['reason'] == 'external_worker_unobserved'
        assert data_hashes(server) == before
    finally:
        server.close()


@pytest.mark.skipif(os.name != 'nt', reason='Owned Windows process remains alive after stdout closes')
@pytest.mark.parametrize('provider', FACTORIES)
def test_alive_process_with_closed_output_is_not_healthy(tmp_path, provider):
    config = provider_spec(provider, 'idle-eof', tmp_path)
    # The Windows venv redirector adds a process retaining stdout, which masks
    # EOF when only the interpreter closes fd 1. Exercise actual pipe closure.
    config['command'][0] = sys._base_executable
    server = Server(tmp_path / 'state', {'worker': config})
    try:
        ready = wait_ping(server, 'worker', lambda r: r['health'] == 'healthy')
        (tmp_path / 'release-error').touch()
        deadline = time.monotonic() + 3
        while not (tmp_path / 'stdout-closed').exists() and time.monotonic() < deadline:
            time.sleep(.025)
        assert (tmp_path / 'stdout-closed').exists(), 'fixture did not close its output descriptor'
        row = wait_ping(server, 'worker', lambda r: r['diagnostics']['transport_open'] is False)
        assert row['health'] in {'unknown', 'faulted'} and row['diagnostics']['process_observation'] == 'live'
        assert row['diagnostics']['pid'] == ready['diagnostics']['pid'] and not row['diagnostics']['transport_open']
        assert len((tmp_path / 'fault-launches').read_text(encoding='utf-8').splitlines()) == 1
        assert server.rpc('queue', {'target': 'worker'})['queues'][0]['active_job_id'] is None
    finally:
        server.close()
