"""Windows owned Job boundaries include silent descendants and exclude other Agents."""
import json
import os
from pathlib import Path
import time

import pytest
from native_kernel import _owner_stopped
from test_native_daemon import Server, spec, submit, await_watch
from test_native_provider_faults import provider_spec, fault, status, healthy_task, recover
from test_native_cli_security import cli

pytestmark = pytest.mark.skipif(os.name != 'nt', reason='Windows native Job ownership contract')

def request_from_status(server, *, silent=True):
    row = status(server)
    return dict(agent_name='worker', runtime_instance_id=row['owned']['runtime_instance_id'],
                pid=row['owned']['pid'], scope_id=row['scope']['scope_id'],
                job_id=(row['active'] or {}).get('job_id'),
                execution_instance_id=(row['active'] or {}).get('execution_instance_id'), record_reply=not silent)

def await_closed(server):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        row = status(server)
        if row['process_state'] == 'closed': return row
        assert server.proc.poll() is None
        time.sleep(.025)
    pytest.fail('owned tree did not reach confirmed closure')

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
@pytest.mark.parametrize('mode', ['tree', 'tree-exit'])
def test_termination_closes_silent_descendants_and_keeps_other_agents_alive(tmp_path, provider, mode):
    server = Server(tmp_path / 'state', {'worker': provider_spec(provider, mode, tmp_path), 'healthy': spec('worker')})
    try:
        receipt = submit(server)
        record = fault(server)
        deadline = time.monotonic() + 8
        while not (tmp_path / 'grandchild.pid').exists() and time.monotonic() < deadline: time.sleep(.025)
        assert (tmp_path / 'grandchild.pid').exists()
        pids = [int((tmp_path / (kind + '.pid')).read_text()) for kind in ('child', 'grandchild')]
        assert all(not _owner_stopped(pid) for pid in pids)
        time.sleep(.15)
        assert server.rpc('watch', {'target': receipt['job_id']})['status'] == 'running'
        row = status(server)
        assert row['scope']['active_processes'] >= 2
        binding = request_from_status(server)
        for field in ('pid', 'scope_id', 'runtime_instance_id', 'execution_instance_id', 'job_id'):
            foreign = dict(binding, **{field: 1 if field == 'pid' else 'foreign'})
            with pytest.raises(AssertionError, match='identity mismatch|execution mismatch'):
                server.rpc('provider-terminate', foreign)
        result = server.rpc('provider-terminate', binding)
        assert result['status'] == 'termination_requested'
        terminal = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        assert terminal['status'] == 'cancelled' and terminal['completion_reason'] == 'provider_terminated'
        with pytest.raises(AssertionError, match='caller provider is faulted'):
            submit(server, 'healthy', actor='worker')
        closed = await_closed(server)
        assert closed['scope']['active_processes'] == 0
        assert all(_owner_stopped(pid) for pid in pids)
        again = server.rpc('provider-terminate', binding)
        assert again['termination_id'] == result['termination_id'] and again['status'] == 'terminated'
        assert server.rpc('records.list', {'stream': 'replies'}, version=1)['result'] == []
        rows = server.rpc('records.latest', dict(stream='jobs', agent_name='worker', identity='job_id'), version=1)['result']
        intent = rows[0]['provider_options']['native_termination_intent']
        assert intent['binding']['scope_id'] == binding['scope_id']
        assert intent['binding']['record_reply'] is False
        assert rows[0]['terminal_decision']['diagnostics']['owned_tree_empty'] is True
        events = server.rpc('records.list', dict(stream='job_events', agent_name='worker'), version=1)['result']
        assert len([e for e in events if e['type'] == 'provider_termination_requested']) == 1
        healthy_task(server)
        recover(server, record)
        second = submit(server)
        assert await_watch(server, second['job_id'], lambda r: r['terminal'])['reply'] == 'fault fixture result'
        with pytest.raises(AssertionError, match='runtime identity mismatch'):
            server.rpc('provider-terminate', binding)
    finally:
        (tmp_path / 'release-fault').touch()
        server.close()

def test_unicode_quoting_and_path_environment_preserve_provider_arguments(tmp_path):
    import sys
    script = tmp_path / '含 空格的参数.py'
    output = tmp_path / 'arguments.json'
    script.write_text('import json,os,sys; from pathlib import Path; Path(sys.argv[1]).write_text(json.dumps([sys.argv[2:],os.environ["CCB_CASE_VALUE"]]),encoding="utf-8")', encoding='utf-8')
    arguments = ['', '含 空格', 'quote"slash\\', 'ends in\\', 'x\\\\" y']
    # A project-local namesake must not shadow the explicitly configured PATH.
    (tmp_path / Path(sys.executable).name).write_bytes(b'not an executable')
    record = dict(provider='pi', driver_kind='pi_rpc', command=[Path(sys.executable).name, '-I', str(script), str(output), *arguments],
                  workspace_path=str(tmp_path), env={'Path':str(Path(sys.executable).parent), 'CCB_CASE_VALUE':'值 "\\尾'})
    server = Server(tmp_path / 'state', {'worker':record})
    try:
        deadline = time.monotonic() + 8
        while not output.exists() and time.monotonic() < deadline: time.sleep(.025)
        assert output.exists()
        assert json.loads(output.read_text()) == [arguments, '值 "\\尾']
    finally: server.close()

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_cli_terminates_idle_owned_scope_and_retains_intent_across_restart(tmp_path, provider):
    root = tmp_path / 'state'
    agents = {'worker': provider_spec(provider, 'normal', tmp_path)}
    server = Server(root, agents)
    try:
        deadline = time.monotonic() + 8
        while status(server)['owned'] is None and time.monotonic() < deadline: time.sleep(.025)
        old = status(server)
        result = cli('--state-root', root, 'provider-terminate', 'worker', '--silent')
        assert result.returncode == 0, result.stderr
        intent = json.loads(result.stdout)
        closed = await_closed(server)
        assert closed['scope']['active_processes'] == 0
        assert closed['fault']['termination_intent']['termination_id'] == intent['termination_id']
        again = cli('--state-root', root, 'provider-terminate', 'worker', '--silent')
        assert again.returncode == 0, again.stderr
        assert json.loads(again.stdout)['termination_id'] == intent['termination_id']
        record = closed['fault']
    finally: server.close()
    restored = Server(root, agents)
    try:
        assert fault(restored)['termination_intent']['termination_id'] == intent['termination_id']
        assert status(restored)['owned'] is None
        recover(restored, record)
        receipt = submit(restored)
        assert await_watch(restored, receipt['job_id'], lambda r: r['terminal'])['reply'] == 'fault fixture result'
        assert status(restored)['scope']['scope_id'] != old['scope']['scope_id']
    finally: restored.close()

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_scoped_termination_preserves_an_already_running_peer_provider(tmp_path, provider):
    peer_directory = tmp_path / 'peer'
    peer_directory.mkdir()
    server = Server(tmp_path / 'state', {'worker':provider_spec(provider, 'tree', tmp_path),
                                      'other':provider_spec('codex', 'normal', peer_directory)})
    try:
        peer = submit(server, 'other')
        await_watch(server, peer['job_id'], lambda r:r['terminal'])
        before = status(server, 'other')
        receipt = submit(server)
        fault(server)
        server.rpc('provider-terminate', request_from_status(server))
        assert await_watch(server, receipt['job_id'], lambda r:r['terminal'])['status'] == 'cancelled'
        assert not _owner_stopped(before['owned']['pid'])
        after = status(server, 'other')
        assert after['owned']['pid'] == before['owned']['pid']
        assert after['scope']['scope_id'] == before['scope']['scope_id']
        again = submit(server, 'other')
        assert await_watch(server, again['job_id'], lambda r:r['terminal'])['reply'] == 'fault fixture result'
    finally:
        (tmp_path / 'release-fault').touch()
        server.close()
