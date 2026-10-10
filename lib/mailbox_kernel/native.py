"""Adapt the existing mailbox interface to Rust without duplicate state logic."""
from __future__ import annotations

from enum import Enum
from functools import wraps
import inspect

from native_kernel import kernel_for_layout, legacy_writer
from .models import DeliveryLease, InboundEventRecord, MailboxRecord

_EVENT_LIST = {'latest_events', 'pending_events'}
_EVENT = {'head_pending_event', 'peek_next', 'claim', 'claim_next', 'ack_reply',
          'consume', 'abandon', 'supersede', 'rewrite_head', 'expire_lease'}
_SUMMARY = {'rebuild_mailbox_summary', 'project_mailbox_summary', 'refresh_mailbox',
            'apply_incremental_summary_update', 'apply_transition_summary_update', 'upsert_mailbox_summary'}
_READ_ONLY = _EVENT_LIST | {'head_pending_event', 'peek_next'}


def _wire(value):
    if isinstance(value, Enum):
        return value.value
    if hasattr(value, 'to_record'):
        return value.to_record()
    return value


def native_mailbox_service(cls):
    for name in _EVENT_LIST | _EVENT | _SUMMARY | {'expired_delivery_leases'}:
        original = getattr(cls, name)
        signature = inspect.signature(original)
        setattr(cls, name, _method(name, original, signature))
    return cls


def _method(name, original, signature):
    @wraps(original)
    def call(self, *args, **kwargs):
        if self._native is None:
            if name in _READ_ONLY | {'expired_delivery_leases', 'project_mailbox_summary'}:
                return original(self, *args, **kwargs)
            with legacy_writer(self._layout):
                return original(self, *args, **kwargs)
        bound = signature.bind(self, *args, **kwargs)
        bound.apply_defaults()
        values = {key: _wire(value) for key, value in bound.arguments.items()
                  if key != 'self' and value is not Ellipsis}
        if name in _READ_ONLY:
            pass  # Reads must not advance a caller-provided logical clock.
        elif name == 'expired_delivery_leases':
            values['now'] = values.get('now') or self._clock()
        elif name == 'ack_reply':
            values['timestamp'] = values.get('finished_at') or values.get('started_at') or self._clock()
        else:
            values['timestamp'] = values.get('updated_at') or values.get('finished_at') or values.get('started_at') or self._clock()
        values['lease_ttl_seconds'] = self._lease_ttl_seconds
        result = self._native.call(name, **values)
        if name in _EVENT_LIST:
            return tuple(InboundEventRecord.from_record(value) for value in result)
        if name in _EVENT:
            return None if result is None else InboundEventRecord.from_record(result)
        if name == 'expired_delivery_leases':
            return tuple(DeliveryLease.from_record(value) for value in result)
        return MailboxRecord.from_record(result)
    return call


def connect_native(layout, stores):
    client = kernel_for_layout(layout)
    if client is not None:
        for store in stores:
            if store is not None and getattr(store, '_native', None) is not client:
                raise ValueError('Rust mailbox requires stores using the same native kernel')
    return client
