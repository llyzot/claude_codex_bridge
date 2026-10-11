"""Normal task commands operate directly on the native authority."""
import json
from pathlib import Path
import time

import pytest
from test_native_daemon import Server, spec, submit, await_watch
from test_native_cli_security import cli
from test_native_pi_rpc import pi_spec
from test_native_provider_faults import provider_spec
from test_native_execution_recovery import wait_input

def latest(server, agent='worker'):
    return server.rpc('records.latest', dict(stream='jobs', agent_name=agent, identity='job_id'), version=1)['result']

def invoke(server, *args, **kwargs):
    return cli('--state-root', server.root, *args, **kwargs)

def result(response):
    assert response.returncode == 0, response.stderr
    return json.loads(response.stdout)

def test_ask_prefix_options_stdin_sender_and_message_boundary(tmp_path):
    server = Server(tmp_path / 'state', {'worker':spec()})
    try:
        receipt = result(invoke(server, 'ask', '--compact', '--task-id', 'task-1', '--reply-to', 'root-1',
                                'worker', 'from', 'user', '--', 'review --chain literally', input='\n中文输入\n'))
        row = latest(server)[0]
        assert row['job_id'] == receipt['job_id']
        request = row['request']
        assert request['task_id'] == 'task-1' and request['reply_to'] == 'root-1'
        assert request['from_actor'] == 'user'
        assert 'review --chain literally' in request['body'] and '中文输入' in request['body']
        assert request['body'].endswith('CCB_REPLY_MODE: compact')
        assert request['route_options'].get('mode') is None
        inspected = result(invoke(server, 'ask', 'get', receipt['job_id']))
        assert inspected['job_id'] == receipt['job_id']
    finally: server.close()

@pytest.mark.parametrize('flag', ['--artifact-request', '--artifact-io', None])
def test_long_request_artifact_is_committed_with_admission(tmp_path, flag):
    server = Server(tmp_path / 'state', {'worker':spec()})
    try:
        body = '😀正文\n' * 1600
        flags = [flag] if flag else []
        receipt = result(invoke(server, 'ask', *flags, 'worker', input=body))
        row = latest(server)[0]
        artifact = row['request']['body_artifact']
        assert row['job_id'] == receipt['job_id']
        assert Path(artifact['path']).read_text(encoding='utf-8') == body.strip()
        assert len(row['request']['body'].encode('utf-8')) <= 4096
        assert artifact['path'] in row['request']['body']
        assert row['request']['route_options'].get('artifact_reply', False) is (flag == '--artifact-io')
    finally: server.close()

def test_invalid_or_oversized_inline_request_does_not_admit_or_leak_artifacts(tmp_path):
    server = Server(tmp_path / 'state', {'worker':spec()})
    try:
        assert invoke(server, 'ask', '--inline-request', 'worker', input='x' * 5000).returncode != 0
        assert invoke(server, 'ask', '--not-an-option', 'worker', 'text').returncode != 0
        assert invoke(server, 'ask', '--artifact-request', 'unknown', 'text').returncode != 0
        assert invoke(server, 'ask', '--mode', 'reply_delivery', 'worker', 'text').returncode != 0
        assert invoke(server, 'ask', 'worker', input='   ').returncode != 0
        assert latest(server) == []
        assert list(server.root.glob('artifacts/text/ask-request/*.txt')) == []
    finally: server.close()

def test_cancel_middle_queued_job_is_atomic_and_preserves_active_slot_and_fifo(tmp_path):
    server = Server(tmp_path / 'state', {'worker':spec()})
    try:
        first = submit(server)
        active = server.rpc('work', {'agent_name':'worker'})['job']
        second = submit(server)
        third = submit(server)
        assert result(invoke(server, 'ask', 'cancel', second['job_id'], '--silent'))['status'] == 'cancelled'
        assert result(invoke(server, 'cancel', second['job_id'], '--silent'))['status'] == 'cancelled'
        queue = server.rpc('queue', {'target':'worker'})['queues'][0]
        assert queue['active_job_id'] == first['job_id'] == active['job_id']
        assert queue['queued_job_ids'] == [third['job_id']]
        row = next(row for row in latest(server) if row['job_id'] == second['job_id'])
        assert row['terminal_decision']['diagnostics']['provider_prompt_sent'] is False
        events = server.rpc('records.list', dict(stream='job_events', agent_name='worker'), version=1)['result']
        assert len([e for e in events if e['job_id'] == second['job_id'] and e['type'] == 'job_cancelled']) == 1
        assert not [e for e in events if e['job_id'] == second['job_id'] and e['type'] == 'job_started']
        assert server.rpc('records.list', {'stream':'replies'}, version=1)['result'] == []
        retried = result(invoke(server, 'retry', second['job_id']))
        assert retried['job_id'] != second['job_id']
        assert server.rpc('queue', {'target':'worker'})['queues'][0]['queued_job_ids'] == [third['job_id'], retried['job_id']]
    finally: server.close()

def test_wait_observation_does_not_finish_or_restart_delegated_parent(tmp_path):
    server = Server(tmp_path / 'state', {'lead':pi_spec('lead'), 'worker':spec()})
    try:
        parent = submit(server, 'lead')
        pending = await_watch(server, parent['job_id'], lambda r:r['visible_reply_source'] == 'callback_delegated_pending')
        assert pending['terminal'] is False and pending['status'] == 'waiting_callback'
        observed = invoke(server, 'watch', parent['job_id'], '--wait', '--timeout-ms', '0')
        assert observed.returncode != 0
        assert json.loads(observed.stdout)['observation_timed_out'] is True
        child = latest(server)[0]
        assert len(latest(server)) == 1
        assert result(invoke(server, 'cancel', child['job_id']))['status'] == 'cancelled'
        final = result(invoke(server, 'watch', parent['job_id'], '--wait', '--timeout-ms', '5000'))
        assert final['visible_reply_source'] == 'message_bureau_reply'
        assert final['reply'] == 'persistent Pi reviewed result'
        assert final['terminal'] is True
    finally: server.close()

@pytest.mark.parametrize('flags,body', [
    (['--compact'], 'summarize the changes'),
    (['--silence'], 'perform the check'),
    (['--compact'], 'Only reply with the full report'),
    (['--compact','--silence'], 'inspect the result'),
])
def test_reply_guidance_matches_existing_cli_rules(tmp_path, flags, body):
    from cli.services.ask_runtime.submission import message_with_reply_guidance
    server = Server(tmp_path / 'state', {'worker':spec()})
    try:
        result(invoke(server, 'ask', *flags, 'worker', body))
        expected = message_with_reply_guidance(body, message_type='ask', compact='--compact' in flags, silence_on_success='--silence' in flags)
        assert latest(server)[0]['request']['body'] == expected
    finally: server.close()

def test_native_sender_extension_preserves_from_at_start_of_message(tmp_path):
    server = Server(tmp_path / 'state', {'worker':spec()})
    try:
        result(invoke(server, 'ask', 'worker', '--from', 'user', 'from the newest revision'))
        assert latest(server)[0]['request']['body'] == 'from the newest revision'
        assert latest(server)[0]['request']['from_actor'] == 'user'
    finally: server.close()

def test_artifact_io_preserves_request_and_forces_reply_artifact(tmp_path):
    server = Server(tmp_path / 'state', {'worker':spec('worker')})
    try:
        receipt = result(invoke(server, 'ask', '--artifact-io', 'worker', 'small request'))
        await_watch(server, receipt['job_id'], lambda r:r['terminal'])
        row = latest(server)[0]
        assert Path(row['request']['body_artifact']['path']).read_text() == 'small request'
        reply = row['terminal_decision']['diagnostics']['reply_artifact']
        assert Path(reply['path']).read_text() == 'worker result'
    finally: server.close()

def finish(server, job, status='completed', reply='result'):
    decision = dict(schema_version=2, record_type='completion_decision', terminal=True, status=status,
                    reason='test_completed' if status=='completed' else 'permission_denied', confidence='observed',
                    reply=reply, anchor_seen=True, reply_started=True, reply_stable=True, provider_turn_ref=None,
                    source_cursor=None, finished_at=job['updated_at'], diagnostics={})
    return server.rpc('complete', dict(job_id=job['job_id'], agent_name=job['agent_name'],
                      execution_instance_id=job['provider_options']['native_execution_instance'], decision=decision))

def test_watch_reports_final_review_failure_not_initial_lead_success(tmp_path):
    server = Server(tmp_path / 'state', {'lead':spec(), 'worker':spec()})
    try:
        parent = submit(server, 'lead')
        initial = server.rpc('work', {'agent_name':'lead'})['job']
        server.rpc('submit', dict(project_id='native-test', to_agent='worker', from_actor='lead', body='child', route_options={'mode':'chain'}))
        finish(server, initial, reply='delegated')
        assert server.rpc('watch', {'target':parent['job_id']})['terminal'] is False
        child = server.rpc('work', {'agent_name':'worker'})['job']
        finish(server, child)
        continuation = server.rpc('work', {'agent_name':'lead'})['job']
        finish(server, continuation, status='failed', reply='review failed')
        view = result(invoke(server, 'watch', parent['job_id'], '--wait', '--timeout-ms', '1000'))
        assert view['terminal'] is True and view['status'] == 'failed'
        assert view['job_status'] == 'completed'
        assert view['visible_reply_source'] == 'message_bureau_reply'
    finally: server.close()

def test_queued_cancellation_survives_restart_and_does_not_consume_next_request(tmp_path):
    root = tmp_path / 'state'
    agents = {'worker':spec()}
    server = Server(root, agents)
    try:
        first = submit(server)
        second = submit(server)
        result(invoke(server, 'cancel', first['job_id']))
        before = server.rpc('records.list', {'stream':'replies'}, version=1)['result']
        assert len(before) == 1
    finally: server.close()
    restored = Server(root, agents)
    try:
        result(invoke(restored, 'cancel', first['job_id']))
        assert restored.rpc('records.list', {'stream':'replies'}, version=1)['result'] == before
        assert restored.rpc('work', {'agent_name':'worker'})['job']['job_id'] == second['job_id']
    finally: restored.close()

@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
def test_cli_running_cancel_returns_receipt_without_inventing_completion(tmp_path, provider):
    server = Server(tmp_path / 'state', {'worker':provider_spec(provider, 'hold', tmp_path)})
    try:
        receipt = result(invoke(server, 'ask', 'worker', 'hold this task'))
        wait_input(tmp_path, server)
        cancelled = result(invoke(server, 'cancel', receipt['job_id'], '--silent'))
        assert cancelled['status'] == 'cancellation_requested'
        row = latest(server)[0]
        assert row['provider_options']['native_cancel_intent']['record_reply'] is False
        assert server.rpc('watch', {'target':receipt['job_id']})['status'] == 'running'
        assert len(latest(server)) == 1
        assert server.rpc('records.list', {'stream':'replies'}, version=1)['result'] == []
    finally: server.close()

def test_failed_chain_admission_rolls_back_generated_request_artifact(tmp_path):
    server = Server(tmp_path / 'state', {'worker':spec()})
    try:
        rejected = invoke(server, 'ask', '--chain', '--artifact-request', 'worker', 'unowned chain request')
        assert rejected.returncode != 0
        assert latest(server) == []
        assert list(server.root.glob('artifacts/text/ask-request/*.txt')) == []
        assert server.rpc('records.list', {'stream':'messages'}, version=1)['result'] == []
    finally: server.close()

def test_wait_timeout_keeps_the_same_running_job_and_cursor_observation_is_read_only(tmp_path):
    server = Server(tmp_path / 'state', {'worker':spec()})
    try:
        receipt = submit(server)
        job = server.rpc('work', {'agent_name':'worker'})['job']
        first = result(invoke(server, 'watch', receipt['job_id']))
        second = result(invoke(server, 'watch', receipt['job_id'], '--cursor', str(first['cursor'])))
        assert second['events'] == []
        expired = invoke(server, 'watch', receipt['job_id'], '--wait', '--timeout-ms', '150')
        assert expired.returncode != 0
        assert json.loads(expired.stdout)['job_id'] == job['job_id']
        assert len(latest(server)) == 1
        assert server.rpc('queue', {'target':'worker'})['queues'][0]['active_job_id'] == job['job_id']
    finally: server.close()
