"""Inbox observation is read-only; explicit FIFO ack never releases active work."""
import json
import pytest
from test_native_daemon import Server, spec, submit
from test_native_project_view import finish, data_hashes
from test_native_cli_security import cli


def reply(server):
    submit(server, actor='lead')
    finish(server, server.rpc('work', {'agent_name': 'worker'})['job'])
    return server.rpc('inbox', {'agent_name': 'lead'})['head']


def mailbox_files(server):
    return {str(p): p.read_bytes() for p in (server.root / 'mailboxes').rglob('*') if p.is_file()}


def test_result_persists_for_disabled_recipient_across_controller_restart(tmp_path):
    root=tmp_path/'state'
    agents={'worker':spec(),'lead':dict(spec(),dispatch_disabled=True)}
    server=Server(root,agents)
    try:
        task=submit(server,actor='lead')
        finish(server,server.rpc('work',{'agent_name':'worker'})['job'])
        assert server.rpc('watch',{'target':task['job_id']})['status']=='completed'
        inbox=server.rpc('inbox',{'agent_name':'lead','detail':True})
        assert inbox['item_count']==1 and inbox['head']['reply']=='worker result'
        event=inbox['head']['inbound_event_id']
        with pytest.raises(AssertionError,match='dispatch-disabled'):
            server.rpc('work',{'agent_name':'lead'})
    finally:server.close()
    restored=Server(root,agents)
    try:
        inbox=restored.rpc('inbox',{'agent_name':'lead','detail':True})
        assert inbox['item_count']==1 and inbox['head']['inbound_event_id']==event
        assert inbox['head']['reply']=='worker result'
    finally:restored.close()


def test_fifo_ack_replay_survives_restart_and_does_not_consume_next(tmp_path):
    root = tmp_path / 'state'
    agents = {'worker': spec(), 'lead': spec()}
    server = Server(root, agents)
    try:
        first = reply(server)
        reply(server)
        before = data_hashes(server)
        cached_before = mailbox_files(server)
        default = server.rpc('inbox', {'agent_name': 'lead'})
        detail = server.rpc('inbox', {'agent_name': 'lead', 'detail': True})
        assert data_hashes(server) == before
        assert mailbox_files(server) == cached_before
        assert default['items'] == [] and default['item_count'] == 2
        assert default['agent']['mailbox_state'] == 'blocked'
        assert detail['head']['reply'] == 'worker result'
        assert detail['head']['source_actor'] == 'worker'
        assert len(detail['events']) == len(detail['items']) == 2
        assert 'reply' not in detail['items'][1]
        second = detail['items'][1]['inbound_event_id']
        with pytest.raises(AssertionError, match='FIFO head'):
            server.rpc('ack', {'agent_name': 'lead', 'inbound_event_id': second})
        assert data_hashes(server) == before
        ack = server.rpc('ack', {'agent_name': 'lead', 'inbound_event_id': first['inbound_event_id']})
        assert ack['reply'] == 'worker result' and not ack['already_acknowledged']
        assert ack['next_inbound_event_id'] == second
        assert ack['mailbox']['queue_depth'] == 1
        before = data_hashes(server)
        assert server.rpc('ack', {'agent_name': 'lead', 'inbound_event_id': first['inbound_event_id']})['already_acknowledged']
        assert data_hashes(server) == before
    finally:
        server.close()
    restored = Server(root, agents)
    try:
        ack = restored.rpc('ack', {'agent_name': 'lead', 'inbound_event_id': first['inbound_event_id']})
        assert ack['already_acknowledged'] and ack['next_inbound_event_id'] == second
        result = cli('--state-root', root, 'ack', 'lead')
        assert result.returncode == 0, result.stderr
        assert json.loads(result.stdout)['acknowledged_inbound_event_id'] == second
        assert restored.rpc('inbox', {'agent_name': 'lead'})['item_count'] == 0
    finally:
        restored.close()


@pytest.mark.parametrize('running', [False, True])
def test_live_task_request_cannot_be_acknowledged(tmp_path, running):
    server = Server(tmp_path / 'state', {'worker': spec()})
    try:
        submitted = submit(server)
        if running:
            server.rpc('work', {'agent_name': 'worker'})
        before = data_hashes(server)
        with pytest.raises(AssertionError, match='terminal task_request|active task'):
            server.rpc('ack', {'agent_name': 'worker'})
        assert data_hashes(server) == before
        assert server.rpc('watch', {'target': submitted['job_id']})['status'] == ('running' if running else 'accepted')
    finally:
        server.close()


def test_automatic_delivery_cannot_be_acknowledged(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(), 'lead': spec()})
    try:
        reply(server)
        delivery = server.rpc('work', {'agent_name': 'lead'})['job']
        assert delivery['request']['message_type'] == 'reply_delivery'
        before = data_hashes(server)
        with pytest.raises(AssertionError, match='automatic reply delivery'):
            server.rpc('ack', {'agent_name': 'lead'})
        assert data_hashes(server) == before
        assert server.rpc('watch', {'target': delivery['job_id']})['status'] == 'running'
        finish(server, delivery)
        assert server.rpc('inbox', {'agent_name': 'lead'})['item_count'] == 0
    finally:
        server.close()


def test_stale_cancelled_request_is_only_retired_by_explicit_ack(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec()})
    try:
        receipt = submit(server)
        event = server.rpc('inbox', {'agent_name': 'worker'})['events'][0]
        server.rpc('cancel', {'job_id': receipt['job_id']})
        server.rpc('inbox.append', {'agent_name': 'worker', 'record': event}, version=1)
        before = data_hashes(server)
        view = server.rpc('inbox', {'agent_name': 'worker', 'detail': True})
        assert view['head']['attempt_state'] == 'cancelled'
        assert view['item_count'] == 1 and view['summary_status'] == 'stale'
        assert data_hashes(server) == before
        ack = server.rpc('ack', {'agent_name': 'worker', 'inbound_event_id': event['inbound_event_id']})
        assert ack['event_status'] == 'abandoned' and ack['mailbox']['queue_depth'] == 0
    finally:
        server.close()


@pytest.mark.parametrize('broken', ['missing_attempt', 'foreign_job', 'mismatched_reply'])
def test_invalid_lineage_refuses_ack_without_writes(tmp_path, broken):
    server = Server(tmp_path / 'state', {'worker': spec(), 'lead': spec()})
    try:
        head = reply(server)
        if broken == 'missing_attempt':
            event = server.rpc('inbox', {'agent_name': 'lead'})['events'][0]
            event['attempt_id'] = 'att_missing'
            server.rpc('inbox.append', {'agent_name': 'lead', 'record': event}, version=1)
        elif broken == 'foreign_job':
            row = server.rpc('records.latest', {'stream': 'jobs', 'agent_name': 'worker', 'identity': 'job_id'}, version=1)['result'][0]
            row['request']['project_id'] = 'foreign'
            server.rpc('records.append', {'stream': 'jobs', 'agent_name': 'worker', 'record': row}, version=1)
        else:
            row = server.rpc('records.latest', {'stream': 'replies', 'identity': 'reply_id'}, version=1)['result'][0]
            row['attempt_id'] = 'att_other'
            server.rpc('records.append', {'stream': 'replies', 'record': row}, version=1)
        before = data_hashes(server)
        with pytest.raises(AssertionError, match='lineage|another project'):
            server.rpc('ack', {'agent_name': 'lead', 'inbound_event_id': head['inbound_event_id']})
        assert data_hashes(server) == before
    finally:
        server.close()


def test_inbox_cli_validation_and_control_escaping(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(), 'lead': spec()})
    try:
        reply(server)
        row = server.rpc('records.latest', {'stream': 'replies', 'identity': 'reply_id'}, version=1)['result'][0]
        row['reply'] = 'control\u202evalue'
        server.rpc('records.append', {'stream': 'replies', 'record': row}, version=1)
        result = cli('--state-root', server.root, 'inbox', '--detail', 'lead')
        assert result.returncode == 0, result.stderr
        assert '\u202e' not in result.stdout and '\\u202e' in result.stdout
        assert json.loads(result.stdout)['head']['reply'] == row['reply']
        for args in [('inbox', 'lead', 'extra'), ('inbox', 'lead', '--detail', '--detail'), ('ack', 'lead', 'one', 'two')]:
            assert cli('--state-root', server.root, *args).returncode != 0
    finally:
        server.close()


def test_other_event_lease_is_not_released(tmp_path):
    server = Server(tmp_path / 'state', {'worker': spec(), 'lead': spec()})
    try:
        head = reply(server)
        server.rpc('claim', {'agent_name': 'lead', 'inbound_event_id': head['inbound_event_id'], 'timestamp': '2026-10-12T12:00:00Z'}, version=1)
        lease = server.rpc('lease.load', {'agent_name': 'lead'}, version=1)['result']
        lease['inbound_event_id'] = 'in_other'
        server.rpc('lease.save', {'agent_name': 'lead', 'record': lease}, version=1)
        before = data_hashes(server)
        with pytest.raises(AssertionError, match='another inbound event'):
            server.rpc('ack', {'agent_name': 'lead'})
        assert server.rpc('lease.load', {'agent_name': 'lead'}, version=1)['result'] == lease
        assert data_hashes(server) == before
    finally:
        server.close()


def test_lost_socket_ack_is_retried_by_exact_event_identity(tmp_path):
    import socket
    from ccbd.control_plane_transport.token_auth import client_authenticate
    server = Server(tmp_path / 'state', {'worker': spec(), 'lead': spec()})
    try:
        first = reply(server)
        reply(server)
        args = {'agent_name': 'lead', 'inbound_event_id': first['inbound_event_id']}
        with socket.create_connection((server.endpoint['host'], server.endpoint['port']), timeout=3) as stream:
            client_authenticate(stream, server.token.token)
            stream.sendall((json.dumps({'api_version': 2, 'op': 'ack', 'request': args}) + '\n').encode())
            stream.shutdown(socket.SHUT_WR)
            # Deliberately discard the complete response: the client never observes success.
            while stream.recv(65536):
                pass
        result = server.rpc('ack', args)
        assert result['already_acknowledged'] and result['mailbox']['queue_depth'] == 1
        assert result['next_inbound_event_id'] != first['inbound_event_id']
        receipts = (server.root / 'mailbox-acks.jsonl').read_text(encoding='utf-8').splitlines()
        assert len(receipts) == 1 and 'worker result' not in receipts[0]
    finally:
        server.close()


def test_inbox_items_match_legacy_projection(tmp_path):
    from types import SimpleNamespace
    from mailbox_kernel import InboundEventRecord
    from message_bureau.models import MessageRecord, AttemptRecord, ReplyRecord
    from message_bureau.control_queue_runtime.events import inbox_item_summary
    server = Server(tmp_path / 'state', {'worker': spec(), 'lead': spec()})
    try:
        reply(server)
        reply(server)
        stores = {}
        for stream, key, model in [('messages', 'message_id', MessageRecord), ('attempts', 'attempt_id', AttemptRecord), ('replies', 'reply_id', ReplyRecord)]:
            records = server.rpc('records.latest', {'stream': stream, 'identity': key}, version=1)['result']
            values = {row[key]: model.from_record(row) for row in records}
            stores[f'_{key.removesuffix("_id")}_store'] = SimpleNamespace(get_latest=values.get)
        service = SimpleNamespace(**stores)
        view = server.rpc('inbox', {'agent_name': 'lead', 'detail': True})
        for position, (event, actual) in enumerate(zip(view['events'], view['items']), 1):
            expected = inbox_item_summary(service, InboundEventRecord.from_record(event), position=position)
            assert {key: actual[key] for key in expected} == expected
    finally:
        server.close()
