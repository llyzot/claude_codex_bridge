"""Record model adapters; Rust owns record IO, filtering and transitions."""
from __future__ import annotations

from pathlib import Path

from agents.models import normalize_agent_name
from native_kernel import kernel_for_layout, rust_core_enabled
from storage.jsonl_store import JsonlStore


class NativeRecordStore:
    def __init__(self, layout):
        self.layout = layout
        self._native = kernel_for_layout(layout)
        self._paths = {
            layout.ccbd_messages_path: 'messages', layout.ccbd_attempts_path: 'attempts',
            layout.ccbd_replies_path: 'replies', layout.ccbd_callback_edges_path: 'callbacks',
            layout.ccbd_submissions_path: 'submissions',
        }

    def _target(self, path):
        path = Path(path)
        if path in self._paths:
            return {'stream':self._paths[path]}
        try:
            relative = path.relative_to(self.layout.agents_dir)
        except ValueError as error:
            raise ValueError('Record path is outside native-owned streams') from error
        if len(relative.parts) != 2 or relative.name not in {'jobs.jsonl','events.jsonl'}:
            raise ValueError('Record path is outside native-owned streams')
        return {'stream':'jobs' if relative.name == 'jobs.jsonl' else 'job_events',
                'agent_name':normalize_agent_name(relative.parts[0])}

    def append(self, path, row, serializer=None):
        record = serializer(row) if serializer else row
        self._native.call('records.append', **self._target(path),record=record)

    def read_all(self, path, loader=None):
        return self._load(self._native.call('records.list',**self._target(path)),loader)

    def read_tail(self, path, limit, loader=None, *, ignore_invalid=False):
        if limit < 0:
            raise ValueError('limit cannot be negative')
        if limit == 0:
            return []
        # The legacy store's ignore_invalid includes loader validation errors.
        budget = limit
        while True:
            rows = self._native.call('records.tail',**self._target(path),limit=budget,ignore_invalid=ignore_invalid)
            result = []
            for row in rows:
                try:
                    result.append(loader(row) if loader else row)
                except Exception:
                    if not ignore_invalid:
                        raise
            if not ignore_invalid or len(result) >= limit or len(rows) < budget:
                return result[-limit:]
            budget *= 2

    def read_since(self, path, start_line=0, loader=None):
        if start_line < 0:
            raise ValueError('start_line cannot be negative')
        result = self._native.call('records.read_since',**self._target(path),start_line=start_line)
        return result['line_no'],self._load(result['rows'],loader)

    def find_matching(self, path, *, fields, loader=None, exclude=None, nonempty=None):
        row = self._native.call('records.find_last',**self._target(path),fields=fields,
                                exclude=exclude or {},nonempty=nonempty or [])
        return None if row is None else loader(row) if loader else row

    def latest(self, path, identity, loader=None):
        return self._load(self._native.call('records.latest',**self._target(path),identity=identity),loader)

    @staticmethod
    def _load(rows, loader):
        return [loader(row) if loader else row for row in rows]


def record_backend(layout, custom=None):
    if not rust_core_enabled():
        return custom or JsonlStore()
    if custom is not None:
        if isinstance(custom,NativeRecordStore) and custom.layout.ccbd_dir == layout.ccbd_dir:
            return custom
        raise ValueError('Rust core requires a native record store for its state directory')
    return NativeRecordStore(layout)


def record_lookup(store, path, *, predicate, loader, fields, exclude=None, nonempty=None):
    if isinstance(store,NativeRecordStore):
        return store.find_matching(path,fields=fields,exclude=exclude,nonempty=nonempty,loader=loader)
    return store.find_last(path,predicate=predicate,loader=loader)
