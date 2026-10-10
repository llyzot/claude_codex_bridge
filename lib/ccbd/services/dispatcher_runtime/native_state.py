"""Dispatcher index owned by the persistent Rust kernel."""
from __future__ import annotations

import inspect
from uuid import uuid4
from weakref import finalize

from ccbd.api_models import TargetKind
from native_kernel import kernel_for_layout
from .state_agents import DispatcherStateAgentMixin


def _release_state(client,state_id):
    try:
        client.call('scheduler.drop',state_id=state_id)
    except Exception:
        pass  # A retired graph does not revive a closed kernel.


class NativeDispatcherState(DispatcherStateAgentMixin):
    def __init__(self, client, agent_names):
        self._native = client
        self._id = uuid4().hex
        client.call('scheduler.create', state_id=self._id, agent_names=list(agent_names))
        self._cleanup=finalize(self,_release_state,client,self._id)

    @property
    def _queues(self):
        # Read-only compatibility view for lifecycle's work-pending check.
        rows = self._native.call('scheduler.queues',state_id=self._id)
        return {(TargetKind(row['target_kind']),row['target_name']):row['items'] for row in rows}

    def rebuild(self, job_store, *, agent_names, mailbox_order=None):
        names = list(agent_names)
        self._native.call('scheduler.rebuild',state_id=self._id,agent_names=names,
                          records={name:[record.to_record() for record in job_store.list_agent(name)] for name in names},
                          mailbox_order=dict(mailbox_order or {}))


def _delegate(name, signature):
    def call(self,*args,**kwargs):
        bound = signature.bind(self,*args,**kwargs)
        bound.apply_defaults()
        values = {key:value for key,value in bound.arguments.items() if key != 'self'}
        for key in ('target_kind',):
            if key in values:
                values[key] = TargetKind(values[key]).value
        if 'record' in values:
            values['record'] = values['record'].to_record()
        result = self._native.call('scheduler.'+name,state_id=self._id,**values)
        if name == 'queued_items_for':
            return tuple(result)
        if name == 'active_items':
            return tuple((TargetKind(kind),target,job) for kind,target,job in result)
        if name == 'slots':
            return tuple((TargetKind(kind),target) for kind,target in result)
        if name == 'target_for_job':
            return None if result is None else (TargetKind(result[0]),result[1])
        return result
    return call


def create_dispatcher_state(layout, agent_names):
    from .state import DispatcherState
    client = kernel_for_layout(layout)
    if client is None:
        return DispatcherState(agent_names)
    for name in ('queue_depth_for','has_outstanding_for','enqueue_for','pop_next_for','queued_items_for',
                 'remove_queued_for','mark_active_for','clear_active_for','active_job_for','active_items',
                 'slots','target_for_job','remember_job','record'):
        if not hasattr(NativeDispatcherState,name):
            setattr(NativeDispatcherState,name,_delegate(name,inspect.signature(getattr(DispatcherState,name))))
    return NativeDispatcherState(client,agent_names)
