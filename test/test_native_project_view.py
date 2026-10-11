"""Native UI observations do not acknowledge, finish or recover queued work."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

import pytest
from test_native_daemon import Server,spec,submit,await_watch
from test_native_pi_rpc import pi_spec
from test_native_cli_security import cli

ROOT=Path(__file__).resolve().parents[1]
SIDEBAR=ROOT/'tools/ccb-agent-sidebar/target/release'/('ccb-agent-sidebar.exe' if os.name=='nt' else 'ccb-agent-sidebar')
NATIVE=Path(os.environ.get('CCB_TEST_NATIVE_BIN') or ROOT/'rust/target/release/ccb-native.exe')


def view(server,**kwargs):return server.rpc('project_view',dict(schema_version=1,**kwargs))
def agents(response):return {row['name']:row for row in response['view']['agents']}
def rows(response):return {row['id']:row for row in response['view']['comms']}
def data_hashes(server):
    return {str(p):hashlib.sha256(p.read_bytes()).hexdigest() for root in [server.root,server.root.parent/'agents']
            for p in root.rglob('*.jsonl')}


def finish(server,job):
    decision=dict(schema_version=2,record_type='completion_decision',terminal=True,status='completed',reason='task_complete',
                  confidence='observed',reply='worker result',anchor_seen=True,reply_started=True,reply_stable=True,
                  provider_turn_ref=None,source_cursor=None,finished_at='2026-10-11T12:00:00Z',diagnostics={})
    server.rpc('complete',dict(job_id=job['job_id'],agent_name=job['agent_name'],
                              execution_instance_id=job['provider_options']['native_execution_instance'],decision=decision))


def test_idle_and_pending_views_are_stable_read_only_and_do_not_guess_physical_panes(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        empty=view(server)
        assert agents(empty)['worker']['activity_state']=='offline'
        assert agents(empty)['worker']['pane_id'] is None
        one=submit(server)['job_id'];two=submit(server,body='second')['job_id']
        before=data_hashes(server);first=view(server);second=view(server)
        assert before==data_hashes(server)
        assert first['cache']['sequence']==second['cache']['sequence']>empty['cache']['sequence']
        assert agents(first)['worker']['queue_depth']==2 and agents(first)['worker']['activity_state']=='pending'
        assert {one,two}<=set(rows(first))
        assert rows(first)[one]['cancel_enabled'] and not rows(first)[one]['recoverable']
        assert not (server.root/'project-view.json').exists()
    finally:server.close()


def test_view_never_hides_or_cancels_live_work_and_terminal_dismissal_survives_restart(tmp_path):
    root=tmp_path/'state';server=Server(root,{'worker':spec()})
    try:
        job=submit(server)['job_id']
        with pytest.raises(AssertionError,match='terminal'):server.rpc('project_view_dismiss_comms',{'id':job})
        active=server.rpc('work',{'agent_name':'worker'})['job']
        snapshot=view(server);assert agents(snapshot)['worker']['activity_state']=='active'
        assert rows(snapshot)[job]['cancel_enabled'] is False
        before=data_hashes(server);finish(server,active)
        assert data_hashes(server)!=before
        completed=view(server);assert rows(completed)[job]['business_status']=='replied'
        before=data_hashes(server)
        assert server.rpc('project_view_dismiss_comms',{'id':job})['dismissed_count']==1
        assert server.rpc('project_view_dismiss_comms',{'id':job})['dismissed_count']==1
        assert before==data_hashes(server) and job not in rows(view(server))
    finally:server.close()
    restored=Server(root,{'worker':spec()})
    try:
        assert job not in rows(view(restored))
        assert restored.rpc('watch',{'target':job})['status']=='completed'
    finally:restored.close()


def test_callback_parent_waits_until_its_continuation_really_finishes(tmp_path):
    server=Server(tmp_path/'state',{'lead':spec('lead'),'worker':spec()})
    try:
        parent=submit(server,'lead')['job_id']
        await_watch(server,parent,lambda r:r['status']=='waiting_callback')
        before=data_hashes(server);waiting=view(server)
        assert agents(waiting)['lead']['activity_reason']=='waiting_callback'
        assert rows(waiting)[parent]['status']=='waiting_callback'
        assert rows(waiting)[parent]['status_label']!='done' and before==data_hashes(server)
        with pytest.raises(AssertionError,match='terminal'):server.rpc('project_view_dismiss_comms',{'id':parent})
        finish(server,server.rpc('work',{'agent_name':'worker'})['job'])
        await_watch(server,parent,lambda r:r['visible_reply_source']=='message_bureau_reply')
        done=view(server)
        assert rows(done)[parent]['status']=='completed' and rows(done)[parent]['status_label']=='done'
        assert done['cache']['sequence']>waiting['cache']['sequence']
    finally:server.close()


def test_worker_completion_is_not_mistaken_for_completed_reply_delivery(tmp_path):
    server=Server(tmp_path/'state',{'lead':spec(),'worker':spec('worker')})
    try:
        job=submit(server,actor='lead')['job_id'];await_watch(server,job,lambda r:r['terminal'])
        waiting=view(server)
        assert rows(waiting)[job]['business_status']=='delivering' and rows(waiting)[job]['status_label']=='back'
        assert agents(waiting)['lead']['activity_state']=='pending'
        before=data_hashes(server);view(server);assert before==data_hashes(server)
        with pytest.raises(AssertionError,match='delivery'):server.rpc('project_view_dismiss_comms',{'id':job})
        delivery=server.rpc('work',{'agent_name':'lead'})['job'];assert delivery['request']['message_type']=='reply_delivery'
        finish(server,delivery)
        assert rows(view(server))[job]['business_status']=='replied'
        assert server.rpc('project_view_dismiss_comms',{'id':job})['status']=='dismissed'
    finally:server.close()


def test_callback_result_can_be_delivered_to_an_idle_agent_using_the_continuation_attempt(tmp_path):
    server=Server(tmp_path/'state',{'caller':spec(),'lead':spec('lead'),'worker':spec('worker')})
    try:
        parent=submit(server,'lead','caller')['job_id']
        await_watch(server,parent,lambda r:r['visible_reply_source']=='message_bureau_reply')
        assert rows(view(server))[parent]['business_status']=='delivering'
        delivery=server.rpc('work',{'agent_name':'caller'})['job']
        assert delivery['request']['message_type']=='reply_delivery'
        finish(server,delivery)
        assert rows(view(server))[parent]['business_status']=='replied'
        assert server.rpc('project_view_dismiss_comms',{'id':parent})['status']=='dismissed'
    finally:server.close()


def test_receiver_fault_keeps_a_completed_worker_result_visible_as_blocked(tmp_path):
    from test_native_role_delivery import SETUPS,held
    _,_,_,behavior,lead=SETUPS['claude'][0](tmp_path)
    behavior.write_text('missing')
    server=Server(tmp_path/'state',{'lead':lead,'worker':spec('worker')})
    try:
        job=submit(server,actor='lead')['job_id'];await_watch(server,job,lambda r:r['terminal']);held(server,'claude')
        before=data_hashes(server);snapshot=view(server)
        assert rows(snapshot)[job]['business_status']=='blocked'
        assert rows(snapshot)[job]['block_reason']=='receiver_provider_fault'
        assert agents(snapshot)['lead']['activity_state']=='failed' and before==data_hashes(server)
        with pytest.raises(AssertionError,match='delivery'):server.rpc('project_view_dismiss_comms',{'id':job})
    finally:server.close()


def test_provider_fault_is_not_displayed_as_idle(tmp_path):
    from test_native_provider_faults import fault
    bad=pi_spec('worker');bad['command']=[str(tmp_path/'missing.exe')]
    server=Server(tmp_path/'state',{'bad':bad,'healthy':spec()})
    try:
        observed=fault(server,'bad')
        row=agents(view(server))['bad']
        assert row['activity_state']=='failed' and row['fault_id']==observed['fault_id']
    finally:server.close()


def test_pending_provider_approval_is_visible_without_answering_it(tmp_path):
    from test_native_codex_requests import spec as interactive,await_request,wire
    server=Server(tmp_path/'state',{'worker':interactive(tmp_path,'command')})
    try:
        job=submit(server)['job_id'];request=await_request(server)
        snapshot=view(server);row=agents(snapshot)['worker']
        assert row['activity_state']=='pending' and row['activity_reason']=='provider_user_input'
        assert row['pending_requests']==1 and row['current_job_id']==job
        assert request['request_id'] not in json.dumps(snapshot)
        assert not [entry for entry in wire(tmp_path) if 'method' not in entry]
        assert server.rpc('watch',{'target':job})['status']=='running'
    finally:server.close()


def test_retries_are_projected_as_the_current_business_attempt(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec('retry-once')})
    try:
        submit(server)
        deadline=time.monotonic()+8
        while time.monotonic()<deadline:
            jobs=server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result']
            if len(jobs)==2 and jobs[-1]['status']=='completed':break
            time.sleep(.025)
        assert len(jobs)==2 and jobs[-1]['status']=='completed'
        snapshot=view(server)
        assert list(rows(snapshot))==[jobs[-1]['job_id']]
        assert rows(snapshot)[jobs[-1]['job_id']]['status']=='completed'
    finally:server.close()


def test_previews_are_bounded_and_terminal_controls_are_removed(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        job=submit(server,body='a\x1b]52;clipboard\x07\u202e\n'+('中文 '*80))['job_id']
        row=rows(view(server))[job];preview=row['body_preview']
        assert len(preview)<=48 and not any(value in preview for value in ['\x1b','\x07','\u202e','\n'])
        assert preview.endswith('...')
    finally:server.close()


def test_repeated_projection_uses_committed_indices_without_rereading_history(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        for n in range(25):submit(server,body=f'task {n}')
        view(server)
        before=server.rpc('records.metrics',{'stream':'jobs','agent_name':'worker'},version=1)['result']
        for _ in range(5):assert len(view(server)['view']['comms'])==8
        after=server.rpc('records.metrics',{'stream':'jobs','agent_name':'worker'},version=1)['result']
        assert after==before
    finally:server.close()


@pytest.mark.parametrize('payload',[{'schema_version':2},{'comms_limit':0},{'comms_limit':65},{'comms_limit':'8'}])
def test_invalid_view_requests_do_not_change_work(tmp_path,payload):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        job=submit(server)['job_id'];before=data_hashes(server)
        with pytest.raises(AssertionError):server.rpc('project_view',payload)
        assert data_hashes(server)==before and server.rpc('watch',{'target':job})['status']=='accepted'
    finally:server.close()


def test_foreign_agent_journal_is_rejected_before_starting_a_new_project(tmp_path):
    root=tmp_path/'state';server=Server(root,{'worker':spec()})
    submit(server)
    server.close()
    record_file=root.parent/'agents/worker/jobs.jsonl'
    records=[json.loads(line) for line in record_file.read_text().splitlines()]
    for record in records:record['request']['project_id']='foreign-project'
    record_file.write_text('\n'.join(json.dumps(record) for record in records)+'\n')
    before=record_file.read_bytes()
    with pytest.raises(AssertionError,match='another project'):
        Server(root,{'worker':spec()})
    assert record_file.read_bytes()==before


def test_foreign_record_injected_after_start_is_not_exposed_by_the_view(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        submit(server)
        records=server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result']
        record=records[-1];record['job_id']='foreign-row';record['request']['project_id']='foreign'
        record['request']['body']='DO_NOT_DISCLOSE_FOREIGN_TASK';record['status']='completed'
        record['terminal_decision']={'reply':'private'}
        server.rpc('records.append',dict(stream='jobs',agent_name='worker',record=record),version=1)
        before=data_hashes(server)
        with pytest.raises(AssertionError,match='another project') as rejected:view(server)
        assert 'DO_NOT_DISCLOSE' not in str(rejected.value) and before==data_hashes(server)
    finally:server.close()


def test_cli_and_sidebar_dump_read_the_native_authenticated_view_without_a_terminal(tmp_path):
    assert SIDEBAR.is_file(),'build the Rust sidebar for native UI contracts'
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        job=submit(server)['job_id']
        native=cli('--state-root',server.root,'view','--limit','3')
        assert native.returncode==0,native.stderr
        assert rows(json.loads(native.stdout))[job]['status']=='accepted'
        env=os.environ.copy();env['CCB_PYTHON']=str(tmp_path/'absent-python.exe')
        before=data_hashes(server)
        result=subprocess.run([str(SIDEBAR),'--state-root',str(server.root),'--native-bin',str(NATIVE),'--dump-json'],
                              env=env,capture_output=True,text=True,encoding='utf-8',timeout=8)
        assert result.returncode==0,result.stderr
        snapshot=json.loads(result.stdout)
        assert rows(snapshot)[job]['id']==job and snapshot['view']['namespace']['runtime']=='rust'
        assert data_hashes(server)==before
        assert not (tmp_path/'project-view.json').exists()
    finally:server.close()
