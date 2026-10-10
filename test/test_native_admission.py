"""Admission parity, atomic rejection and callback policy through real Rust IPC."""
from dataclasses import replace
import os
from pathlib import Path
import re
from types import SimpleNamespace
from uuid import uuid4

import pytest

from ccbd.api_models import DeliveryScope, MessageEnvelope, JobStatus, TargetKind
from ccbd.services.dispatcher_runtime.native_state import create_dispatcher_state
from ccbd.services.dispatcher_runtime.submission_models import _JobDraft, _SubmissionPlan, _message_for_agent
from ccbd.services.dispatcher_runtime.submission_recording import _submit_plan
from ccbd.services.dispatcher_runtime.callbacks import validate_callback_request
from jobs.store import JobStore, JobEventStore, SubmissionStore
from mailbox_kernel import InboundEventStore, MailboxStore
from message_bureau import MessageBureauFacade, MessageStore, AttemptStore, CallbackEdgeStore
from native_kernel import close_kernels
from storage.paths import PathLayout

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT/'rust'/'target'/'debug'/('ccb-kernel.exe' if os.name == 'nt' else 'ccb-kernel')
NOW = '2026-10-11T10:00:00Z'
NAMES = ['pi_lead', 'codex_worker', 'claude_reviewer']

@pytest.fixture(autouse=True)
def native_environment(monkeypatch):
    assert BIN.is_file()
    monkeypatch.setenv('CCB_KERNEL_BIN', str(BIN))
    close_kernels()
    yield
    close_kernels()

def dispatcher(root, mode, monkeypatch):
    monkeypatch.setenv('CCB_CORE_RUNTIME', mode)
    layout = PathLayout(root)
    config = SimpleNamespace(agents=dict.fromkeys(NAMES), max_callback_depth=5)
    return SimpleNamespace(_layout=layout, _config=config,
        _state=create_dispatcher_state(layout, NAMES), _job_store=JobStore(layout),
        _event_store=JobEventStore(layout), _submission_store=SubmissionStore(layout),
        _message_bureau=MessageBureauFacade(layout, config=config, clock=lambda: NOW),
        _registry=SimpleNamespace(get=lambda name: None), _runtime_service=None,
        _dispatch_error=ValueError, _clock=lambda: NOW,
        _new_id=lambda prefix: f'{prefix}_{uuid4().hex[:12]}')

def request(target='pi_lead', actor='user', **values):
    return MessageEnvelope('project', target, actor, values.pop('body', 'task'), 'task', None, 'ask',
                           DeliveryScope.BROADCAST if target == 'all' else DeliveryScope.SINGLE, **values)

def plan(message, targets=None):
    targets = targets or [message.to_agent]
    return _SubmissionPlan(message.project_id, message.from_actor, message, message.task_id,
        tuple(_JobDraft(name, 'pi' if name == 'pi_lead' else 'codex', _message_for_agent(message, agent_name=name),
                        TargetKind.AGENT, name) for name in targets),
        submission_id='sub-broadcast' if message.to_agent == 'all' else None,
        target_scope='all' if message.to_agent == 'all' else None)

def history(d):
    layout = d._layout
    return dict(jobs={n:[r.to_record() for r in d._job_store.list_agent(n)] for n in NAMES},
        events={n:d._event_store._store.read_all(layout.target_events_path('agent', n)) for n in NAMES},
        messages=[r.to_record() for r in MessageStore(layout).list_all()],
        attempts=[r.to_record() for r in AttemptStore(layout).list_all()],
        edges=[r.to_record() for r in CallbackEdgeStore(layout).list_all()],
        submissions=[r.to_record() for r in d._submission_store.list_all()],
        inbox={n:[r.to_record() for r in InboundEventStore(layout).list_agent(n)] for n in NAMES},
        mailboxes={n:r.to_record() if (r:=MailboxStore(layout).load(n)) else None for n in NAMES},
        queues={n:list(d._state.queued_items_for('agent', n)) for n in NAMES})

def canonical(value):
    identities = {}
    def walk(v):
        if isinstance(v, dict): return {k:walk(v[k]) for k in sorted(v)}
        if isinstance(v, (list, tuple)): return [walk(x) for x in v]
        if isinstance(v, str):
            def substitute(m):
                key=m.group()
                if key not in identities: identities[key]=f'{key.split("_")[0]}-id-{len(identities)}'
                return identities[key]
            return re.sub(r'\b(?:job|evt|msg|att|cb|iev)_[0-9a-f]{12}\b', substitute, v)
        return v
    return walk(value)

def activate(d):
    receipt, _ = _submit_plan(d, plan(request()), accepted_at=NOW)
    id = receipt.jobs[0].job_id
    parent = d._job_store.get_latest('pi_lead', id)
    parent = replace(parent, status=JobStatus.RUNNING)
    d._job_store.append(parent)
    assert d._state.pop_next('pi_lead') == id
    d._state.mark_active('pi_lead', id)
    return parent

@pytest.mark.parametrize('scenario', ['single_queued', 'broadcast', 'chain_artifact'])
def test_full_admission_matches_python_history(tmp_path, monkeypatch, scenario):
    results=[]
    for mode in ['python', 'rust']:
        d=dispatcher(tmp_path/mode, mode, monkeypatch)
        if scenario == 'chain_artifact':
            activate(d)
            message=request('codex_worker', 'pi_lead', body='  '+'汉'*850+'\n\nCCB_REPLY_MODE: hidden',
                route_options={'mode':'chain'}, body_artifact={'path':'C:/artifact.txt','bytes':2550,'sha256':'abc'})
            validate_callback_request(d, message)
            receipt, id = _submit_plan(d, plan(message), accepted_at=NOW)
        elif scenario == 'broadcast':
            receipt, id = _submit_plan(d, plan(request('all'), NAMES), accepted_at=NOW)
        else:
            _submit_plan(d, plan(request()), accepted_at=NOW)
            receipt, id = _submit_plan(d, plan(request()), accepted_at=NOW)
            assert receipt.jobs[0].status is JobStatus.QUEUED
        results.append(canonical(dict(receipt=receipt.to_record(),message=id,history=history(d))))
        close_kernels()
    assert results[0] == results[1]

def wire(d, value):
    return dict(state_id=d._state._id, accepted_at=NOW, agent_names=NAMES, bureau_enabled=True,
        max_callback_depth=5, plan=dict(project_id=value.project_id, from_actor=value.from_actor,
            task_id=value.task_id, request=value.request.to_record(), submission_id=value.submission_id,
            target_scope=value.target_scope, origin_message_id=value.origin_message_id,
            drafts=[dict(agent_name=x.agent_name, provider=x.provider, request=x.request.to_record(),
                         target_kind=x.target_kind.value, target_name=x.target_name,
                         workspace_path=x.workspace_path, provider_options=x.provider_options) for x in value.drafts]))

def test_bad_later_draft_rolls_back_jobs_events_and_bureau(tmp_path, monkeypatch):
    d=dispatcher(tmp_path, 'rust', monkeypatch)
    before=history(d)
    args=wire(d, plan(request('all'), NAMES))
    args['plan']['drafts'][1]['provider']=''
    with pytest.raises(ValueError, match='provider'):
        d._state._native.call('dispatcher.submit', **args)
    assert history(d)==before
    receipt,_=_submit_plan(d, plan(request()), accepted_at=NOW)
    assert receipt.jobs[0].status is JobStatus.ACCEPTED

def test_native_transaction_rechecks_callback_policy_after_preflight(tmp_path, monkeypatch):
    d=dispatcher(tmp_path, 'rust', monkeypatch)
    parent=activate(d)
    message=request('codex_worker','pi_lead',route_options={'mode':'chain'})
    validate_callback_request(d,message)
    d._state.clear_active('pi_lead',job_id=parent.job_id)
    before=history(d)
    with pytest.raises(ValueError,match='active parent'):
        _submit_plan(d,plan(message),accepted_at=NOW)
    assert history(d)==before

def test_native_rejects_duplicate_chain_without_partial_admission(tmp_path, monkeypatch):
    d=dispatcher(tmp_path, 'rust', monkeypatch)
    activate(d)
    message=request('codex_worker','pi_lead',route_options={'mode':'chain'})
    _submit_plan(d,plan(message),accepted_at=NOW)
    before=history(d)
    with pytest.raises(ValueError,match='one outstanding'):
        _submit_plan(d,plan(message),accepted_at=NOW)
    assert history(d)==before

def test_admission_rebuild_after_owner_restart_keeps_fifo_and_edge(tmp_path, monkeypatch):
    d=dispatcher(tmp_path, 'rust', monkeypatch)
    activate(d)
    child,_=_submit_plan(d,plan(request('codex_worker','pi_lead',route_options={'mode':'chain'})),accepted_at=NOW)
    tail,_=_submit_plan(d,plan(request('codex_worker',silence_on_success=True)),accepted_at=NOW)
    close_kernels()
    d=dispatcher(tmp_path, 'rust', monkeypatch)
    d._state.rebuild(d._job_store, agent_names=NAMES)
    assert d._state.queued_items_for('agent','codex_worker') == (child.jobs[0].job_id,tail.jobs[0].job_id)
    edge=CallbackEdgeStore(d._layout).list_latest()[0]
    assert edge.child_job_id==child.jobs[0].job_id
    assert len(AttemptStore(d._layout).list_all())==3


def test_native_rejects_unbound_restricted_tree_after_staging_without_partial_records(tmp_path, monkeypatch):
    d=dispatcher(tmp_path, 'rust', monkeypatch)
    parent=activate(d)
    parent.request.route_options.update(allowed_chain_targets=['codex_worker'], bind_chain_workspace_tree=True)
    d._job_store.append(parent)
    before=history(d)
    message=request('codex_worker','pi_lead',route_options={'mode':'chain'})
    with pytest.raises(ValueError,match='bound parent workspace tree'):
        _submit_plan(d,plan(message),accepted_at=NOW)
    assert history(d)==before


def test_native_rejects_terminal_parent_even_if_active_index_has_not_cleared(tmp_path, monkeypatch):
    d=dispatcher(tmp_path, 'rust', monkeypatch)
    parent=activate(d)
    d._job_store.append(replace(parent,status=JobStatus.COMPLETED,terminal_decision={'reason':'done'}))
    before=history(d)
    with pytest.raises(ValueError,match='active parent'):
        _submit_plan(d,plan(request('codex_worker','pi_lead',route_options={'mode':'chain'})),accepted_at=NOW)
    assert history(d)==before


def test_failed_runtime_projection_keeps_committed_receipt_and_does_not_replay(tmp_path, monkeypatch, caplog):
    from ccbd.services.dispatcher_runtime import runtime_state
    d=dispatcher(tmp_path, 'rust', monkeypatch)
    def failed_projection(*args, **kwargs):
        raise OSError('projection unavailable')
    monkeypatch.setattr(runtime_state, 'sync_runtime', failed_projection)
    receipt,message=_submit_plan(d,plan(request()),accepted_at=NOW)
    assert receipt.jobs[0].status is JobStatus.ACCEPTED
    assert MessageStore(d._layout).get_latest(message) is not None
    assert d._state.queued_items_for('agent','pi_lead')==(receipt.jobs[0].job_id,)
    assert len(d._job_store.list_agent('pi_lead'))==1
    assert 'Accepted admission; runtime projection failed' in caplog.text
