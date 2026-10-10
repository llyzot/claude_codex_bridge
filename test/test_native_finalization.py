"""Terminal transactions and callback repair against the actual native process."""
from dataclasses import replace
import os
from threading import RLock
from pathlib import Path
from types import SimpleNamespace

import pytest

from test_native_admission import dispatcher, request, plan, activate, history, canonical, NOW, NAMES, BIN
from ccbd.services.dispatcher_runtime.submission_recording import _submit_plan
from ccbd.services.dispatcher_runtime.finalization_runtime.persistence import persist_terminal_completion
from ccbd.services.dispatcher_runtime.callbacks import repair_callback_edges
from ccbd.services.snapshot_writer import SnapshotWriter
from completion.models import CompletionDecision, CompletionStatus, CompletionConfidence, CompletionFamily
from completion.models import CompletionCursor, CompletionItem, CompletionItemKind, CompletionSourceKind
from completion.snapshot_store import CompletionSnapshotStore
from message_bureau import AttemptStore, ReplyStore, CallbackEdgeStore, CallbackEdgeState
from native_kernel import close_kernels
from ccbd.api_models import JobStatus

BIN=Path(os.environ.get('CCB_TEST_KERNEL_BIN') or BIN)

@pytest.fixture(autouse=True)
def native_environment(monkeypatch):
    assert BIN.is_file()
    monkeypatch.setenv('CCB_KERNEL_BIN',str(BIN))
    close_kernels()
    yield
    close_kernels()

def setup(root,mode,monkeypatch):
    d=dispatcher(root,mode,monkeypatch)
    d._snapshot_writer=SnapshotWriter(d._layout,clock=lambda:NOW)
    d._completion_tracker=None
    d._chain_transition_lock=RLock()
    d._profile_family_for_job=lambda job:CompletionFamily.PROTOCOL_TURN
    d._terminal_event_by_status={JobStatus.COMPLETED:'job_completed',JobStatus.CANCELLED:'job_cancelled',
                                 JobStatus.FAILED:'job_failed',JobStatus.INCOMPLETE:'job_incomplete'}
    return d

def decision(reply='done',status=CompletionStatus.COMPLETED,reason='task_complete',diagnostics=None):
    return CompletionDecision(True,status,reason,CompletionConfidence.OBSERVED,reply,True,True,True,None,None,NOW,diagnostics or {})

def wire(d,current,result):
    return dict(current=current.to_record(),decision=result.to_record(),state_id=d._state._id,
                bureau_enabled=True,tracker_group=None,tracker_view=None,profile_family='protocol_turn',
                preview_limit=240,finished_at=NOW,prior_snapshot=None)

def normalized(value,root):
    def walk(v):
        if isinstance(v,dict): return {k:walk(x) for k,x in v.items()}
        if isinstance(v,list): return [walk(x) for x in v]
        if isinstance(v,str): return v.replace(str(root.resolve()),'<ccbd>')
        return v
    value=walk(value)
    # Artifact IDs appear in both metadata and references embedded in replies.
    import re
    ids={}
    def artifact_walk(v):
        if isinstance(v,dict): return {k:artifact_walk(x) for k,x in v.items()}
        if isinstance(v,list): return [artifact_walk(x) for x in v]
        if isinstance(v,str):
            def replace_id(m):
                return ids.setdefault(m.group(),f'art-id-{len(ids)}')
            return re.sub(r'\bart_[0-9a-f]{16}\b',replace_id,v)
        return v
    return canonical(artifact_walk(value))

@pytest.mark.parametrize('reply,force,status,diagnostics',[
    ('done',False,CompletionStatus.COMPLETED,{}),
    ('汉'*1800,False,CompletionStatus.COMPLETED,{}),
    ('short',True,CompletionStatus.COMPLETED,{}),
    ('',True,CompletionStatus.CANCELLED,{}),
    ('',False,CompletionStatus.INCOMPLETE,{'suppress_completion_state_merge':True}),
], ids=['small','unicode_spill','forced_short','forced_empty_cancel','suppressed_merge'])
def test_terminal_history_snapshot_and_artifact_parity(tmp_path,monkeypatch,reply,force,status,diagnostics):
    bundles=[]
    for mode in ['python','rust']:
        # Equal directory-name lengths keep path-containing preview truncation
        # comparable before the two absolute roots are normalized.
        d=setup(tmp_path/('python' if mode=='python' else 'rust__'),mode,monkeypatch)
        parent=activate(d)
        if force:
            parent.request.route_options['artifact_reply']=True
            d._job_store.append(parent)
        terminal,result,prior=persist_terminal_completion(d,parent,decision(reply,status,diagnostics=diagnostics),finished_at=NOW)
        if mode=='python':
            d._message_bureau.record_attempt_terminal(terminal,result,finished_at=NOW)
        snapshot=CompletionSnapshotStore(d._layout).load(parent.job_id)
        artifact=result.diagnostics.get('reply_artifact')
        if artifact:
            assert Path(artifact['path']).read_text(encoding='utf-8')==reply
            assert artifact['bytes']==len(reply.encode('utf-8'))
        bundles.append(normalized(dict(history=history(d),snapshot=snapshot.to_record(),job=terminal.to_record(),
                                       decision=result.to_record(),prior=prior),d._layout.ccbd_dir))
        close_kernels()
    def difference(a,b,path='root'):
        if isinstance(a,dict) and isinstance(b,dict):
            for key in sorted(set(a)|set(b)):
                if a.get(key)!=b.get(key):return difference(a.get(key),b.get(key),path+'.'+key)
        if isinstance(a,list) and isinstance(b,list):
            if len(a)!=len(b):return f'{path}: row counts {len(a)} versus {len(b)}'
            for index,(x,y) in enumerate(zip(a,b)):
                if x!=y:return difference(x,y,f'{path}[{index}]')
        return f'{path}: {repr(a)[:320]} versus {repr(b)[:320]}'
    if bundles[0]!=bundles[1]:pytest.fail(difference(*bundles))

def test_invalid_snapshot_rolls_back_terminal_and_artifact(tmp_path,monkeypatch):
    d=setup(tmp_path,'rust',monkeypatch)
    parent=activate(d)
    before=history(d)
    args=wire(d,parent,decision('汉'*1800))
    args['profile_family']='unknown'
    with pytest.raises(ValueError): d._state._native.call('dispatcher.persist_terminal',**args)
    assert history(d)==before
    assert CompletionSnapshotStore(d._layout).load(parent.job_id) is None
    assert not list(d._layout.ccbd_text_artifacts_dir.rglob('*.txt'))
    assert d._state.active_job('pi_lead')==parent.job_id

def test_terminal_replay_does_not_duplicate_records(tmp_path,monkeypatch):
    d=setup(tmp_path,'rust',monkeypatch)
    parent=activate(d)
    args=wire(d,parent,decision())
    first=d._state._native.call('dispatcher.persist_terminal',**args)
    before=history(d)
    again=d._state._native.call('dispatcher.persist_terminal',**args)
    assert first['created'] and not again['created']
    assert again['job']==first['job']
    assert history(d)==before


def test_legacy_request_defaults_and_unknown_record_fields_survive_terminal_write(tmp_path,monkeypatch):
    d=setup(tmp_path,'rust',monkeypatch)
    parent=activate(d)
    raw=parent.to_record()
    raw['opaque_extension']={'version':1,'provider_data':'preserve'}
    for key in ['route_options','body_artifact','silence_on_success']:
        raw['request'].pop(key)
    d._state._native.call('records.append',stream='jobs',agent_name=parent.agent_name,record=raw)
    loaded=d._job_store.get_latest(parent.agent_name,parent.job_id)
    persist_terminal_completion(d,loaded,decision(),finished_at=NOW)
    latest=d._state._native.call('records.find_last',stream='jobs',agent_name=parent.agent_name,fields={'job_id':parent.job_id})
    assert latest['opaque_extension']==raw['opaque_extension']
    assert latest['status']=='completed'


def test_default_unicode_preview_matches_python_snapshot_writer(tmp_path,monkeypatch):
    d=setup(tmp_path,'rust',monkeypatch)
    parent=activate(d)
    text='汉'*200+'a'*180
    terminal,result,_=persist_terminal_completion(d,parent,decision(text),finished_at=NOW)
    snapshot=CompletionSnapshotStore(d._layout).load(parent.job_id)
    assert snapshot.latest_reply_preview==d._snapshot_writer._preview(text)
    assert snapshot.latest_reply_preview=='汉'*200+'a'*40+'...'


@pytest.mark.parametrize('suppress', [False, True])
def test_shared_native_tracker_merges_or_suppresses_and_retires_atomically(tmp_path,monkeypatch,suppress):
    from test_native_completion import profile
    d=setup(tmp_path,'rust',monkeypatch)
    parent=activate(d)
    client=d._state._native
    client.call('completion.create',group_id='live')
    client.call('completion.start',group_id='live',job_id=parent.job_id,agent_name=parent.agent_name,
                provider=parent.provider,profile=profile(CompletionFamily.PROTOCOL_TURN).to_record(),
                started_at=NOW,timeout_s=30)
    for index,kind in enumerate([CompletionItemKind.ANCHOR_SEEN,CompletionItemKind.ASSISTANT_FINAL],start=1):
        cursor=CompletionCursor(CompletionSourceKind.PROTOCOL_EVENT_STREAM,event_seq=index,updated_at=NOW)
        event=CompletionItem(kind,NOW,cursor,parent.provider,parent.agent_name,parent.job_id,{'text':'captured reply'})
        client.call('completion.ingest',group_id='live',job_id=parent.job_id,item=event.to_record())
    d._completion_tracker=SimpleNamespace(_native=client,_group='live')
    incoming=replace(decision('',diagnostics={'suppress_completion_state_merge':suppress}),
                     anchor_seen=False,reply_started=False,reply_stable=False)
    terminal,result,_=persist_terminal_completion(d,parent,incoming,finished_at=NOW)
    assert result.reply==('' if suppress else 'captured reply')
    assert result.anchor_seen is (not suppress)
    assert result.reply_started is (not suppress)
    assert 'suppress_completion_state_merge' not in result.diagnostics
    assert client.call('completion.stats')['tracker_count']==0
    assert d._state.active_job('pi_lead') is None

def interrupted_child(d,status=CompletionStatus.COMPLETED,reason='task_complete'):
    parent=activate(d)
    receipt,_=_submit_plan(d,plan(request('codex_worker','pi_lead',route_options={'mode':'chain'})),accepted_at=NOW)
    persist_terminal_completion(d,parent,decision('delegated'),finished_at=NOW)
    child=d._job_store.get_latest('codex_worker',receipt.jobs[0].job_id)
    assert d._state.pop_next('codex_worker')==child.job_id
    child=replace(child,status=JobStatus.RUNNING)
    d._job_store.append(child)
    d._state.mark_active('codex_worker',child.job_id)
    terminal,_,_=persist_terminal_completion(d,child,decision('child-result',status,reason),finished_at=NOW)
    assert not ReplyStore(d._layout).list_all()
    return parent,terminal

def recovery_runtime(d):
    d._registry=SimpleNamespace(get=lambda name:None,spec_for=lambda name:SimpleNamespace(provider='pi' if name=='pi_lead' else 'codex'))
    d._validate_targets_available=lambda names:None
    d._provider_catalog=SimpleNamespace(get=lambda name:SimpleNamespace(supports_resume=True))

def test_restart_repairs_terminal_child_without_reply_and_submits_one_continuation(tmp_path,monkeypatch):
    d=setup(tmp_path,'rust',monkeypatch)
    parent,child=interrupted_child(d)
    close_kernels()
    d=setup(tmp_path,'rust',monkeypatch)
    d._state.rebuild(d._job_store,agent_names=NAMES)
    recovery_runtime(d)
    repaired=repair_callback_edges(d)
    assert len(repaired)==1
    edge=CallbackEdgeStore(d._layout).list_latest()[0]
    assert edge.state is CallbackEdgeState.CONTINUATION_SUBMITTED
    assert edge.continuation_job_id
    assert d._state.queued_items_for('agent','pi_lead')==(edge.continuation_job_id,)
    assert len(ReplyStore(d._layout).list_all())==1
    before=history(d)
    assert repair_callback_edges(d)==()
    assert history(d)==before
    assert AttemptStore(d._layout).get_latest_by_job_id(child.job_id).attempt_state.value=='completed'

@pytest.mark.parametrize('old_reply', [False, True])
def test_missing_result_recovery_defers_while_retry_owns_logical_child(tmp_path,monkeypatch,old_reply):
    d=setup(tmp_path,'rust',monkeypatch)
    parent,child=interrupted_child(d,CompletionStatus.FAILED,'api_error')
    if old_reply:
        d._message_bureau.record_reply(child,decision('old-error',CompletionStatus.FAILED,'api_error'),
                                      finished_at=NOW,deliver_to_caller=False)
    attempt=AttemptStore(d._layout).get_latest_by_job_id(child.job_id)
    retry=replace(child,job_id='job-retry',status=JobStatus.ACCEPTED,terminal_decision=None)
    d._job_store.append(retry)
    d._state.record(retry)
    d._message_bureau.record_retry_attempt(attempt.message_id,retry,accepted_at=NOW)
    before=history(d)
    result=d._state._native.call('callback.repair',state_id=d._state._id,agent_names=NAMES)
    assert result=={'repaired':[],'actions':[]}
    assert history(d)==before


def real_chain(tmp_path,monkeypatch):
    from test_v2_message_bureau_dispatcher_integration import _bootstrap_test_project, _provider_config, _runtime
    from ccbd.services.registry import AgentRegistry
    from ccbd.services.dispatcher import JobDispatcher
    from storage.paths import PathLayout
    monkeypatch.setenv('CCB_CORE_RUNTIME','rust')
    ctx=_bootstrap_test_project(tmp_path/'real-dispatcher-recovery')
    layout=PathLayout(ctx.project_root)
    config=_provider_config('codex','claude')
    registry=AgentRegistry(layout,config)
    for index,name in enumerate(config.agents):
        registry.upsert(_runtime(name,project_id=ctx.project_id,layout=layout,pid=101+index))
    d=JobDispatcher(layout,config,registry,clock=lambda:NOW)
    parent_id=d.submit(replace(request('codex'),project_id=ctx.project_id)).jobs[0].job_id
    d.tick()
    child_id=d.submit(replace(request('claude','codex',route_options={'mode':'chain'}),project_id=ctx.project_id)).jobs[0].job_id
    d.complete(parent_id,decision('delegated'))
    d.tick()
    assert d._execution_service is None  # In-memory runtime records only.
    return layout,config,d,child_id


def test_failed_child_recovery_resumes_real_dispatcher_retry_before_callback(tmp_path,monkeypatch):
    from ccbd.services.registry import AgentRegistry
    from ccbd.services.dispatcher import JobDispatcher
    layout,config,d,child_id=real_chain(tmp_path,monkeypatch)
    child=d.get(child_id)
    persist_terminal_completion(d,child,decision('temporary API error',CompletionStatus.FAILED,'api_error'),finished_at=NOW)
    original=AttemptStore(layout).get_latest_by_job_id(child_id)
    assert ReplyStore(layout).list_message(original.message_id)==[]
    close_kernels()
    restored=JobDispatcher(layout,config,AgentRegistry(layout,config),clock=lambda:NOW)
    repair_callback_edges(restored)
    latest={row.attempt_id:row for row in AttemptStore(layout).list_message(original.message_id)}
    assert len(latest)==2
    retry=max(latest.values(),key=lambda row:row.retry_index)
    assert retry.retry_index==1
    assert retry.attempt_state.value=='pending'
    assert retry.job_id in restored._state.queued_items_for('agent','claude')
    assert ReplyStore(layout).list_message(original.message_id)==[]
    edge=CallbackEdgeStore(layout).get_latest_for_child_job(child_id)
    assert edge.state is CallbackEdgeState.PENDING
    assert not edge.continuation_job_id


def test_silent_cancel_intent_survives_restart_without_synthetic_reply(tmp_path,monkeypatch):
    from ccbd.services.registry import AgentRegistry
    from ccbd.services.dispatcher import JobDispatcher
    layout,config,d,child_id=real_chain(tmp_path,monkeypatch)
    d.cancel(child_id,record_reply=False)
    child=d.get(child_id)
    assert child.terminal_decision['diagnostics']['result_reply_enabled'] is False
    close_kernels()
    restored=JobDispatcher(layout,config,AgentRegistry(layout,config),clock=lambda:NOW)
    assert repair_callback_edges(restored)==()
    attempt=AttemptStore(layout).get_latest_by_job_id(child_id)
    assert ReplyStore(layout).list_message(attempt.message_id)==[]
    edge=CallbackEdgeStore(layout).get_latest_for_child_job(child_id)
    assert edge.state is CallbackEdgeState.PENDING
    assert not edge.continuation_job_id
