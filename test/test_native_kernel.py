"""Contract tests against the real persistent Rust process, without AI CLIs."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from dataclasses import replace
import os
from pathlib import Path
import random

import pytest

from mailbox_kernel import (InboundEventRecord, InboundEventStatus, InboundEventType,
                            InboundEventStore, MailboxKernelService, MailboxStore, DeliveryLeaseStore)
from native_kernel import KernelClient, KernelUnavailable, close_kernels
from storage.paths import PathLayout

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / 'rust' / 'target' / 'debug' / ('ccb-kernel.exe' if os.name == 'nt' else 'ccb-kernel')


@pytest.fixture(autouse=True)
def native_binary(monkeypatch):
    assert BIN.is_file(), 'Build ccb-kernel before running its integration tests'
    monkeypatch.setenv('CCB_KERNEL_BIN', str(BIN))
    close_kernels()
    yield
    close_kernels()


def event(index, *, reply=False, agent='pi_lead'):
    return InboundEventRecord(f'evt-{index}', agent,
                             InboundEventType.TASK_REPLY if reply else InboundEventType.TASK_REQUEST,
                             f'msg-{index}', f'att-{index}', f'job:{index}', 100,
                             InboundEventStatus.QUEUED, '2026-10-11T10:00:00Z')


def setup(root, mode, monkeypatch):
    monkeypatch.setenv('CCB_CORE_RUNTIME', mode)
    layout = PathLayout(root)
    inbox = InboundEventStore(layout)
    mailbox = MailboxStore(layout)
    lease = DeliveryLeaseStore(layout)
    service = MailboxKernelService(layout, clock=lambda: '2026-10-11T10:00:01Z', lease_ttl_seconds=60,
                                   inbound_store=inbox, mailbox_store=mailbox, lease_store=lease)
    return layout, inbox, mailbox, lease, service


def wire(value):
    if isinstance(value, (tuple, list)):
        return [wire(v) for v in value]
    return value.to_record() if hasattr(value, 'to_record') else value


def test_randomized_state_transitions_match_python(tmp_path, monkeypatch):
    py = setup(tmp_path / 'python', 'python', monkeypatch)
    rs = setup(tmp_path / 'rust', 'rust', monkeypatch)
    rng = random.Random(617)
    count = 0
    for step in range(160):
        if count == 0 or rng.random() < .25:
            count += 1
            record = event(count, reply=rng.choice([True, False]))
            py[1].append(record)
            rs[1].append(record)
        else:
            op = rng.choice(['claim', 'claim_next', 'consume', 'abandon', 'supersede', 'ack_reply',
                             'rewrite_head', 'expire_lease', 'rebuild_mailbox_summary'])
            args = ['pi_lead']
            kwargs = {}
            if op in {'claim','consume','abandon','supersede','ack_reply','rewrite_head'}:
                args.append(f'evt-{rng.randint(1,count+1)}')
            if op == 'rewrite_head':
                kwargs = dict(payload_ref='job:rewritten', status=InboundEventStatus.QUEUED, clear_progress=True)
            if op == 'claim_next':
                kwargs['event_type'] = rng.choice([None, InboundEventType.TASK_REQUEST, InboundEventType.TASK_REPLY])
            actual = wire(getattr(rs[4],op)(*args,**kwargs))
            expected = wire(getattr(py[4],op)(*args,**kwargs))
            assert actual == expected, (step,op)
        assert wire(rs[4].latest_events('pi_lead')) == wire(py[4].latest_events('pi_lead'))
        assert wire(rs[2].load('pi_lead')) == wire(py[2].load('pi_lead'))
        assert wire(rs[3].load('pi_lead')) == wire(py[3].load('pi_lead'))


def test_incremental_summary_and_compare_save_parity(tmp_path, monkeypatch):
    py = setup(tmp_path/'python','python',monkeypatch)
    rs = setup(tmp_path/'rust','rust',monkeypatch)
    for state in (py,rs):
        state[1].append(event(1,reply=True))
    for kwargs in [dict(queue_delta=1,pending_reply_delta=1),dict(active_inbound_event_id='evt-1'),
                   dict(active_inbound_event_id=None,last_finished_at='2026-10-11T10:00:05Z'),dict(queue_delta=-10)]:
        assert wire(rs[4].apply_incremental_summary_update('pi_lead',**kwargs)) == wire(py[4].apply_incremental_summary_update('pi_lead',**kwargs))
    for state in (py,rs):
        prior = state[2].load('pi_lead')
        update = replace(prior,summary_version=prior.summary_version+1)
        assert not state[2].compare_and_save(update,expected_summary_version=0)
        assert state[2].compare_and_save(update,expected_summary_version=prior.summary_version)
    assert wire(rs[2].load('pi_lead')) == wire(py[2].load('pi_lead'))


def test_restart_preserves_mailbox_fifo_and_lease(tmp_path, monkeypatch):
    state = setup(tmp_path/'repo','rust',monkeypatch)
    layout,inbox,mailbox,lease,service = state
    inbox.append(event(1,reply=True))
    inbox.append(event(2))
    service.claim_next('pi_lead')
    before = wire(mailbox.load('pi_lead'))
    first_pid = service._native.process.pid
    close_kernels()
    state = setup(tmp_path/'repo','rust',monkeypatch)
    assert state[4]._native.process.pid != first_pid
    assert wire(state[2].load('pi_lead')) == before
    assert state[4].claim('pi_lead','evt-2') is None
    state[4].consume('pi_lead','evt-1')
    assert state[4].claim_next('pi_lead').inbound_event_id == 'evt-2'
    assert state[3].load('pi_lead').lease_version == 2


def test_parallel_transport_serializes_mutations_and_one_claim(tmp_path, monkeypatch):
    _,inbox,_,lease,service = setup(tmp_path/'repo','rust',monkeypatch)
    with ThreadPoolExecutor(max_workers=8) as workers:
        list(workers.map(lambda i: inbox.append(event(i)),range(30)))
    ids = [e.inbound_event_id for e in service.latest_events('pi_lead')]
    assert len(ids) == len(set(ids)) == 30
    with ThreadPoolExecutor(max_workers=8) as workers:
        claimed = list(workers.map(lambda id: service.claim('pi_lead',id),ids))
    assert {e.inbound_event_id for e in claimed if e is not None} == {ids[0]}
    assert lease.load('pi_lead').inbound_event_id == ids[0]


def test_dead_kernel_never_falls_back_or_replays(tmp_path, monkeypatch):
    _,inbox,mailbox,_,service = setup(tmp_path/'repo','rust',monkeypatch)
    inbox.append(event(1))
    process = service._native.process
    process.kill()
    process.wait(timeout=5)
    with pytest.raises(KernelUnavailable):
        service.consume('pi_lead','evt-1')
    with pytest.raises(KernelUnavailable):
        inbox.append(event(2))
    close_kernels()
    _,inbox,_,_,service = setup(tmp_path/'repo','rust',monkeypatch)
    assert len(inbox.list_agent('pi_lead')) == 1
    assert service.peek_next('pi_lead').status is InboundEventStatus.QUEUED


def test_another_writer_and_invalid_schema_are_rejected(tmp_path, monkeypatch):
    layout,inbox,_,_,service = setup(tmp_path/'repo','rust',monkeypatch)
    with pytest.raises(KernelUnavailable):
        KernelClient(layout.ccbd_dir)
    client = service._native
    bad = event(1).to_record()
    bad['schema_version'] = 99
    with pytest.raises(ValueError,match='schema'):
        client.call('inbox.append',agent_name='pi_lead',record=bad)
    assert inbox.list_agent('pi_lead') == []
    inbox.append(event(2))
    assert len(inbox.list_agent('pi_lead')) == 1


def test_read_queries_do_not_advance_logical_clock(tmp_path, monkeypatch):
    monkeypatch.setenv('CCB_CORE_RUNTIME','rust')
    layout = PathLayout(tmp_path/'repo')
    calls = []
    service = MailboxKernelService(layout,clock=lambda: calls.append('tick') or '2026-10-11T10:00:00Z')
    InboundEventStore(layout).append(event(1))
    service.latest_events('pi_lead')
    service.pending_events('pi_lead')
    service.head_pending_event('pi_lead')
    service.peek_next('pi_lead')
    assert calls == []
    service.claim_next('pi_lead')
    assert calls == ['tick']


def test_scheduler_indices_and_activation_order_match_python(tmp_path, monkeypatch):
    from ccbd.services.dispatcher_runtime.native_state import create_dispatcher_state
    from ccbd.services.dispatcher_runtime.state import DispatcherState
    monkeypatch.setenv('CCB_CORE_RUNTIME','rust')
    native = create_dispatcher_state(PathLayout(tmp_path/'repo'),['pi_lead','codex_worker'])
    python = DispatcherState(['pi_lead','codex_worker'])
    rng = random.Random(100)
    for index in range(100):
        agent = rng.choice(['pi_lead','codex_worker'])
        job = f'job-{rng.randrange(10)}'
        operation = rng.choice(['enqueue','pop_next','remove_queued','mark_active','clear_active'])
        args = [agent] if operation in {'pop_next','clear_active'} else [agent,job]
        assert getattr(native,operation)(*args) == getattr(python,operation)(*args)
        native.remember_job(job,'agent',agent)
        python.remember_job(job,'agent',agent)
        assert native.target_for_job(job) == python.target_for_job(job)
        assert native.active_items() == python.active_items()
        assert native.slots() == python.slots()
        for name in ['pi_lead','codex_worker']:
            assert native.queued_items_for('agent',name) == python.queued_items_for('agent',name)
            assert native.queue_depth(name) == python.queue_depth(name)


def test_expiry_and_filtered_fifo_head_match_python(tmp_path, monkeypatch):
    py = setup(tmp_path/'python','python',monkeypatch)
    rs = setup(tmp_path/'rust','rust',monkeypatch)
    for state in (py,rs):
        state[1].append(event(1,reply=True))
        state[1].append(event(2))
        assert state[4].peek_next('pi_lead',event_type=InboundEventType.TASK_REQUEST) is None
        state[4].claim_next('pi_lead',started_at='2026-10-11T10:00:00Z')
    for now in ['2026-10-11T10:00:59Z','2026-10-11T10:01:00Z']:
        assert wire(rs[4].expired_delivery_leases(now=now)) == wire(py[4].expired_delivery_leases(now=now))
    assert wire(rs[4].expire_lease('pi_lead',finished_at='2026-10-11T10:01:00Z')) == wire(py[4].expire_lease('pi_lead',finished_at='2026-10-11T10:01:00Z'))
    assert wire(rs[3].load('pi_lead')) == wire(py[3].load('pi_lead'))
    assert wire(rs[2].load('pi_lead')) == wire(py[2].load('pi_lead'))


def test_native_selection_survives_control_plane_environment_filter():
    from runtime_env.control_plane import control_plane_env
    result = control_plane_env(environ={'CCB_CORE_RUNTIME':'rust','CCB_KERNEL_BIN':str(BIN)})
    assert result['CCB_CORE_RUNTIME'] == 'rust'
    assert result['CCB_KERNEL_BIN'] == str(BIN)


def test_rejected_summary_does_not_commit_partial_changes(tmp_path, monkeypatch):
    _,inbox,mailbox,_,service = setup(tmp_path/'repo','rust',monkeypatch)
    inbox.append(event(1))
    before = wire(service.rebuild_mailbox_summary('pi_lead'))
    with pytest.raises(ValueError):
        service.apply_transition_summary_update('pi_lead',queue_depth=0,pending_reply_count=0,
                                                 active_inbound_event_id=None,summary_source='')
    assert wire(mailbox.load('pi_lead')) == before
    assert len(inbox.list_agent('pi_lead')) == 1


def test_python_cannot_write_while_rust_owns_the_state(tmp_path, monkeypatch):
    _,native,_,_,_ = setup(tmp_path/'repo','rust',monkeypatch)
    _,python,_,_,_ = setup(tmp_path/'repo','python',monkeypatch)
    native.append(event(1))
    with pytest.raises(KernelUnavailable,match='active writer'):
        python.append(event(2))
    close_kernels()
    # An old lock file is not evidence of a live writer: switching back once
    # the owning process has exited remains possible.
    python.append(event(2))
    assert len(python.list_agent('pi_lead')) == 2
