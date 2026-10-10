"""Actual Rust record and callback transactions against Python contract fixtures."""
from dataclasses import replace
import os
from pathlib import Path
import re
from types import SimpleNamespace

import pytest

from ccbd.api_models import DeliveryScope, JobRecord, JobStatus, MessageEnvelope
from ccbd.services.dispatcher_runtime.native_state import create_dispatcher_state
from completion.models import CompletionDecision, CompletionStatus, CompletionConfidence
from jobs.store import JobStore
from mailbox_kernel import InboundEventStore, MailboxStore
from message_bureau import MessageBureauFacade, MessageStore, AttemptStore, ReplyStore, CallbackEdgeRecord, CallbackEdgeState, CallbackEdgeStore
from native_kernel import close_kernels, KernelUnavailable
from storage.paths import PathLayout

ROOT=Path(__file__).resolve().parents[1]
BIN=ROOT/'rust'/'target'/'debug'/('ccb-kernel.exe' if os.name=='nt' else 'ccb-kernel')
NOW='2026-10-11T10:00:00Z'

@pytest.fixture(autouse=True)
def environment(monkeypatch):
    assert BIN.is_file(), 'Build ccb-kernel before integration tests'
    monkeypatch.setenv('CCB_KERNEL_BIN',str(BIN))
    close_kernels()
    yield
    close_kernels()

def setup(root,mode,monkeypatch):
    monkeypatch.setenv('CCB_CORE_RUNTIME',mode)
    layout=PathLayout(root)
    config=SimpleNamespace(agents={'pi_lead':None,'codex_worker':None})
    bureau=MessageBureauFacade(layout,config=config,clock=lambda:NOW)
    return layout,bureau

def job(id='job-original',target='codex_worker',caller='pi_lead',*,silence=False):
    request=MessageEnvelope(project_id='project',to_agent=target,from_actor=caller,body='task',task_id='task',reply_to=None,
                            message_type='ask',delivery_scope=DeliveryScope.SINGLE,silence_on_success=silence)
    return JobRecord(id,None,target,'pi' if target=='pi_lead' else 'codex',request,JobStatus.ACCEPTED,None,None,NOW,NOW)

def decision(status,reply):
    return CompletionDecision(True,CompletionStatus(status.value),'test',CompletionConfidence.OBSERVED,reply,
                              True,True,True,'turn-1',None,NOW,{})

def canonical(bundle):
    names={}
    counts={}
    def transform(value):
        if isinstance(value,dict): return {key:transform(value[key]) for key in sorted(value)}
        if isinstance(value,list): return [transform(v) for v in value]
        if isinstance(value,str):
            def rewrite(match):
                id=match.group()
                if id not in names:
                    prefix=id.split('_')[0]
                    counts[prefix]=counts.get(prefix,0)+1
                    names[id]=f'{prefix}_{counts[prefix]}'
                return names[id]
            return re.sub(r'\b(?:msg|att|rep|iev)_[0-9a-f]{12}\b',rewrite,value)
        return value
    return transform(bundle)

def records(layout):
    return {name:[row.to_record() for row in cls(layout).list_all()] for name,cls in
            [('messages',MessageStore),('attempts',AttemptStore),('replies',ReplyStore)]} | {
                'inbox':{name:[row.to_record() for row in InboundEventStore(layout).list_agent(name)] for name in ['pi_lead','codex_worker']},
                'mailbox':{name:value.to_record() if (value:=MailboxStore(layout).load(name)) else None for name in ['pi_lead','codex_worker']}}

@pytest.mark.parametrize('status,reply,silence',[(JobStatus.COMPLETED,'done',False),(JobStatus.COMPLETED,'done',True),
                                               (JobStatus.CANCELLED,'',False),(JobStatus.FAILED,'error',False)])
def test_submission_start_and_terminal_history_parity(tmp_path,monkeypatch,status,reply,silence):
    bundles=[]
    for mode in ['python','rust']:
        layout,bureau=setup(tmp_path/mode,mode,monkeypatch)
        task=job(silence=silence)
        bureau.record_submission(task.request,(task,),submission_id=None,accepted_at=NOW)
        running=replace(task,status=JobStatus.RUNNING)
        bureau.mark_attempt_started(running,started_at=NOW)
        result=decision(status,reply)
        terminal=replace(running,status=status,terminal_decision=result.to_record())
        bureau.record_terminal(terminal,result,finished_at=NOW)
        bundles.append(canonical(records(layout)))
    assert bundles[0]==bundles[1]

def test_rejected_admission_rolls_back_all_streams(tmp_path,monkeypatch):
    layout,bureau=setup(tmp_path/'repo','rust',monkeypatch)
    good=job()
    bad=replace(good,job_id='job-invalid',provider='')
    with pytest.raises(ValueError,match='provider'):
        bureau.record_submission(good.request,(good,bad),submission_id=None,accepted_at=NOW)
    assert MessageStore(layout).list_all()==[]
    assert AttemptStore(layout).list_all()==[]
    assert InboundEventStore(layout).list_agent('codex_worker')==[]
    assert MailboxStore(layout).load('codex_worker') is None

def test_callback_admission_is_idempotent_and_restores_from_disk(tmp_path,monkeypatch):
    layout,bureau=setup(tmp_path/'repo','rust',monkeypatch)
    parent=job('job-parent','pi_lead','user')
    parent=replace(parent,status=JobStatus.RUNNING)
    parent_message=bureau.record_submission(parent.request,(parent,),submission_id=None,accepted_at=NOW)
    JobStore(layout).append(parent)
    edge=CallbackEdgeRecord('cb-edge','job-parent',parent_message,'pi_lead','job-child','msg-child','pi_lead','user','task',
                            state=CallbackEdgeState.CHILD_COMPLETED,created_at=NOW,updated_at=NOW)
    CallbackEdgeStore(layout).append(edge)
    state=create_dispatcher_state(layout,['pi_lead','codex_worker'])
    state.record(parent)
    state.mark_active('pi_lead',parent.job_id)
    request=MessageEnvelope('project','pi_lead','user','result','task',parent_message,'chain_continuation',DeliveryScope.SINGLE,
                            route_options={'chain_edge_id':'cb-edge','chain_parent_job_id':'job-parent','chain_child_job_id':'job-child'})
    args=dict(edge_id='cb-edge',request=request.to_record(),provider='pi',workspace_path=None,state_id=state._id,finished_at=NOW)
    first=state._native.call('callback.enqueue',**args)
    second=state._native.call('callback.enqueue',**args)
    assert first['created'] and not second['created']
    continuation=first['job']['job_id']
    assert [row.job_id for row in JobStore(layout).list_agent('pi_lead')]==['job-parent',continuation]
    assert len([row for row in AttemptStore(layout).list_all() if row.job_id==continuation])==1
    close_kernels()
    layout,bureau=setup(tmp_path/'repo','rust',monkeypatch)
    restored=create_dispatcher_state(layout,['pi_lead','codex_worker'])
    restored.rebuild(JobStore(layout),agent_names=['pi_lead','codex_worker'],mailbox_order={'pi_lead':[continuation]})
    assert restored.active_job('pi_lead')=='job-parent'
    assert restored.queued_items_for('agent','pi_lead')==(continuation,)
    assert CallbackEdgeStore(layout).get_latest('cb-edge').continuation_job_id==continuation

def test_python_record_writer_cannot_bypass_native_owner(tmp_path,monkeypatch):
    layout,bureau=setup(tmp_path/'repo','rust',monkeypatch)
    task=job()
    message=bureau.record_submission(task.request,(task,),submission_id=None,accepted_at=NOW)
    current=MessageStore(layout).get_latest(message)
    monkeypatch.setenv('CCB_CORE_RUNTIME','python')
    with pytest.raises(KernelUnavailable,match='active writer'):
        MessageStore(layout).append(current)


def test_callback_index_updates_without_rescanning_history(tmp_path,monkeypatch):
    layout,_=setup(tmp_path/'repo','rust',monkeypatch)
    store=CallbackEdgeStore(layout)
    edge=CallbackEdgeRecord('cb-edge','parent','parent-msg','pi_lead','child','child-msg','pi_lead','user','task',
                            created_at=NOW,updated_at=NOW)
    store.append(edge)
    assert store.get_latest(edge.edge_id)==edge
    for index in range(40):
        edge=store.update(edge,state=CallbackEdgeState.CHILD_COMPLETED,diagnostics={'index':index})
        assert store.get_latest(edge.edge_id)==edge
    assert store._native.call('records.metrics')['history_reads']==1


def test_native_tail_preserves_invalid_row_skip_contract(tmp_path,monkeypatch):
    from native_records import NativeRecordStore
    layout,_=setup(tmp_path/'repo','rust',monkeypatch)
    # A recorded legacy fixture may contain malformed diagnostic rows. Reading
    # a valid tail must not parse unrelated older rows; ignore_invalid fills
    # its limit using earlier valid records.
    path=layout.ccbd_messages_path
    path.parent.mkdir(parents=True,exist_ok=True)
    path.write_text('malformed old row\n{"value":1}\nnot-json\n',encoding='utf-8')
    store=NativeRecordStore(layout)
    assert store.read_tail(path,1,ignore_invalid=True)==[{'value':1}]
    assert store.read_tail(path,0)==[]
    with pytest.raises(ValueError):
        store.read_tail(path,1)
