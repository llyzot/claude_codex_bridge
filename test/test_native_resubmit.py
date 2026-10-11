"""Explicit resubmission creates a new logical message, never an implicit retry."""
import json
import uuid
import pytest
from test_native_daemon import Server,spec,submit,await_watch
from test_native_project_view import finish,data_hashes
from test_native_cli_security import cli

def messages(server):return server.rpc('records.latest',{'stream':'messages','identity':'message_id'},version=1)['result']

def test_completed_task_resubmission_has_new_ids_and_origin(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec('worker')})
    try:
        original=submit(server,body='saved task');done=await_watch(server,original['job_id'],lambda r:r['terminal'])
        result=cli('--state-root',server.root,'resubmit',done['message_id'])
        assert result.returncode==0,result.stderr
        receipt=json.loads(result.stdout)
        assert receipt['original_message_id']==done['message_id'] and receipt['message_id']!=done['message_id']
        assert receipt['jobs'][0]['job_id']!=original['job_id']
        new=next(m for m in messages(server) if m['message_id']==receipt['message_id'])
        assert new['origin_message_id']==done['message_id']
        traced=server.rpc('trace',{'target':receipt['message_id']})
        assert traced['attempts'][0]['retry_index']==0
        assert await_watch(server,receipt['jobs'][0]['job_id'],lambda r:r['terminal'])['reply']=='worker result'
    finally:server.close()

def test_resubmit_operation_replay_survives_restart_and_rejects_retarget(tmp_path):
    root=tmp_path/'state';agents={'worker':spec()};server=Server(root,agents)
    try:
        old=submit(server)['job_id'];finish(server,server.rpc('work',{'agent_name':'worker'})['job'])
        mid=server.rpc('watch',{'target':old})['message_id'];op=str(uuid.uuid4())
        receipt=server.rpc('resubmit',{'message_id':mid,'operation_id':op})
        again=server.rpc('resubmit',{'message_id':mid,'operation_id':op});assert receipt==again
    finally:server.close()

    server=Server(root,agents)
    try:
        assert server.rpc('resubmit',{'message_id':mid,'operation_id':op})==receipt
        with pytest.raises(AssertionError):server.rpc('resubmit',{'message_id':'msg_other','operation_id':op})
        assert len(messages(server))==2
    finally:server.close()

def test_lost_resubmit_acknowledgement_does_not_duplicate_admission(tmp_path):
    import socket
    from ccbd.control_plane_transport.token_auth import client_authenticate
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        root=submit(server)['job_id'];finish(server,server.rpc('work',{'agent_name':'worker'})['job'])
        mid=server.rpc('watch',{'target':root})['message_id'];request={'message_id':mid,'operation_id':str(uuid.uuid4())}
        with socket.create_connection((server.endpoint['host'],server.endpoint['port']),timeout=3) as stream:
            client_authenticate(stream,server.token.token)
            stream.sendall((json.dumps({'api_version':2,'op':'resubmit','request':request})+'\n').encode())
        receipt=server.rpc('resubmit',request)
        assert server.rpc('resubmit',request)==receipt and len(messages(server))==2
    finally:server.close()

def test_active_original_message_is_not_resubmitted(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        root=submit(server)['job_id'];mid=server.rpc('watch',{'target':root})['message_id'];before=data_hashes(server)
        with pytest.raises(AssertionError,match='active attempts'):server.rpc('resubmit',{'message_id':mid})
        assert data_hashes(server)==before
    finally:server.close()

def test_completed_callback_parent_restarts_original_request_not_review_prompt(tmp_path):
    server=Server(tmp_path/'state',{'lead':spec('lead'),'worker':spec('worker')})
    try:
        root=submit(server,'lead',body='original parent request')['job_id']
        done=await_watch(server,root,lambda r:r['visible_reply_source']=='message_bureau_reply')
        result=server.rpc('resubmit',{'message_id':done['message_id']})
        job_id=result['jobs'][0]['job_id']
        jobs=server.rpc('records.latest',{'stream':'jobs','agent_name':'lead','identity':'job_id'},version=1)['result']
        new=next(j for j in jobs if j['job_id']==job_id)
        assert new['request']['message_type']=='ask' and new['request']['body']=='original parent request'
        assert await_watch(server,job_id,lambda r:r['visible_reply_source']=='message_bureau_reply')['reply']=='reviewed: worker result'
    finally:server.close()

def test_broadcast_resubmit_freezes_original_recipients_after_agent_added(tmp_path):
    from test_native_reload import applied,config
    agents={'a':spec(),'b':spec()};server=Server(tmp_path/'state',agents)
    try:
        first=server.rpc('submit',{'project_id':'native-test','to_agent':'all','from_actor':'user','body':'original broadcast','delivery_scope':'broadcast'})
        for name in agents:finish(server,server.rpc('work',{'agent_name':name})['job'])
        mid=server.rpc('watch',{'target':first['jobs'][0]['job_id']})['message_id']
        applied(server,config({**agents,'c':spec()}))
        receipt=server.rpc('resubmit',{'message_id':mid})
        assert {j['agent_name'] for j in receipt['jobs']}=={'a','b'}
        assert receipt['submission_id']!=first['submission_id']
        assert server.rpc('queue',{'target':'c'})['queues'][0]['queued_job_ids']==[]
    finally:server.close()

def test_missing_broadcast_target_refuses_entire_resubmission(tmp_path):
    from test_native_reload import applied,config
    server=Server(tmp_path/'state',{'a':spec(),'b':spec()})
    try:
        first=server.rpc('submit',{'project_id':'native-test','to_agent':'all','from_actor':'user','body':'original broadcast','delivery_scope':'broadcast'})
        for name in ['a','b']:finish(server,server.rpc('work',{'agent_name':name})['job'])
        mid=server.rpc('watch',{'target':first['jobs'][0]['job_id']})['message_id']
        applied(server,config({'a':spec()}));before=data_hashes(server)
        with pytest.raises(AssertionError):server.rpc('resubmit',{'message_id':mid})
        assert data_hashes(server)==before
    finally:server.close()

def test_resubmit_revalidates_saved_request_artifact(tmp_path):
    from pathlib import Path
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        root=server.rpc('submit',{'project_id':'native-test','to_agent':'worker','from_actor':'user','body':'saved artifact request','body_transport':'artifact'})['job_id']
        job=server.rpc('work',{'agent_name':'worker'})['job'];finish(server,job)
        mid=server.rpc('watch',{'target':root})['message_id'];ref=job['request']['body_artifact'];path=Path(ref['path'])
        assert path.resolve().is_relative_to(tmp_path.resolve())
        content=path.read_bytes();before=data_hashes(server);path.write_bytes(b'changed artifact')
        try:
            with pytest.raises(AssertionError):server.rpc('resubmit',{'message_id':mid})
            assert data_hashes(server)==before
        finally:path.write_bytes(content)
        receipt=server.rpc('resubmit',{'message_id':mid});new=server.rpc('records.latest',{'stream':'jobs','agent_name':'worker','identity':'job_id'},version=1)['result'][-1]
        assert new['job_id']==receipt['jobs'][0]['job_id'] and new['request']['body_artifact']==ref
    finally:server.close()

def test_unresolved_chain_parent_and_completed_chain_child_cannot_be_retargeted(tmp_path):
    server=Server(tmp_path/'state',{'lead':spec('lead'),'worker':spec('slow-input')})
    try:
        root=submit(server,'lead')['job_id'];pending=await_watch(server,root,lambda r:r['status']=='waiting_callback')
        with pytest.raises(AssertionError,match='callback'):server.rpc('resubmit',{'message_id':pending['message_id']})
        await_watch(server,root,lambda r:r['visible_reply_source']=='message_bureau_reply')
        edge=server.rpc('records.latest',{'stream':'callbacks','identity':'edge_id'},version=1)['result'][0]
        before=data_hashes(server)
        with pytest.raises(AssertionError,match='chain child'):server.rpc('resubmit',{'message_id':edge['child_message_id']})
        assert data_hashes(server)==before
    finally:server.close()

def test_context_control_cannot_be_replayed_as_resubmitted_task(tmp_path):
    from test_native_context import provider,control,settled,job
    from test_native_restart import ready
    server=Server(tmp_path/'state',{'worker':provider(tmp_path,kind='codex')})
    try:
        ready(server);op=settled(server,control(server));assert op['status']=='completed'
        mid=server.rpc('watch',{'target':job(op)})['message_id'];before=data_hashes(server)
        with pytest.raises(AssertionError,match='context controls'):server.rpc('resubmit',{'message_id':mid})
        assert data_hashes(server)==before
    finally:server.close()
