"""Shared/private project memory is live, scoped and not copied into job records."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import pytest
from test_native_daemon import Server, submit, await_watch
from test_native_role_memory import FACTORIES, prompts, install


def memory(project, name='worker'):
    shared = project / '.ccb/ccb_memory.md'
    private = project / '.ccb/agents' / name / 'memory.md'
    private.parent.mkdir(parents=True, exist_ok=True)
    shared.write_text('SHARED_OLD 中文', encoding='utf-8')
    private.write_text('PRIVATE_OLD 中文', encoding='utf-8')
    return shared, private


def agent(provider, workspace, trace, **extra):
    return dict(FACTORIES[provider](), workspace_path=str(workspace), env={'CCB_FIXTURE_TRACE': str(trace)}, **extra)


def done(server, **kwargs):
    receipt = submit(server, **kwargs)
    return await_watch(server, receipt['job_id'], lambda r: r['terminal'])


@pytest.mark.parametrize('provider', FACTORIES)
def test_live_shared_private_memory_and_deletion_on_same_session(tmp_path, provider):
    project = tmp_path / 'project'; project.mkdir()
    shared, private = memory(project)
    trace = tmp_path / 'wire.jsonl'
    server = Server(tmp_path / 'runtime/ccbd', {'worker': agent(provider, project, trace)}, config_extra={'project_root': str(project)})
    try:
        assert done(server, body='task one')['status'] == 'completed'
        first = json.loads((server.root / f'providers/{provider}/worker/session.json').read_text(encoding='utf-8'))
        text = prompts(trace, provider)[-1]
        assert 'SHARED_OLD 中文' in text and 'PRIVATE_OLD 中文' in text
        shared.write_text('SHARED_NEW 中文', encoding='utf-8'); private.write_text('PRIVATE_NEW 中文', encoding='utf-8')
        assert done(server, body='task two')['status'] == 'completed'
        text = prompts(trace, provider)[-1]
        assert 'SHARED_NEW 中文' in text and 'PRIVATE_NEW 中文' in text and '_OLD' not in text
        events = server.rpc('records.list', {'stream': 'job_events', 'agent_name': 'worker'}, version=1)['result']
        audit = [e['payload'] for e in events if e['type'] == 'project_memory_loaded'][-1]
        assert audit['sources'][0]['sha256'] == hashlib.sha256(shared.read_bytes()).hexdigest()
        assert audit['sources'][1]['sha256'] == hashlib.sha256(private.read_bytes()).hexdigest()
        # Legacy source discovery agrees on the two CCB source paths and content.
        from project_memory import load_memory_sources
        expected = load_memory_sources(project, agent_name='worker', provider=provider)
        assert [(s.kind, s.content) for s in expected] == [('ccb_shared', 'SHARED_NEW 中文'), ('agent_private', 'PRIVATE_NEW 中文')]
        shared.unlink(); private.unlink()
        assert done(server, body='task three')['status'] == 'completed'
        text = prompts(trace, provider)[-1]
        assert 'SHARED_NEW' not in text and 'PRIVATE_NEW' not in text
        assert 'Empty content means no current memory' in text
        current = json.loads((server.root / f'providers/{provider}/worker/session.json').read_text(encoding='utf-8'))
        assert (first['pid'], first['runtime_instance_id']) == (current['pid'], current['runtime_instance_id'])
        for path in list(server.root.rglob('*.jsonl')) + list(server.root.parent.joinpath('agents').rglob('*.jsonl')):
            content = path.read_text(encoding='utf-8')
            assert 'SHARED_OLD' not in content and 'PRIVATE_NEW' not in content
        assert not shared.exists() and not private.exists()
    finally:
        server.close()


def test_project_anchor_private_isolation_and_role_order(tmp_path):
    project = tmp_path / 'project'; project.mkdir()
    shared, private = memory(project)
    peer = project / '.ccb/agents/peer/memory.md'; peer.parent.mkdir(); peer.write_text('PEER_ONLY', encoding='utf-8')
    workspace = tmp_path / 'worktree'; workspace.mkdir(); memory(workspace)
    (workspace / '.ccb/ccb_memory.md').write_text('WORKTREE_DECOY', encoding='utf-8')
    role = install(project); (role / 'memory.md').write_text('ROLE_ORDER', encoding='utf-8')
    # Provider-native project files remain the provider's responsibility.
    (project / 'AGENTS.md').write_text('NATIVE_PROJECT_DECOY', encoding='utf-8')
    trace = tmp_path / 'wire.jsonl'
    server = Server(tmp_path / 'runtime/ccbd', {'worker': agent('codex', workspace, trace, role='agentroles.reader')}, config_extra={'project_root': str(project)})
    try:
        assert done(server)['status'] == 'completed'
        text = prompts(trace, 'codex')[-1]
        assert text.index('SHARED_OLD') < text.index('ROLE_ORDER') < text.index('PRIVATE_OLD')
        assert f'source: {shared.as_posix()}' in text
        assert f'source: {private.as_posix()}' in text
        assert 'PEER_ONLY' not in text and 'WORKTREE_DECOY' not in text and 'NATIVE_PROJECT_DECOY' not in text
        assert shared.read_text(encoding='utf-8') == 'SHARED_OLD 中文'
    finally:
        server.close()


@pytest.mark.parametrize('bad', ['utf8', 'combined-size', 'directory'])
def test_bad_memory_fails_task_without_sending_and_can_be_repaired_live(tmp_path, bad):
    project = tmp_path / 'project'; project.mkdir(); shared, private = memory(project)
    if bad == 'utf8': private.write_bytes(b'\xffSECRET_MEMORY_ERROR')
    elif bad == 'combined-size':
        shared.write_bytes(b'x' * (200 * 1024)); private.write_bytes(b'y' * (100 * 1024))
    else:
        private.unlink(); private.mkdir()
    trace = tmp_path / 'wire.jsonl'
    server = Server(tmp_path / 'runtime/ccbd', {'worker': agent('pi', project, trace)}, config_extra={'project_root': str(project)})
    try:
        result = done(server)
        assert result['status'] == 'failed' and result['completion_reason'] == 'project_memory_invalid'
        assert prompts(trace, 'pi') == [] and 'SECRET_MEMORY_ERROR' not in json.dumps(result)
        before = json.loads((server.root / 'providers/pi/worker/session.json').read_text(encoding='utf-8'))
        if bad == 'directory': private.rmdir()
        shared.write_text('SHARED_FIXED', encoding='utf-8'); private.write_text('PRIVATE_FIXED', encoding='utf-8')
        assert done(server)['status'] == 'completed'
        assert 'PRIVATE_FIXED' in prompts(trace, 'pi')[-1]
        after = json.loads((server.root / 'providers/pi/worker/session.json').read_text(encoding='utf-8'))
        assert before['pid'] == after['pid']
    finally:
        server.close()


@pytest.mark.skipif(os.name != 'nt', reason='Windows junction privacy fence')
def test_junction_cannot_redirect_private_memory_to_another_agent(tmp_path):
    project = tmp_path / 'project'; project.mkdir()
    memory(project, 'peer')
    parent = project / '.ccb/agents'
    result = subprocess.run(['cmd', '/c', 'mklink', '/J', str(parent / 'worker'), str(parent / 'peer')], capture_output=True)
    assert result.returncode == 0, result.stderr
    trace = tmp_path / 'wire.jsonl'
    server = Server(tmp_path / 'runtime/ccbd', {'worker': agent('codex', project, trace)}, config_extra={'project_root': str(project)})
    try:
        result = done(server)
        assert result['completion_reason'] == 'project_memory_invalid'
        assert prompts(trace, 'codex') == []
        assert (parent / 'peer/memory.md').read_text(encoding='utf-8') == 'PRIVATE_OLD 中文'
    finally:
        server.close()


def test_cold_restart_reads_current_memory_into_resumed_codex_thread(tmp_path):
    project = tmp_path / 'project'; project.mkdir(); shared, private = memory(project)
    trace = tmp_path / 'wire.jsonl'
    root = tmp_path / 'runtime/ccbd'; config = {'worker': agent('codex', project, trace)}
    first = Server(root, config, config_extra={'project_root': str(project)})
    try:
        assert done(first)['status'] == 'completed'
        original = json.loads((root / 'providers/codex/worker/session.json').read_text(encoding='utf-8'))
    finally:
        first.close()
    private.write_text('AFTER_RESTART', encoding='utf-8')
    second = Server(root, config, config_extra={'project_root': str(project)})
    try:
        assert done(second)['status'] == 'completed'
        current = json.loads((root / 'providers/codex/worker/session.json').read_text(encoding='utf-8'))
        assert current['state']['thread_id'] == original['state']['thread_id']
        assert 'AFTER_RESTART' in prompts(trace, 'codex')[-1] and 'PRIVATE_OLD' not in prompts(trace, 'codex')[-1]
    finally:
        second.close()


def test_queued_task_reads_memory_at_dispatch_not_admission(tmp_path):
    from test_native_codex_rpc import codex_spec
    project = tmp_path / 'project'; project.mkdir(); shared, private = memory(project)
    trace = tmp_path / 'wire.jsonl'
    config = agent('codex', project, trace, command=codex_spec('cancel-normal')['command'])
    server = Server(tmp_path / 'runtime/ccbd', {'worker': config}, config_extra={'project_root': str(project)})
    try:
        first = submit(server)['job_id']
        await_watch(server, first, lambda r: r['status'] == 'running')
        queued = submit(server, body='queued before edit')['job_id']
        shared.write_text('QUEUED_SHARED_NEW', encoding='utf-8')
        private.write_text('QUEUED_PRIVATE_NEW', encoding='utf-8')
        server.rpc('cancel', {'job_id': first})
        result = await_watch(server, queued, lambda r: r['terminal'])
        assert result['status'] == 'completed'
        text = prompts(trace, 'codex')[-1]
        assert 'QUEUED_SHARED_NEW' in text and 'QUEUED_PRIVATE_NEW' in text
        assert 'SHARED_OLD' not in text and 'PRIVATE_OLD' not in text
    finally:
        server.close()
