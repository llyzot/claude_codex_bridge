"""Operator snapshots distinguish incoming replies from an agent's own job."""
import json
from types import SimpleNamespace
import pytest
from test_native_daemon import Server, spec, submit
from test_native_project_view import data_hashes
from test_native_mailbox_control import mailbox_files
from test_native_cli_security import cli


def finish(server, job, reply):
    decision = dict(schema_version=2, record_type='completion_decision', terminal=True, status='completed', reason='task_complete',
                    confidence='observed', reply=reply, anchor_seen=True, reply_started=True, reply_stable=True,
                    provider_turn_ref=None, source_cursor=None, finished_at='2026-10-12T12:00:00Z', diagnostics={})
    server.rpc('complete', dict(job_id=job['job_id'], agent_name=job['agent_name'],
                               execution_instance_id=job['provider_options']['native_execution_instance'], decision=decision))


def incoming(server, body='NEW_INCOMING'):
    receipt = submit(server, actor='lead')
    finish(server, server.rpc('work', {'agent_name': 'worker'})['job'], body)
    return receipt['job_id']


def invoke(server, *args):
    result = cli('--state-root', server.root, *args)
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


def test_pend_overlays_incoming_fifo_reply_without_replacing_own_job(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(), 'lead': spec()})
    try:
        own = submit(server, 'lead')['job_id']
        finish(server, server.rpc('work', {'agent_name': 'lead'})['job'], 'OLD_OWN_REPLY')
        child = incoming(server)
        before = data_hashes(server); files = mailbox_files(server)
        row = invoke(server, 'pend', 'lead')
        assert row['job_id'] == own and row['reply'] == 'OLD_OWN_REPLY'
        assert row['mailbox_reply_ready'] and row['mailbox_reply'] == 'NEW_INCOMING'
        assert row['mailbox_reply_job_id'] == child and row['mailbox_reply_from_agent'] == 'worker'
        from cli.services.pend import _mailbox_overlay_fields
        head = server.rpc('inbox', {'agent_name': 'lead'})['head']
        expected = _mailbox_overlay_fields(head)
        for key, value in expected.items():
            if key not in ['mailbox_summary_status', 'mailbox_summary_error']:
                assert row[key] == value
        assert invoke(server, 'pend', 'lead', '3')['mailbox_reply_id'] == row['mailbox_reply_id']
        by_job = invoke(server, 'pend', own)
        assert by_job['reply'] == 'OLD_OWN_REPLY' and 'mailbox_reply' not in by_job
        assert data_hashes(server) == before and mailbox_files(server) == files
    finally:
        server.close()


def test_pend_agent_without_jobs_can_show_mailbox_reply_and_ack_transition(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(), 'lead': spec()})
    try:
        empty = invoke(server, 'pend', 'lead')
        assert empty['status'] == 'unknown' and not empty['mailbox_reply_ready']
        child = incoming(server)
        row = invoke(server, 'pend', 'lead')
        assert row['status'] == 'mailbox_reply' and row['job'] is None and row['job_id'] == child
        assert row['mailbox_reply'] == 'NEW_INCOMING' and not row['terminal']
        server.rpc('ack', {'agent_name': 'lead', 'inbound_event_id': row['mailbox_inbound_event_id']})
        assert not invoke(server, 'pend', 'lead')['mailbox_reply_ready']
    finally:
        server.close()


def test_queue_summary_matches_legacy_and_detail_preserves_active_and_fifo(tmp_path):
    from mailbox_kernel import MailboxRecord
    from message_bureau.control_queue_runtime.views_runtime.summary import queue_summary
    server = Server(tmp_path / 'state', {'worker': spec(), 'lead': spec()})
    try:
        first = submit(server)['job_id']; second = submit(server)['job_id']
        server.rpc('work', {'agent_name': 'worker'})
        before = data_hashes(server); files = mailbox_files(server)
        summary = invoke(server, 'queue', 'all')
        assert server.rpc('queue', {'target': 'all', 'detail': None})['agents'] == summary['agents']
        assert server.rpc('inbox', {'agent_name': 'worker', 'detail': None})['items'] == []
        cached = {name: server.rpc('mailbox.load', {'agent_name': name}, version=1)['result'] for name in ['lead', 'worker']}
        service = SimpleNamespace(_known_mailboxes=('lead', 'worker'), _config=SimpleNamespace(agents={'lead': None, 'worker': None}),
                                  _mailbox_store=SimpleNamespace(load=lambda name: MailboxRecord.from_record(cached[name]) if cached[name] else None))
        expected = queue_summary(service, 'all')
        assert {k: summary[k] for k in expected} == expected
        row = invoke(server, 'queue', '--detail', 'worker')
        assert row['queues'][0]['active_job_id'] == first and row['queues'][0]['queued_job_ids'] == [second]
        assert row['agent']['active']['job_id'] == first
        assert [r['job_id'] for r in row['agent']['queued_events']] == [first, second]
        alias = invoke(server, 'pend', '--queue', '--detail', 'worker')
        assert alias['agent'] == row['agent']
        assert data_hashes(server) == before and mailbox_files(server) == files
    finally:
        server.close()


def test_queue_summary_uses_projection_without_decoding_corrupt_inbox(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec()})
    try:
        first = submit(server)['job_id']
        inbox = server.root / 'mailboxes/worker/inbox.jsonl'
        inbox.write_text('deliberately corrupt fixture\n', encoding='utf-8')
        before = data_hashes(server)
        row = invoke(server, 'queue', 'worker')
        assert row['queues'][0]['queued_job_ids'] == [first] and row['agent']['queue_depth'] == 1
        assert cli('--state-root', server.root, 'queue', '--detail', 'worker').returncode != 0
        assert data_hashes(server) == before
    finally:
        server.close()


def test_pend_inbox_detail_does_not_ack_or_skip_reply_under_delivery(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(), 'lead': spec()})
    try:
        incoming(server)
        delivery = server.rpc('work', {'agent_name': 'lead'})['job']
        before = data_hashes(server); files = mailbox_files(server)
        row = invoke(server, 'pend', 'lead')
        assert row['mailbox_reply_ready'] and row['mailbox_inbound_event_status'] == 'delivering'
        inbox = invoke(server, 'pend', '--inbox', 'lead', '--detail')
        assert inbox['head']['inbound_event_id'] == row['mailbox_inbound_event_id']
        assert server.rpc('queue', {'target': 'lead'})['queues'][0]['active_job_id'] == delivery['job_id']
        assert data_hashes(server) == before and mailbox_files(server) == files
    finally:
        server.close()


def test_pend_watch_timeout_keeps_job_and_rejects_ambiguous_flags(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec()})
    try:
        job = submit(server)['job_id']; before = data_hashes(server)
        result = cli('--state-root', server.root, 'pend', '--watch', job, '--timeout-ms', '0')
        assert result.returncode != 0 and json.loads(result.stdout)['observation_timed_out']
        assert server.rpc('watch', {'target': job})['status'] == 'accepted'
        for args in [('pend', '--inbox', '--queue', 'worker'), ('pend', '--detail', 'worker'),
                     ('pend', '--queue', 'worker', '2'), ('pend', 'worker', '0'),
                     ('pend', '--inbox', 'worker', '--timeout-ms', '0'), ('queue', 'worker', 'extra'), ('queue', '--unknown', 'worker')]:
            assert cli('--state-root', server.root, *args).returncode != 0, args
        assert data_hashes(server) == before
    finally:
        server.close()


def test_pending_task_head_is_not_skipped_to_show_a_later_reply(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(), 'lead': spec()})
    try:
        parent = submit(server, 'lead')['job_id']
        incoming(server)
        before = data_hashes(server)
        row = invoke(server, 'pend', 'lead')
        assert row['job_id'] == parent and not row['mailbox_reply_ready']
        queue = invoke(server, 'queue', 'lead', '--detail')
        assert [r['event_type'] for r in queue['agent']['queued_events']] == ['task_request', 'task_reply']
        assert queue['agent']['pending_reply_count'] == 1
        assert data_hashes(server) == before
    finally:
        server.close()


def test_invalid_summary_is_reported_without_overwriting_or_hiding_scheduler(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(), 'lead': spec()})
    try:
        child = incoming(server)
        path = server.root / 'mailboxes/lead/mailbox.json'
        row = json.loads(path.read_text(encoding='utf-8')); row['schema_version'] = 999
        path.write_text(json.dumps(row), encoding='utf-8'); content = path.read_bytes()
        queue = invoke(server, 'queue', 'lead')
        assert queue['agent']['summary_status'] == 'error' and queue['queues'][0]['active_job_id'] is None
        pend = invoke(server, 'pend', 'lead')
        assert pend['mailbox_summary_status'] == 'error' and pend['mailbox_reply_job_id'] == child
        assert pend['mailbox_reply'] == 'NEW_INCOMING' and path.read_bytes() == content
    finally:
        server.close()
