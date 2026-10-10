from __future__ import annotations

from dataclasses import dataclass, field, replace
from enum import Enum
from threading import RLock
from typing import Any

from storage.jsonl_store import JsonlStore
from storage.paths import PathLayout
from native_kernel import guard_legacy_writes
from native_records import NativeRecordStore, record_backend, record_lookup

SCHEMA_VERSION = 1


class CallbackEdgeState(str, Enum):
    PENDING = 'pending'
    CHILD_COMPLETED = 'child_completed'
    CONTINUATION_SUBMITTED = 'continuation_submitted'
    DONE = 'done'
    FAILED = 'failed'
    TIMED_OUT = 'timed_out'


@dataclass(frozen=True)
class CallbackEdgeRecord:
    edge_id: str
    parent_job_id: str
    parent_message_id: str
    parent_agent: str
    child_job_id: str
    child_message_id: str
    callback_target_agent: str
    original_caller: str
    original_task_id: str | None
    state: CallbackEdgeState = CallbackEdgeState.PENDING
    child_reply_id: str | None = None
    child_status: str | None = None
    continuation_job_id: str | None = None
    continuation_message_id: str | None = None
    timeout_at: str | None = None
    created_at: str = ''
    updated_at: str = ''
    diagnostics: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if not self.edge_id:
            raise ValueError('edge_id cannot be empty')
        if not self.parent_job_id:
            raise ValueError('parent_job_id cannot be empty')
        if not self.parent_message_id:
            raise ValueError('parent_message_id cannot be empty')
        if not self.child_job_id:
            raise ValueError('child_job_id cannot be empty')
        if not self.child_message_id:
            raise ValueError('child_message_id cannot be empty')
        object.__setattr__(self, 'state', CallbackEdgeState(self.state))
        object.__setattr__(self, 'diagnostics', dict(self.diagnostics or {}))

    def to_record(self) -> dict[str, Any]:
        return {
            'schema_version': SCHEMA_VERSION,
            'record_type': 'callback_edge',
            'edge_id': self.edge_id,
            'parent_job_id': self.parent_job_id,
            'parent_message_id': self.parent_message_id,
            'parent_agent': self.parent_agent,
            'child_job_id': self.child_job_id,
            'child_message_id': self.child_message_id,
            'callback_target_agent': self.callback_target_agent,
            'original_caller': self.original_caller,
            'original_task_id': self.original_task_id,
            'state': self.state.value,
            'child_reply_id': self.child_reply_id,
            'child_status': self.child_status,
            'continuation_job_id': self.continuation_job_id,
            'continuation_message_id': self.continuation_message_id,
            'timeout_at': self.timeout_at,
            'created_at': self.created_at,
            'updated_at': self.updated_at,
            'diagnostics': dict(self.diagnostics),
        }

    @classmethod
    def from_record(cls, record: dict[str, Any]) -> 'CallbackEdgeRecord':
        if int(record.get('schema_version') or 0) != SCHEMA_VERSION:
            raise ValueError(f'schema_version must be {SCHEMA_VERSION}')
        if record.get('record_type') != 'callback_edge':
            raise ValueError("record_type must be 'callback_edge'")
        return cls(
            edge_id=str(record['edge_id']),
            parent_job_id=str(record['parent_job_id']),
            parent_message_id=str(record['parent_message_id']),
            parent_agent=str(record['parent_agent']),
            child_job_id=str(record['child_job_id']),
            child_message_id=str(record['child_message_id']),
            callback_target_agent=str(record['callback_target_agent']),
            original_caller=str(record['original_caller']),
            original_task_id=record.get('original_task_id'),
            state=CallbackEdgeState(record.get('state', CallbackEdgeState.PENDING.value)),
            child_reply_id=record.get('child_reply_id'),
            child_status=record.get('child_status'),
            continuation_job_id=record.get('continuation_job_id'),
            continuation_message_id=record.get('continuation_message_id'),
            timeout_at=record.get('timeout_at'),
            created_at=str(record.get('created_at') or ''),
            updated_at=str(record.get('updated_at') or ''),
            diagnostics=dict(record.get('diagnostics') or {}),
        )


class CallbackEdgeStore:
    def __init__(self, layout: PathLayout, store: JsonlStore | None = None) -> None:
        self._layout = layout
        self._store = record_backend(layout, store)
        self._native = getattr(self._store, '_native', None)
        self._cache_lock = RLock()
        self._latest_by_id: dict[str, CallbackEdgeRecord] | None = None
        self._cache_signature: tuple[int, int, int, int] | None = None

    def append(self, record: CallbackEdgeRecord) -> None:
        if self._native is not None:
            self._store.append(self._layout.ccbd_callback_edges_path,record,serializer=lambda value:value.to_record())
            return
        with self._cache_lock:
            signature_before = self._path_signature()
            self._store.append(
                self._layout.ccbd_callback_edges_path,
                record,
                serializer=lambda value: value.to_record(),
            )
            if self._latest_by_id is not None and signature_before == self._cache_signature:
                self._latest_by_id[record.edge_id] = record
                self._cache_signature = self._path_signature()
                return
            self._latest_by_id = None
            self._cache_signature = None

    def list_all(self) -> list[CallbackEdgeRecord]:
        return self._store.read_all(self._layout.ccbd_callback_edges_path, loader=CallbackEdgeRecord.from_record)

    def list_latest(self) -> tuple[CallbackEdgeRecord, ...]:
        with self._cache_lock:
            return tuple(self._latest_records_locked().values())

    def get_latest(self, edge_id: str) -> CallbackEdgeRecord | None:
        if self._native is not None:
            return self._native_latest({'edge_id':edge_id})
        with self._cache_lock:
            return self._latest_records_locked().get(edge_id)

    def get_latest_for_child_job(self, child_job_id: str) -> CallbackEdgeRecord | None:
        if self._native is not None:
            return self._native_latest({'child_job_id':child_job_id})
        return next((record for record in self.list_latest() if record.child_job_id == child_job_id), None)

    def get_latest_for_child_message(self, child_message_id: str) -> CallbackEdgeRecord | None:
        if self._native is not None:
            return self._native_latest({'child_message_id':child_message_id})
        return next((record for record in self.list_latest() if record.child_message_id == child_message_id), None)

    def get_latest_for_parent_job(self, parent_job_id: str) -> CallbackEdgeRecord | None:
        if self._native is not None:
            return self._native_latest({'parent_job_id':parent_job_id})
        return next((record for record in self.list_latest() if record.parent_job_id == parent_job_id), None)

    def get_latest_continuation_for_edge(self, edge_id: str) -> CallbackEdgeRecord | None:
        return record_lookup(self._store,
            self._layout.ccbd_callback_edges_path,
            predicate=lambda payload: str(payload.get('edge_id') or '') == edge_id
            and bool(str(payload.get('continuation_job_id') or '').strip()),
            loader=CallbackEdgeRecord.from_record,
            fields={'edge_id':edge_id},
            nonempty=['continuation_job_id'],
        )

    def update(self, record: CallbackEdgeRecord, **changes) -> CallbackEdgeRecord:
        if self._native is not None:
            return CallbackEdgeRecord.from_record(self._native.call('callback.update',record=record.to_record(),changes=changes))
        updated = replace(record, **changes)
        self.append(updated)
        return updated

    def _path_signature(self) -> tuple[int, int, int, int] | None:
        try:
            stat = self._layout.ccbd_callback_edges_path.stat()
        except FileNotFoundError:
            return None
        return int(stat.st_dev), int(stat.st_ino), int(stat.st_size), int(stat.st_mtime_ns)

    def _native_latest(self, fields):
        record = self._native.call('records.latest_find',stream='callbacks',identity='edge_id',fields=fields)
        return None if record is None else CallbackEdgeRecord.from_record(record)

    def _latest_records_locked(self) -> dict[str, CallbackEdgeRecord]:
        if self._native is not None:
            return {record.edge_id:record for record in self._store.latest(self._layout.ccbd_callback_edges_path,
                                                                          'edge_id',CallbackEdgeRecord.from_record)}
        signature = self._path_signature()
        if self._latest_by_id is None or signature != self._cache_signature:
            latest: dict[str, CallbackEdgeRecord] = {}
            for record in self.list_all():
                latest[record.edge_id] = record
            self._latest_by_id = latest
            self._cache_signature = self._path_signature()
        return self._latest_by_id


__all__ = [
    'CallbackEdgeRecord',
    'CallbackEdgeState',
    'CallbackEdgeStore',
    'SCHEMA_VERSION',
]

guard_legacy_writes(CallbackEdgeStore,'append','update')
