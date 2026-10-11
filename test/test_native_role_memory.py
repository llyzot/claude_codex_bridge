"""Live role memory crosses real native provider transports without copied trees."""
import hashlib
import json
from pathlib import Path
import time

import pytest
from test_native_daemon import Server, submit, await_watch, spec
from test_native_pi_rpc import pi_spec
from test_native_codex_rpc import codex_spec
from test_native_claude_stream import claude_spec
from test_native_task_cli import finish

FACTORIES = {'pi': lambda: pi_spec('worker'), 'codex': codex_spec, 'claude': claude_spec}

def install(project, schema='rolepack/v1', name='agentroles.reader'):
    root = project / 'roles/installed' / name / 'current'
    root.mkdir(parents=True)
    header = f'schema="{schema}"\nid="{name}"\nname="Reader"\nversion="1"\ndescription="Fixture role"\n'
    if schema.startswith('agent-role/'):
        memory = '[contents]\nmemory=["memory.md"]\n'
        (root / 'adapters/ccb').mkdir(parents=True)
        (root / 'adapters/ccb/adapter.toml').write_text('memory=["adapters/ccb/memory.md"]\n', encoding='utf-8')
        (root / 'adapters/ccb/memory.md').write_text('adapter live memory', encoding='utf-8')
    else:
        memory = '[memory]\nfiles=["memory.md"]\nmerge_strategy="append_after_project_memory"\n'
    (root / 'role.toml').write_text(header + memory, encoding='utf-8')
    (root / 'memory.md').write_text('ROLE_MEMORY_OLD 中文', encoding='utf-8')
    return root

def configured(provider, project, trace, role='agentroles.reader'):
    return dict(FACTORIES[provider](), role=role, workspace_path=str(project),
                env={'CCB_FIXTURE_TRACE': str(trace)})

def prompts(trace, provider):
    if not trace.exists(): return []
    rows = [json.loads(line) for line in trace.read_text(encoding='utf-8').splitlines()]
    if provider == 'pi': return [r['message'] for r in rows if r.get('type') == 'prompt']
    if provider == 'codex': return [r['params']['input'][0]['text'] for r in rows if r.get('method') == 'turn/start']
    return [r['message']['content'] for r in rows if r.get('type') == 'user']

@pytest.mark.parametrize('provider', FACTORIES)
@pytest.mark.parametrize('schema', ['rolepack/v1', 'agent-role/preview-0.1'])
def test_live_memory_reloads_on_same_provider_session_without_persisting_contents(tmp_path, provider, schema):
    root = install(tmp_path, schema)
    trace = tmp_path / 'wire.jsonl'
    server = Server(tmp_path / 'state', {'worker': configured(provider, tmp_path, trace)})
    try:
        before = None
        for index, content in enumerate(['ROLE_MEMORY_OLD 中文', 'ROLE_MEMORY_NEW 中文']):
            (root / 'memory.md').write_text(content, encoding='utf-8')
            receipt = submit(server, body=f'task {index}')
            row = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
            assert row['status'] == 'completed', row
            sent = prompts(trace, provider)
            assert len(sent) == index + 1 and content in sent[-1]
            assert sent[-1].startswith(f"CCB_REQ_ID: {receipt['job_id']}\ntask {index}")
            if index: assert 'ROLE_MEMORY_OLD' not in sent[-1]
            if schema.startswith('agent-role/'):
                assert sent[-1].index(content) < sent[-1].index('adapter live memory')
            session = json.loads((server.root / f'providers/{provider}/worker/session.json').read_text())
            if before: assert (before['pid'], before['runtime_instance_id']) == (session['pid'], session['runtime_instance_id'])
            before = session
        events = server.rpc('records.list', {'stream': 'job_events', 'agent_name': 'worker'}, version=1)['result']
        audit = [e for e in events if e['type'] == 'role_context_loaded']
        assert len(audit) == 2
        assert audit[-1]['payload']['sources'][0]['sha256'] == hashlib.sha256('ROLE_MEMORY_NEW 中文'.encode()).hexdigest()
        # Native records may contain hashes/paths, but never a duplicate role memory body.
        records = list(server.root.parent.joinpath('agents').rglob('*.jsonl'))
        assert records
        for path in records:
            assert 'ROLE_MEMORY_' not in path.read_text(encoding='utf-8')
        assert not (server.root / 'roles').exists()
        # Removing the declaration produces an explicit empty current context.
        manifest = root / 'role.toml'
        manifest.write_text(manifest.read_text().replace('["memory.md"]', '[]'), encoding='utf-8')
        receipt = submit(server)
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'completed'
        assert 'ROLE_MEMORY_' not in prompts(trace, provider)[-1]
        assert 'replacing earlier CCB role memory' in prompts(trace, provider)[-1]
    finally: server.close()

@pytest.mark.parametrize('kind', ['missing', 'escape', 'absolute', 'bad-utf8', 'oversize', 'many-files',
                                  'bad-manifest', 'bad-table', 'bad-list', 'bad-item', 'bad-schema', 'bad-strategy'])
def test_bad_live_memory_fails_only_task_and_can_be_fixed_without_restart(tmp_path, kind):
    root = install(tmp_path)
    manifest = root / 'role.toml'
    original = manifest.read_text()
    if kind == 'missing': (root / 'memory.md').unlink()
    elif kind == 'bad-utf8': (root / 'memory.md').write_bytes(b'\xff\xfe')
    elif kind == 'oversize': (root / 'memory.md').write_bytes(b'x' * (256 * 1024 + 1))
    elif kind == 'bad-manifest': manifest.write_text('SECRET_NEVER_IN_RECORDS = [')
    else:
        change = {'escape': '["../outside.md"]', 'absolute': '["C:/outside.md"]',
                  'many-files': json.dumps(['memory.md'] * 65), 'bad-list': '"memory.md"', 'bad-item': '[123]'}
        if kind in change: manifest.write_text(original.replace('["memory.md"]', change[kind]))
        elif kind == 'bad-table': manifest.write_text('id="agentroles.reader"\nmemory="bad"')
        elif kind == 'bad-schema': manifest.write_text(original.replace('rolepack/v1', 'rolepack/v999'))
        elif kind == 'bad-strategy': manifest.write_text(original.replace('append_after_project_memory', 'replace_project'))
    trace = tmp_path / 'wire.jsonl'
    server = Server(tmp_path / 'state', {'worker': configured('pi', tmp_path, trace), 'peer': spec('worker')})
    try:
        receipt = submit(server)
        row = await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        assert row['status'] == 'failed' and row['completion_reason'] == 'role_context_invalid', row
        assert prompts(trace, 'pi') == []
        before = json.loads((server.root / 'providers/pi/worker/session.json').read_text())
        peer = submit(server, 'peer')
        assert await_watch(server, peer['job_id'], lambda r: r['terminal'])['status'] == 'completed'
        manifest.write_text(original, encoding='utf-8')
        (root / 'memory.md').write_text('REPAIRED', encoding='utf-8')
        receipt = submit(server)
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'completed'
        assert len(prompts(trace, 'pi')) == 1 and 'REPAIRED' in prompts(trace, 'pi')[0]
        after = json.loads((server.root / 'providers/pi/worker/session.json').read_text())
        assert before['runtime_instance_id'] == after['runtime_instance_id']
        jobs = server.rpc('records.latest', {'stream':'jobs', 'agent_name':'worker', 'identity':'job_id'}, version=1)['result']
        assert len(jobs) == 2  # No automatic replay of a configuration failure.
        assert 'SECRET_NEVER_IN_RECORDS' not in json.dumps(jobs)
    finally: server.close()

def test_automatic_pi_lead_callback_reads_latest_memory(tmp_path):
    root = install(tmp_path)
    trace = tmp_path / 'lead-wire.jsonl'
    lead = dict(pi_spec('lead'), role='agentroles.reader', workspace_path=str(tmp_path),
                env={'CCB_FIXTURE_TRACE': str(trace)})
    # Manual Worker gives the test a precise edit point after delegation.
    server = Server(tmp_path / 'state', {'lead': lead, 'worker': spec()})
    try:
        receipt = submit(server, 'lead')
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            jobs = server.rpc('records.latest', {'stream':'jobs','agent_name':'worker','identity':'job_id'}, version=1)['result']
            if jobs: break
            time.sleep(.025)
        assert len(jobs) == 1
        (root / 'memory.md').write_text('CALLBACK_NEW_MEMORY', encoding='utf-8')
        active = server.rpc('work', {'agent_name':'worker'})['job']
        finish(server, active, reply='worker result')
        row = await_watch(server, receipt['job_id'], lambda r:r['visible_reply_source']=='message_bureau_reply')
        assert row['reply'] == 'persistent Pi reviewed result'
        sent = prompts(trace, 'pi')
        assert len(sent) == 2 and 'ROLE_MEMORY_OLD' in sent[0]
        assert 'CALLBACK_NEW_MEMORY' in sent[1] and 'ROLE_MEMORY_OLD' not in sent[1]
        metadata = json.loads((server.root / 'providers/pi/lead/session.json').read_text())
        assert metadata['state']['messageCount'] == 4
    finally: server.close()
