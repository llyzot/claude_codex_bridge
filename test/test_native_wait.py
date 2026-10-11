"""Reply-count observations never submit, acknowledge or cancel work."""
import json
import pytest
from test_native_daemon import Server,spec,submit,await_watch
from test_native_cli_security import cli
from test_native_project_view import finish,data_hashes
from cli.services.wait_runtime.replies import latest_replies

def summary(server,target,mode='all',**kwargs):return server.rpc('wait-summary',dict(target=target,mode=mode,**kwargs))

def test_broadcast_any_all_quorum_and_read_only_timeout(tmp_path):
    server=Server(tmp_path/'state',{'a':spec(),'b':spec()})
    try:
        receipt=server.rpc('submit',dict(project_id='native-test',to_agent='all',from_actor='user',body='fan out',delivery_scope='broadcast'))
        target=receipt['submission_id'];before=data_hashes(server)
        assert summary(server,target)['wait_status']=='pending'
        expired=cli('--state-root',server.root,'wait-all',target,'--timeout-ms','0')
        assert expired.returncode!=0 and json.loads(expired.stdout)['observation_timed_out']
        assert data_hashes(server)==before
        finish(server,server.rpc('work',{'agent_name':'a'})['job'])
        assert summary(server,target,'any')['wait_status']=='satisfied'
        assert summary(server,target)['received_count']==1
        assert summary(server,target,'quorum',quorum=2)['wait_status']=='pending'
        finish(server,server.rpc('work',{'agent_name':'b'})['job'])
        result=cli('--state-root',server.root,'wait-quorum','2',target,'--timeout','0.5')
        assert result.returncode==0,result.stderr
        assert json.loads(result.stdout)['received_count']==2
    finally:server.close()

def test_waits_for_logical_callback_result_not_parent_turn_end(tmp_path):
    server=Server(tmp_path/'state',{'lead':spec('lead'),'worker':spec('slow-input')})
    try:
        root=submit(server,'lead')['job_id'];await_watch(server,root,lambda r:r['status']=='waiting_callback')
        assert summary(server,root)['wait_status']=='pending'
        result=cli('--state-root',server.root,'wait-all',root,'--timeout','5')
        assert result.returncode==0,result.stderr
        assert json.loads(result.stdout)['replies'][0]['reply']=='reviewed: worker result'
    finally:server.close()

def test_snapshot_matches_legacy_latest_reply_selection_after_retry(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec('retry-once')})
    try:
        receipt=submit(server);row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        result=cli('--state-root',server.root,'wait-all',row['message_id'],'--timeout','5');assert result.returncode==0,result.stderr
        traced=server.rpc('trace',{'target':row['message_id']});expected,replies,terminal,notice=latest_replies(traced)
        observed=summary(server,row['message_id'])
        assert (observed['expected_count'],observed['terminal_count'],observed['notice_count'])==(expected,terminal,notice)
        assert [{k:r[k] for k in replies[0]} for r in observed['replies']]==list(replies)
    finally:server.close()

def test_heartbeat_notice_returns_notice_without_finishing_job(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        root=submit(server)['job_id'];job=server.rpc('work',{'agent_name':'worker'})['job']
        attempt=server.rpc('records.latest',{'stream':'attempts','identity':'attempt_id'},version=1)['result'][0]
        row=dict(schema_version=1,record_type='reply_record',reply_id='rep_notice_fixture',message_id=attempt['message_id'],
                 attempt_id=attempt['attempt_id'],agent_name='worker',terminal_status='incomplete',reply='Still working',reply_artifact=None,
                 diagnostics={'notice':True,'notice_kind':' Heartbeat ','reason':'waiting','last_progress_at':' 2026-10-12T00:00:00Z ','heartbeat_silence_seconds':'1.5'},finished_at='2026-10-12T00:00:00Z')
        server.rpc('records.append',{'stream':'replies','record':row},version=1)
        before=data_hashes(server);result=summary(server,root,'any')
        assert result['wait_status']=='notice' and result['notice_count']==1 and result['terminal_count']==0
        assert result['replies'][0]['notice_kind']=='heartbeat' and result['replies'][0]['heartbeat_silence_seconds']==1.5
        assert server.rpc('watch',{'target':root})['status']=='running'
        assert data_hashes(server)==before
    finally:server.close()

def test_cancelled_reply_satisfies_observation_without_claiming_success(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        root=submit(server)['job_id'];job=server.rpc('work',{'agent_name':'worker'})['job']
        decision=dict(schema_version=2,record_type='completion_decision',terminal=True,status='cancelled',reason='cancel_info',
                      confidence='observed',reply='Cancelled',anchor_seen=True,reply_started=True,reply_stable=True,
                      provider_turn_ref=None,source_cursor=None,finished_at='2026-10-12T00:00:00Z',diagnostics={})
        server.rpc('complete',dict(job_id=root,agent_name='worker',execution_instance_id=job['provider_options']['native_execution_instance'],decision=decision))
        result=summary(server,root,'all')
        assert result['wait_status']=='satisfied' and result['terminal_count']==1
        assert result['replies'][0]['terminal_status']=='cancelled'
    finally:server.close()

@pytest.mark.parametrize('option,value',[('--timeout','NaN'),('--timeout-ms','86400001')])
def test_invalid_client_deadlines_do_not_mutate_work(tmp_path,option,value):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        root=submit(server)['job_id'];before=data_hashes(server)
        result=cli('--state-root',server.root,'wait-any',root,option,value)
        assert result.returncode!=0 and data_hashes(server)==before
    finally:server.close()

@pytest.mark.parametrize('payload',[{'mode':'bad'},{'mode':'quorum','quorum':0},{'mode':'quorum','quorum':2},{'mode':'any','quorum':1}])
def test_bad_quorum_or_mode_does_not_change_work(tmp_path,payload):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        root=submit(server)['job_id'];before=data_hashes(server)
        with pytest.raises(AssertionError):server.rpc('wait-summary',{'target':root,**payload})
        assert data_hashes(server)==before
    finally:server.close()
