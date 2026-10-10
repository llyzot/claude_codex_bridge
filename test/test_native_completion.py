"""Differential event traces against every existing completion family."""
import os
from pathlib import Path
import random
from itertools import product
from types import SimpleNamespace

import pytest

from agents.models import RuntimeMode
from ccbd.api_models import JobRecord,JobStatus,MessageEnvelope,DeliveryScope
from completion.models import CompletionFamily,CompletionSourceKind,SelectorFamily,CompletionProfile,CompletionCursor,CompletionItem,CompletionItemKind
from completion.registry import CompletionRegistry
from completion.tracker import CompletionTrackerService
from native_kernel import KernelClient
from provider_core.catalog import build_default_provider_catalog

ROOT=Path(__file__).resolve().parents[1]
BIN=ROOT/'rust'/'target'/'debug'/('ccb-kernel.exe' if os.name=='nt' else 'ccb-kernel')
START='2026-10-11T10:00:00Z'

class OracleRegistry(CompletionRegistry):
    def __init__(self,profile):self.profile=profile
    def build_profile(self,*_args):return self.profile

def profile(family):
    source={CompletionFamily.PROTOCOL_TURN:CompletionSourceKind.PROTOCOL_EVENT_STREAM,
            CompletionFamily.STRUCTURED_RESULT:CompletionSourceKind.STRUCTURED_RESULT_STREAM,
            CompletionFamily.SESSION_BOUNDARY:CompletionSourceKind.SESSION_EVENT_LOG,
            CompletionFamily.ANCHORED_SESSION_STABILITY:CompletionSourceKind.SESSION_SNAPSHOT,
            CompletionFamily.TERMINAL_TEXT_QUIET:CompletionSourceKind.TERMINAL_TEXT}[family]
    return CompletionProfile('codex',RuntimeMode.PANE_BACKED,family,source,True,True,True,True,True,SelectorFamily.FINAL_MESSAGE)

def task():
    request=MessageEnvelope('project','agent1','user','task',None,None,'ask',DeliveryScope.SINGLE)
    return JobRecord('job1',None,'agent1','codex',request,JobStatus.RUNNING,None,None,START,START)

def item(kind,index,payload,source):
    ts=f'2026-10-11T10:00:{index:02d}Z'
    return CompletionItem(kind,ts,CompletionCursor(source,event_seq=index,updated_at=ts),'codex','agent1','job1',payload)

def wire(view):
    return {'job_id':view.job_id,'agent_name':view.agent_name,'state':view.state.to_record(),'decision':view.decision.to_record()}

@pytest.mark.parametrize('family',list(CompletionFamily))
@pytest.mark.parametrize('seed',[1,17,94])
def test_event_trace_matches_python(family,seed,tmp_path,monkeypatch):
    monkeypatch.setenv('CCB_CORE_RUNTIME','python')
    monkeypatch.setenv('CCB_KERNEL_BIN',str(BIN))
    policy=profile(family)
    oracle=CompletionTrackerService(SimpleNamespace(agents={'agent1':SimpleNamespace(provider='codex',runtime_mode=RuntimeMode.PANE_BACKED)}),
                                   build_default_provider_catalog(),OracleRegistry(policy),request_timeout_s=30)
    expected=oracle.start(task(),started_at=START)
    client=KernelClient(tmp_path/'ccbd')
    try:
        client.call('completion.create',group_id='trace')
        actual=client.call('completion.start',group_id='trace',job_id='job1',agent_name='agent1',provider='codex',profile=policy.to_record(),started_at=START,timeout_s=30)
        assert actual==wire(expected)
        rng=random.Random(seed)
        kinds=[CompletionItemKind.ANCHOR_SEEN,CompletionItemKind.ASSISTANT_CHUNK,CompletionItemKind.ASSISTANT_FINAL,
               CompletionItemKind.TOOL_CALL,CompletionItemKind.TOOL_RESULT,CompletionItemKind.SESSION_ROTATE,
               CompletionItemKind.SESSION_SNAPSHOT,CompletionItemKind.SESSION_MUTATION]
        for index in range(1,15):
            event=item(rng.choice(kinds),index,{'text':f' answer {index} ','message_id':f'm{index}',
                       'message_count':index,'last_updated':None,'tool_call_count':rng.choice([0,1,'0',False]),'fallback_text':f'fallback {index}'},policy.completion_source_kind)
            assert client.call('completion.ingest',group_id='trace',job_id='job1',item=event.to_record())==wire(oracle.ingest('job1',event))
            assert client.call('completion.tick',group_id='trace',job_id='job1',now=event.timestamp)==wire(oracle.tick('job1',now=event.timestamp))
        for index,kind in [(15,CompletionItemKind.RESULT),(16,CompletionItemKind.TURN_BOUNDARY),(17,CompletionItemKind.TURN_ABORTED)]:
            event=item(kind,index,{'reply':'final','last_agent_message':'last','reason':'task_complete','status':'CANCELLED'},policy.completion_source_kind)
            assert client.call('completion.ingest',group_id='trace',job_id='job1',item=event.to_record())==wire(oracle.ingest('job1',event))
        assert client.call('completion.tick',group_id='trace',job_id='job1',now='2026-10-11T10:01:00Z')==wire(oracle.tick('job1',now='2026-10-11T10:01:00Z'))
    finally:client.close()

@pytest.mark.parametrize('field,value',[('req_id','foreign-job'),('agent_name','other_agent'),('provider','claude')])
def test_foreign_terminal_event_cannot_complete_request(field,value,tmp_path,monkeypatch):
    monkeypatch.setenv('CCB_KERNEL_BIN',str(BIN))
    client=KernelClient(tmp_path/'ccbd')
    try:
        client.call('completion.create',group_id='guard')
        initial=client.call('completion.start',group_id='guard',job_id='job1',agent_name='agent1',provider='codex',
                            profile=profile(CompletionFamily.PROTOCOL_TURN).to_record(),started_at=START,timeout_s=30)
        event=item(CompletionItemKind.TURN_BOUNDARY,1,{'reply':'foreign answer'},CompletionSourceKind.PROTOCOL_EVENT_STREAM).to_record()
        event[field]=value
        assert client.call('completion.ingest',group_id='guard',job_id='job1',item=event)==initial
    finally:client.close()

def test_groups_and_finish_are_isolated(tmp_path,monkeypatch):
    monkeypatch.setenv('CCB_KERNEL_BIN',str(BIN))
    client=KernelClient(tmp_path/'ccbd')
    try:
        for group in ['old','new']:
            client.call('completion.create',group_id=group)
            client.call('completion.start',group_id=group,job_id='job1',agent_name='agent1',provider='codex',
                        profile=profile(CompletionFamily.PROTOCOL_TURN).to_record(),started_at=START,timeout_s=0)
        client.call('completion.finish',group_id='old',job_id='job1')
        assert client.call('completion.current',group_id='old',job_id='job1') is None
        assert client.call('completion.current',group_id='new',job_id='job1') is not None
        client.call('completion.drop_group',group_id='old')
        assert len(client.call('completion.tick_all',group_id='new',now='2026-10-11T12:00:00Z'))==1
    finally:client.close()


def test_provider_authority_and_delivery_gate_matches_python(tmp_path,monkeypatch):
    from completion.models import CompletionDecision,CompletionConfidence,CompletionStatus
    from completion.native_gate import validate_native_completion
    from ccbd.services.dispatcher_runtime.polling_service import _validate_provider_completion_decision
    monkeypatch.setenv('CCB_KERNEL_BIN',str(BIN))
    client=KernelClient(tmp_path/'ccbd')
    try:
        for provider,mode,delivery,anchor,accepted,text,no_wrap in product(['codex','claude'],['active','passive'],[False,True],[False,True],[False,True],['','reply'],[False,True]):
            submission=SimpleNamespace(provider=provider,source_kind=CompletionSourceKind.PROTOCOL_EVENT_STREAM,
                runtime_state={'mode':mode,'delivery_state':'accepted' if accepted else 'pending_anchor','anchor_seen':anchor,'no_wrap':no_wrap})
            identity=SimpleNamespace(request=SimpleNamespace(message_type='reply_delivery' if delivery else 'ask'),provider_options={})
            decision=CompletionDecision(True,CompletionStatus.COMPLETED,'task_complete',CompletionConfidence.EXACT,text,
                                        False,False,False,None,None,START,{})
            expected=_validate_provider_completion_decision(submission,decision,identity)
            actual=validate_native_completion(client,submission,decision,identity)
            assert actual.to_record()==expected.to_record(),(provider,mode,delivery,anchor,accepted,text,no_wrap)
        for reason in ['task_complete_empty_reply','hook_stop_empty_reply','api_error','timeout']:
            for delivery in [False,True]:
                submission=SimpleNamespace(provider='claude',source_kind=CompletionSourceKind.SESSION_EVENT_LOG,runtime_state={})
                identity=SimpleNamespace(request=SimpleNamespace(message_type='reply_delivery' if delivery else 'ask'),provider_options={})
                decision=CompletionDecision(True,CompletionStatus.INCOMPLETE,reason,CompletionConfidence.OBSERVED,'',False,False,False,None,None,START,{})
                assert validate_native_completion(client,submission,decision,identity).to_record()==_validate_provider_completion_decision(submission,decision,identity).to_record()
        for source in ['herdr_agent_state','herdr-agent-state','herdr_agent_state_ref']:
            decision=CompletionDecision(True,CompletionStatus.COMPLETED,'idle',CompletionConfidence.EXACT,'reply',True,True,True,None,None,START,{'completion_source':source})
            assert validate_native_completion(client,None,decision,None).to_record()==_validate_provider_completion_decision(None,decision,None).to_record()
    finally:client.close()


def test_snapshot_native_ownership_and_restart(tmp_path,monkeypatch):
    from completion.models import CompletionSnapshot,CompletionState,CompletionDecision
    from completion.snapshot_store import CompletionSnapshotStore
    from native_kernel import close_kernels,KernelUnavailable
    from storage.paths import PathLayout
    monkeypatch.setenv('CCB_KERNEL_BIN',str(BIN))
    monkeypatch.setenv('CCB_CORE_RUNTIME','rust')
    layout=PathLayout(tmp_path/'repo')
    native=CompletionSnapshotStore(layout)
    snapshot=CompletionSnapshot('job1','agent1',CompletionFamily.PROTOCOL_TURN,CompletionState(),CompletionDecision.pending(),'preview',START)
    native.save(snapshot)
    assert native.load('job1').to_record()==snapshot.to_record()
    monkeypatch.setenv('CCB_CORE_RUNTIME','python')
    with pytest.raises(KernelUnavailable,match='active writer'):
        CompletionSnapshotStore(layout).save(snapshot)
    close_kernels()
    monkeypatch.setenv('CCB_CORE_RUNTIME','rust')
    assert CompletionSnapshotStore(layout).load('job1').to_record()==snapshot.to_record()
    close_kernels()


def test_retired_graph_releases_native_completion_and_scheduler_state(tmp_path,monkeypatch):
    import gc
    from native_kernel import kernel_for_layout,close_kernels
    from storage.paths import PathLayout
    from ccbd.services.dispatcher_runtime.native_state import create_dispatcher_state
    monkeypatch.setenv('CCB_KERNEL_BIN',str(BIN))
    monkeypatch.setenv('CCB_CORE_RUNTIME','rust')
    layout=PathLayout(tmp_path/'repo')
    client=kernel_for_layout(layout)
    tracker=CompletionTrackerService(SimpleNamespace(agents={}),build_default_provider_catalog(),layout=layout)
    state=create_dispatcher_state(layout,['agent1'])
    assert client.call('completion.stats')['group_count']==1
    assert client.call('scheduler.stats')['state_count']==1
    del tracker,state
    gc.collect()
    assert client.call('completion.stats')['group_count']==0
    assert client.call('scheduler.stats')['state_count']==0
    close_kernels()
