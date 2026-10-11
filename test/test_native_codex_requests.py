"""Explicit responses bind the live task and never infer approvals or completion."""
import json
from pathlib import Path
import sys
import time

import pytest
from test_native_daemon import Server, submit, await_watch
from test_native_cli_security import cli
from test_native_pi_rpc import pi_spec

STUB = Path(__file__).parent / 'stubs' / 'codex_interactive.py'


def spec(tmp_path, mode):
    return dict(provider='codex', driver_kind='codex_app_server', profile_family='protocol_turn',
                command=[sys.executable, '-I', str(STUB), mode],
                env={'CCB_FIXTURE_TRACE': str(tmp_path / 'wire.jsonl')})


def pending(server):
    return server.rpc('provider-requests', {'agent_name': 'worker'})['requests']


def await_request(server):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        rows = pending(server)
        if rows:
            return rows[0]
        assert server.proc.poll() is None
        time.sleep(.025)
    pytest.fail('interactive request was not admitted')


def answer(request, result):
    return {**{key: request[key] for key in ['request_id', 'job_id', 'execution_instance_id', 'runtime_instance_id']},
            'agent_name': 'worker', 'result': result}


def wire(tmp_path):
    path = tmp_path / 'wire.jsonl'
    return [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines() if line] if path.exists() else []


@pytest.mark.parametrize('mode,result', [
    ('command', {'decision': 'decline'}), ('file', {'decision': 'acceptForSession'}),
    ('permissions', {'permissions': {'fileSystem': {'entries': [{'path': {'type': 'path', 'path': 'C:/fixture'}, 'access': 'read'}]}}, 'scope': 'turn'}),
    ('input', {'answers': {'choice': {'answers': ['SECRET_FIXTURE_ANSWER']}}}),
    ('mcp', {'action': 'accept', 'content': {'name': 'fixture'}}),
    ('mcp-url', {'action': 'decline', 'content': None}),
    ('dynamic', {'success': True, 'contentItems': [{'type': 'inputText', 'text': 'fixture output'}]}),
    ('late-accept', {'decision': 'decline'}), ('string-id', {'decision': 'decline'}),
    ('duplicate', {'decision': 'decline'}),
])
def test_explicit_response_unblocks_owned_request_without_implicit_completion(tmp_path, mode, result):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, mode)})
    try:
        receipt = submit(server)
        request = await_request(server)
        assert request['job_id'] == receipt['job_id'] and request['status'] == 'awaiting_user'
        assert len(pending(server)) == 1
        assert not [row for row in wire(tmp_path) if 'method' not in row]
        payload = answer(request, result)
        assert server.rpc('provider-respond', payload)['status'] == 'response_queued'
        assert server.rpc('provider-respond', payload)['status'] == 'response_queued'
        assert server.rpc('watch', {'target': receipt['job_id']})['status'] == 'running'
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'completed'
        assert pending(server) == []
        responses = [row for row in wire(tmp_path) if 'method' not in row]
        assert len(responses) == 1 and responses[0]['result'] == result
        assert responses[0]['id'] == ('string-request' if mode == 'string-id' else 201)
        for directory in [server.root, tmp_path / 'agents']:
            for file in directory.rglob('*.json*'):
                assert 'SECRET_FIXTURE_ANSWER' not in file.read_text(encoding='utf-8')
    finally:
        server.close()


@pytest.mark.parametrize('mode,result,error', [
    ('command', {'decision': 'autoAccept'}, 'response shape'),
    ('command', {'decision': 'acceptForSession'}, 'not offered'),
    ('permissions', {'permissions': {'network': {'enabled': True}}}, 'exceeds'),
    ('permissions', {'permissions': {'fileSystem': {'write': ['C:/outside']}}}, 'exceeds'),
    ('input', {'answers': {'unknown': {'answers': ['text']}}}, 'unknown question'),
    ('mcp', {'action': 'accept', 'content': {'name': 1}}, 'form answer'),
])
def test_invalid_answers_never_reach_provider_and_valid_request_remains_pending(tmp_path, mode, result, error):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, mode)})
    try:
        receipt = submit(server)
        request = await_request(server)
        with pytest.raises(AssertionError, match=error):
            server.rpc('provider-respond', answer(request, result))
        assert pending(server)[0]['status'] == 'awaiting_user'
        assert server.rpc('watch', {'target': receipt['job_id']})['status'] == 'running'
        assert not [row for row in wire(tmp_path) if 'method' not in row]
    finally:
        server.close()


@pytest.mark.parametrize('mode', ['foreign-thread', 'foreign-turn', 'malformed', 'unknown'])
def test_foreign_or_unsupported_requests_receive_errors_without_approval(tmp_path, mode):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, mode)})
    try:
        receipt = submit(server)
        await_watch(server, receipt['job_id'], lambda r: r['status'] == 'running')
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline and not [row for row in wire(tmp_path) if 'error' in row]:
            time.sleep(.025)
        errors = [row for row in wire(tmp_path) if 'error' in row]
        assert len(errors) == 1 and 'result' not in errors[0]
        assert pending(server) == []
        assert server.rpc('watch', {'target': receipt['job_id']})['status'] == 'running'
    finally:
        server.close()


def test_resolved_notification_clears_prompt_without_releasing_fifo(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, 'resolved-only')})
    try:
        receipt = submit(server)
        request = await_request(server)
        server.rpc('provider-respond', answer(request, {'decision': 'decline'}))
        time.sleep(.25)
        assert pending(server) == []
        assert server.rpc('watch', {'target': receipt['job_id']})['status'] == 'running'
        assert server.rpc('queue', {'target': 'worker'})['queues'][0]['active_job_id'] == receipt['job_id']
    finally:
        server.close()


def test_stale_user_response_cannot_answer_a_new_turn_reusing_wire_id(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, 'command')})
    try:
        first = submit(server)
        old = await_request(server)
        server.rpc('provider-respond', answer(old, {'decision': 'decline'}))
        await_watch(server, first['job_id'], lambda r: r['terminal'])
        second = submit(server)
        new = await_request(server)
        assert old['request_id'] != new['request_id']
        with pytest.raises(AssertionError, match='identity mismatch'):
            server.rpc('provider-respond', answer(old, {'decision': 'accept'}))
        assert pending(server)[0]['status'] == 'awaiting_user'
        server.rpc('provider-respond', answer(new, {'decision': 'decline'}))
        assert await_watch(server, second['job_id'], lambda r: r['terminal'])['status'] == 'completed'
    finally:
        server.close()


def test_cancellation_retires_user_prompt_and_rejects_a_late_answer(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, 'command')})
    try:
        receipt = submit(server)
        request = await_request(server)
        server.rpc('cancel', {'job_id': receipt['job_id']})
        with pytest.raises(AssertionError, match='no longer accepts|no active task|resolved'):
            server.rpc('provider-respond', answer(request, {'decision': 'accept'}))
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'cancelled'
        assert pending(server) == []
    finally:
        server.close()


def test_native_cli_pending_and_response_file_preserve_binding_and_hide_answer(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, 'input')})
    try:
        receipt = submit(server)
        await_request(server)
        shown = cli('--state-root', server.root, 'pending', 'worker')
        assert shown.returncode == 0, shown.stderr
        request = json.loads(shown.stdout)['requests'][0]
        answer_file = tmp_path / 'answer.json'
        answer_file.write_text(json.dumps({'answers': {'choice': {'answers': ['SECRET_FIXTURE_ANSWER']}}}), encoding='utf-8-sig')
        sent = cli('--state-root', server.root, 'respond', 'worker', request['request_id'], answer_file)
        assert sent.returncode == 0, sent.stderr
        assert 'SECRET_FIXTURE_ANSWER' not in sent.stdout + sent.stderr
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'completed'
    finally:
        server.close()


def test_wrong_live_bindings_cannot_forward_answer_and_do_not_poison_valid_response(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, 'command')})
    try:
        receipt = submit(server)
        request = await_request(server)
        valid = answer(request, {'decision': 'decline'})
        for field in ['runtime_instance_id', 'execution_instance_id', 'job_id']:
            with pytest.raises(AssertionError, match='identity mismatch'):
                server.rpc('provider-respond', {**valid, field: 'foreign'})
        assert not [row for row in wire(tmp_path) if 'method' not in row]
        assert pending(server)[0]['request_id'] == request['request_id']
        server.rpc('provider-respond', valid)
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'completed'
    finally:
        server.close()


def test_different_duplicate_answer_is_rejected_before_provider_resolution(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, 'resolved-only')})
    try:
        submit(server)
        request = await_request(server)
        server.rpc('provider-respond', answer(request, {'decision': 'decline'}))
        with pytest.raises(AssertionError, match='different answer'):
            server.rpc('provider-respond', answer(request, {'decision': 'accept'}))
        time.sleep(.15)
        assert len([row for row in wire(tmp_path) if 'method' not in row]) == 1
    finally:
        server.close()


def test_dead_provider_prompt_and_answer_are_not_replayed_after_restart(tmp_path):
    root = tmp_path / 'state'
    agents = {'worker': spec(tmp_path, 'command')}
    server = Server(root, agents)
    try:
        submit(server)
        old = await_request(server)
        server.proc.kill()
        server.proc.wait(timeout=4)
    finally:
        if server.proc.poll() is None:
            server.close()
    restored = Server(root, agents)
    try:
        assert await_watch(restored, old['job_id'], lambda r:r['terminal'])['status'] == 'incomplete'
        assert pending(restored) == []
        with pytest.raises(AssertionError, match='runtime identity mismatch'):
            restored.rpc('provider-respond', answer(old, {'decision': 'accept'}))
        assert not [row for row in wire(tmp_path) if 'method' not in row]
        assert len([row for row in wire(tmp_path) if row.get('method') == 'turn/start']) == 1
        assert restored.rpc('queue', {'target': 'worker'})['queues'][0]['active_job_id'] is None
    finally:
        restored.close()


def test_explicit_experimental_capabilities_are_forwarded_without_auto_tool_execution(tmp_path):
    configured = spec(tmp_path, 'dynamic')
    configured['capabilities'] = {'experimentalApi': True, 'mcpServerOpenaiFormElicitation': True}
    server = Server(tmp_path / 'state', {'worker': configured})
    try:
        receipt = submit(server)
        request = await_request(server)
        initialization = next(row for row in wire(tmp_path) if row.get('method') == 'initialize')
        assert initialization['params']['capabilities'] == configured['capabilities']
        assert not [row for row in wire(tmp_path) if 'method' not in row]
        server.rpc('provider-respond', answer(request, {'success': False, 'contentItems': []}))
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'completed'
    finally:
        server.close()


def test_worker_approval_does_not_block_lead_and_result_automatically_resumes_same_pi(tmp_path):
    server = Server(tmp_path / 'state', {'lead': pi_spec('lead'), 'worker': spec(tmp_path, 'command')})
    try:
        parent = submit(server, 'lead')
        request = await_request(server)
        await_watch(server, parent['job_id'], lambda r: r['visible_reply_source'] == 'callback_delegated_pending')
        assert server.rpc('queue', {'target': 'lead'})['queues'][0]['active_job_id'] is None
        before = json.loads((server.root / 'providers/pi/lead/session.json').read_text())
        assert before['state']['isStreaming'] is False
        server.rpc('provider-respond', answer(request, {'decision': 'decline'}))
        result = await_watch(server, parent['job_id'], lambda r: r['visible_reply_source'] == 'message_bureau_reply')
        assert result['reply'] == 'persistent Pi reviewed result'
        after = json.loads((server.root / 'providers/pi/lead/session.json').read_text())
        assert before['pid'] == after['pid']
        assert before['state']['sessionId'] == after['state']['sessionId']
        edges = server.rpc('records.latest', dict(stream='callbacks', identity='edge_id'), version=1)['result']
        assert len(edges) == 1 and edges[0]['state'] == 'done'
    finally:
        server.close()


def test_binding_metadata_is_not_published_before_thread_identity_is_known(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(tmp_path, 'slow-bind')})
    try:
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline and not [r for r in wire(tmp_path) if r.get('method') == 'thread/start']:
            time.sleep(.01)
        assert any(r.get('method') == 'thread/start' for r in wire(tmp_path))
        path = server.root / 'providers/codex/worker/session.json'
        assert not path.exists()
        while not path.exists() and time.monotonic() < deadline:
            time.sleep(.025)
        assert json.loads(path.read_text())['state']['thread_id'] == 'interactive-thread'
    finally:
        server.close()
