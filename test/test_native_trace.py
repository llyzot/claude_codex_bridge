"""Public trace resolves durable IDs and callback/retry lineage without dispatch."""
import json
from pathlib import Path
import pytest
from test_native_daemon import Server,spec,submit,await_watch
from test_native_cli_security import cli

def latest(server,stream,key):
    return server.rpc('records.latest',{'stream':stream,'identity':key},version=1)['result']

def test_all_direct_ids_resolve_same_task_and_match_legacy_summaries(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec('worker')})
    try:
        receipt=submit(server);done=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        attempt=latest(server,'attempts','attempt_id')[0];reply=latest(server,'replies','reply_id')[0]
        for target,kind in [(receipt['job_id'],'job'),(done['message_id'],'message'),(attempt['attempt_id'],'attempt'),(reply['reply_id'],'reply')]:
            traced=server.rpc('trace',{'target':target})
            assert traced['resolved_kind']==kind and traced['message_id']==done['message_id']
            assert traced['job_count']==1 and traced['reply_count']==1
            assert traced['jobs'][0]['job_id']==receipt['job_id'] and traced['replies'][0]['reply']=='worker result'
        from message_bureau.models import MessageRecord,AttemptRecord,ReplyRecord
        from message_bureau.control_trace_runtime.summaries import message_summary,attempt_summary,reply_summary
        traced=server.rpc('trace',{'target':done['message_id']})
        message=latest(server,'messages','message_id')[0]
        for actual,expected in [(traced['messages'][0],message_summary(MessageRecord.from_record(message))),
                                (traced['attempts'][0],attempt_summary(AttemptRecord.from_record(attempt))),
                                (traced['replies'][0],reply_summary(ReplyRecord.from_record(reply)))]:
            assert {k:actual[k] for k in expected}==expected
        result=cli('--state-root',server.root,'trace',receipt['job_id'])
        assert result.returncode==0,result.stderr
        assert json.loads(result.stdout)['job']['status']=='completed'
    finally:server.close()

def test_trace_exposes_reply_artifact_metadata_without_reading_artifact_body(tmp_path):
    from test_native_project_view import finish
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        receipt=server.rpc('submit',{'project_id':'native-test','to_agent':'worker','from_actor':'user','body':'PRIVATE_REQUEST','route_options':{'artifact_reply':True}})
        job=server.rpc('work',{'agent_name':'worker'})['job'];finish(server,job)
        traced=server.rpc('trace',{'target':receipt['job_id']});reply=traced['replies'][0]
        assert reply['reply_artifact_path'] and reply['reply_artifact_bytes']>0 and reply['reply_artifact_sha256']
        assert 'PRIVATE_REQUEST' not in json.dumps(traced)
        path=Path(reply['reply_artifact_path']);assert path.resolve().is_relative_to(tmp_path.resolve())
        original=path.read_bytes();path.write_bytes(b'REPLACED_ARTIFACT_MUST_NOT_BE_READ')
        try:
            again=server.rpc('trace',{'target':receipt['job_id']})
            assert again['replies']==traced['replies'] and 'REPLACED_ARTIFACT' not in json.dumps(again)
        finally:path.write_bytes(original)
    finally:server.close()

def test_trace_broadcast_submission_matches_job_order_and_is_read_only(tmp_path):
    server=Server(tmp_path/'state',{'a':spec('worker'),'b':spec('worker')})
    try:
        receipt=server.rpc('submit',{'project_id':'native-test','to_agent':'all','from_actor':'user','body':'broadcast','delivery_scope':'broadcast'})
        for job in receipt['jobs']:await_watch(server,job['job_id'],lambda r:r['terminal'])
        submissions=latest(server,'submissions','submission_id');assert len(submissions)==1
        before={p:p.read_bytes() for p in (server.root.parent/'agents').rglob('*.jsonl')}
        traced=server.rpc('trace',{'target':submissions[0]['submission_id']})
        assert traced['resolved_kind']=='submission' and traced['job_count']==2 and traced['attempt_count']==2
        assert [j['job_id'] for j in traced['jobs']]==submissions[0]['job_ids']
        assert all(p.read_bytes()==data for p,data in before.items())
    finally:server.close()

def test_trace_callback_graph_excludes_unrelated_jobs(tmp_path):
    server=Server(tmp_path/'state',{'lead':spec('lead'),'worker':spec('worker'),'other':spec('worker')})
    try:
        unrelated=submit(server,'other')['job_id'];await_watch(server,unrelated,lambda r:r['terminal'])
        root=submit(server,'lead')['job_id'];await_watch(server,root,lambda r:r['visible_reply_source']=='message_bureau_reply')
        traced=server.rpc('trace',{'target':root})
        assert traced['job']['business_status']=='completed'
        assert len(traced['callbacks'])==1 and traced['callbacks'][0]['state']=='done'
        assert len(traced['callback_jobs'])==3
        assert unrelated not in {j['job_id'] for j in traced['callback_jobs']}
    finally:server.close()

def test_parent_trace_distinguishes_finished_turn_from_pending_callback(tmp_path):
    server=Server(tmp_path/'state',{'lead':spec('lead'),'worker':spec('slow-input')})
    try:
        root=submit(server,'lead')['job_id'];await_watch(server,root,lambda r:r['status']=='waiting_callback')
        traced=server.rpc('trace',{'target':root})
        assert traced['job']['status']=='completed' and traced['job']['business_status']=='waiting_callback'
        done=await_watch(server,root,lambda r:r['visible_reply_source']=='message_bureau_reply')
        traced=server.rpc('trace',{'target':root})
        assert traced['job']['visible_reply_id']==done['visible_reply_id']
    finally:server.close()

def test_trace_refuses_cross_project_job_record(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        root=submit(server)['job_id']
        row=server.rpc('records.latest',{'stream':'jobs','identity':'job_id','agent_name':'worker'},version=1)['result'][0]
        row['request']['project_id']='foreign-project'
        server.rpc('records.append',{'stream':'jobs','agent_name':'worker','record':row},version=1)
        with pytest.raises(AssertionError,match='another project'):server.rpc('trace',{'target':root})
    finally:server.close()

def test_trace_internal_reply_delivery_job_links_original_result(tmp_path):
    import time
    server=Server(tmp_path/'state',{'lead':spec('lead'),'worker':spec('worker')})
    try:
        root=submit(server,'worker','lead')['job_id'];await_watch(server,root,lambda r:r['terminal'])
        until=time.monotonic()+8;deliveries=[]
        while time.monotonic()<until:
            jobs=server.rpc('records.latest',{'stream':'jobs','identity':'job_id','agent_name':'lead'},version=1)['result']
            deliveries=[j for j in jobs if j['provider_options'].get('reply_delivery')]
            if deliveries and deliveries[-1]['status']=='completed':break
            time.sleep(.03)
        assert deliveries
        traced=server.rpc('trace',{'target':deliveries[-1]['job_id']})
        assert traced['job_role']=='reply_delivery' and traced['attempt_id'] is not None
        assert traced['job_count']==1 and traced['source_reply']['reply']=='worker result'
        assert traced['delivery_event']['inbound_event_id']==deliveries[-1]['provider_options']['reply_delivery_inbound_event_id']
    finally:server.close()

def test_trace_message_keeps_retry_attempts_and_pages_explicitly(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec('retry-once')})
    try:
        receipt=submit(server);done=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        # Automatic retry has a distinct job; await its final reply.
        import time
        until=time.monotonic()+8
        while time.monotonic()<until:
            replies=latest(server,'replies','reply_id')
            if replies:break
            time.sleep(.03)
        target=done['message_id'];traced=server.rpc('trace',{'target':target})
        assert traced['attempt_count']==2 and traced['job_count']==2
        assert [a['retry_index'] for a in traced['attempts']]==[0,1]
        first=server.rpc('trace',{'target':target,'limit':1})
        second=server.rpc('trace',{'target':target,'limit':1,'offset':1})
        assert first['truncated'] and first['next_offset']==1
        assert first['attempts'][0]['attempt_id']!=second['attempts'][0]['attempt_id']
        assert first['attempt_count']==second['attempt_count']==2
    finally:server.close()

@pytest.mark.parametrize('target',['worker','../../token','job_missing','rep_missing'])
def test_trace_invalid_or_missing_target_does_not_submit_work(tmp_path,target):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        with pytest.raises(AssertionError):server.rpc('trace',{'target':target})
        assert server.rpc('queue',{'target':'worker'})['queues'][0]['queued_job_ids']==[]
    finally:server.close()
