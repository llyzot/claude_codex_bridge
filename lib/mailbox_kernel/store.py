from __future__ import annotations

from storage.json_store import JsonStore
from storage.jsonl_store import JsonlStore
from storage.paths import PathLayout
from native_kernel import guard_legacy_writes, kernel_for_layout

from .models import DeliveryLease, InboundEventRecord, MailboxRecord


class MailboxStore:
    def __init__(self, layout: PathLayout, store: JsonStore | None = None) -> None:
        self._layout = layout
        self._store = store or JsonStore()
        self._native = kernel_for_layout(layout)
        if self._native is not None and store is not None:
            raise ValueError('Rust mailbox cannot use a custom JSON store')

    def load(self, agent_name: str) -> MailboxRecord | None:
        if self._native is not None:
            record = self._native.call('mailbox.load', agent_name=agent_name)
            return None if record is None else MailboxRecord.from_record(record)
        path = self._layout.agent_mailbox_path(agent_name)
        if not path.exists():
            return None
        return self._store.load(path, loader=MailboxRecord.from_record)

    def save(self, record: MailboxRecord) -> None:
        if self._native is not None:
            self._native.call('mailbox.save', agent_name=record.agent_name, record=record.to_record())
            return
        self._store.save(
            self._layout.agent_mailbox_path(record.agent_name),
            record,
            serializer=lambda value: value.to_record(),
        )

    def compare_and_save(self, record: MailboxRecord, *, expected_summary_version: int | None) -> bool:
        if self._native is not None:
            return self._native.call('mailbox.compare_and_save', agent_name=record.agent_name,
                                     record=record.to_record(), expected_summary_version=expected_summary_version)
        current = self.load(record.agent_name)
        current_version = None if current is None else int(current.summary_version)
        if current_version != expected_summary_version:
            return False
        self.save(record)
        return True

    def list_all(self) -> list[MailboxRecord]:
        if self._native is not None:
            return [MailboxRecord.from_record(value) for value in self._native.call('mailbox.list')]
        directory = self._layout.ccbd_mailboxes_dir
        if not directory.exists():
            return []
        records: list[MailboxRecord] = []
        for path in sorted(directory.glob('*/mailbox.json')):
            try:
                records.append(self._store.load(path, loader=MailboxRecord.from_record))
            except Exception:
                continue
        return records


class InboundEventStore:
    def __init__(self, layout: PathLayout, store: JsonlStore | None = None) -> None:
        self._layout = layout
        self._store = store or JsonlStore()
        self._native = kernel_for_layout(layout)
        if self._native is not None and store is not None:
            raise ValueError('Rust mailbox cannot use a custom JSONL store')

    def append(self, record: InboundEventRecord) -> None:
        if self._native is not None:
            self._native.call('inbox.append', agent_name=record.agent_name, record=record.to_record())
            return
        self._store.append(
            self._layout.agent_inbox_path(record.agent_name),
            record,
            serializer=lambda value: value.to_record(),
        )

    def list_agent(self, agent_name: str) -> list[InboundEventRecord]:
        if self._native is not None:
            return [InboundEventRecord.from_record(value) for value in self._native.call('inbox.list', agent_name=agent_name)]
        return self._store.read_all(
            self._layout.agent_inbox_path(agent_name),
            loader=InboundEventRecord.from_record,
        )

    def read_since(self, agent_name: str, start_line: int = 0) -> tuple[int, list[InboundEventRecord]]:
        if self._native is not None:
            if start_line < 0:
                raise ValueError('start_line cannot be negative')
            result = self._native.call('inbox.read_since', agent_name=agent_name, start_line=start_line)
            return result['line_no'], [InboundEventRecord.from_record(row) for row in result['rows']]
        line_no, rows = self._store.read_since(
            self._layout.agent_inbox_path(agent_name),
            start_line,
            loader=InboundEventRecord.from_record,
        )
        return line_no, list(rows)

    def get_latest(self, agent_name: str, inbound_event_id: str) -> InboundEventRecord | None:
        if self._native is not None:
            row = self._native.call('inbox.latest', agent_name=agent_name, field='inbound_event_id', value=inbound_event_id)
            return None if row is None else InboundEventRecord.from_record(row)
        return self._store.find_last(
            self._layout.agent_inbox_path(agent_name),
            predicate=lambda payload: str(payload.get('inbound_event_id') or '') == inbound_event_id,
            loader=InboundEventRecord.from_record,
        )

    def get_latest_for_attempt(self, agent_name: str, attempt_id: str) -> InboundEventRecord | None:
        if self._native is not None:
            row = self._native.call('inbox.latest', agent_name=agent_name, field='attempt_id', value=attempt_id)
            return None if row is None else InboundEventRecord.from_record(row)
        return self._store.find_last(
            self._layout.agent_inbox_path(agent_name),
            predicate=lambda payload: str(payload.get('attempt_id') or '') == attempt_id,
            loader=InboundEventRecord.from_record,
        )


class DeliveryLeaseStore:
    def __init__(self, layout: PathLayout, store: JsonStore | None = None) -> None:
        self._layout = layout
        self._store = store or JsonStore()
        self._native = kernel_for_layout(layout)
        if self._native is not None and store is not None:
            raise ValueError('Rust mailbox cannot use a custom JSON store')

    def load(self, agent_name: str) -> DeliveryLease | None:
        if self._native is not None:
            value = self._native.call('lease.load', agent_name=agent_name)
            return None if value is None else DeliveryLease.from_record(value)
        path = self._layout.mailbox_lease_path(agent_name)
        if not path.exists():
            return None
        return self._store.load(path, loader=DeliveryLease.from_record)

    def save(self, record: DeliveryLease) -> None:
        if self._native is not None:
            self._native.call('lease.save', agent_name=record.agent_name, record=record.to_record())
            return
        self._store.save(
            self._layout.mailbox_lease_path(record.agent_name),
            record,
            serializer=lambda value: value.to_record(),
        )

    def remove(self, agent_name: str) -> None:
        if self._native is not None:
            self._native.call('lease.remove', agent_name=agent_name)
            return
        path = self._layout.mailbox_lease_path(agent_name)
        try:
            path.unlink()
        except FileNotFoundError:
            return

    def list_all(self) -> list[DeliveryLease]:
        if self._native is not None:
            return [DeliveryLease.from_record(value) for value in self._native.call('lease.list')]
        directory = self._layout.ccbd_leases_dir
        if not directory.exists():
            return []
        leases: list[DeliveryLease] = []
        for path in sorted(directory.glob('*.json')):
            try:
                leases.append(self._store.load(path, loader=DeliveryLease.from_record))
            except Exception:
                continue
        return leases


__all__ = ['DeliveryLeaseStore', 'InboundEventStore', 'MailboxStore']

guard_legacy_writes(MailboxStore, 'save', 'compare_and_save')
guard_legacy_writes(InboundEventStore, 'append')
guard_legacy_writes(DeliveryLeaseStore, 'save', 'remove')
