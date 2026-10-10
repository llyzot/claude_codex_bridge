from __future__ import annotations

from agents.models import normalize_agent_name
from storage.jsonl_store import JsonlStore
from storage.paths import PathLayout
from native_kernel import guard_legacy_writes
from native_records import record_backend, record_lookup

from .models import AttemptRecord, MessageRecord, ReplyRecord


class MessageStore:
    def __init__(self, layout: PathLayout, store: JsonlStore | None = None) -> None:
        self._layout = layout
        self._store = record_backend(layout, store)
        self._native = getattr(self._store, '_native', None)

    def append(self, record: MessageRecord) -> None:
        self._store.append(self._layout.ccbd_messages_path, record, serializer=lambda value: value.to_record())

    def list_all(self) -> list[MessageRecord]:
        return self._store.read_all(self._layout.ccbd_messages_path, loader=MessageRecord.from_record)

    def get_latest(self, message_id: str) -> MessageRecord | None:
        return record_lookup(self._store,
            self._layout.ccbd_messages_path,
            predicate=lambda payload: str(payload.get('message_id') or '') == message_id,
            loader=MessageRecord.from_record,
            fields={'message_id':message_id},
        )

    def list_submission(self, submission_id: str) -> list[MessageRecord]:
        return [record for record in self.list_all() if record.submission_id == submission_id]


class AttemptStore:
    def __init__(self, layout: PathLayout, store: JsonlStore | None = None) -> None:
        self._layout = layout
        self._store = record_backend(layout, store)
        self._native = getattr(self._store, '_native', None)

    def append(self, record: AttemptRecord) -> None:
        self._store.append(self._layout.ccbd_attempts_path, record, serializer=lambda value: value.to_record())

    def list_all(self) -> list[AttemptRecord]:
        return self._store.read_all(self._layout.ccbd_attempts_path, loader=AttemptRecord.from_record)

    def get_latest(self, attempt_id: str) -> AttemptRecord | None:
        return record_lookup(self._store,
            self._layout.ccbd_attempts_path,
            predicate=lambda payload: str(payload.get('attempt_id') or '') == attempt_id,
            loader=AttemptRecord.from_record,
            fields={'attempt_id':attempt_id},
        )

    def get_latest_by_job_id(self, job_id: str) -> AttemptRecord | None:
        return record_lookup(self._store,
            self._layout.ccbd_attempts_path,
            predicate=lambda payload: str(payload.get('job_id') or '') == job_id,
            loader=AttemptRecord.from_record,
            fields={'job_id':job_id},
        )

    def get_latest_by_message_id(self, message_id: str, *, exclude_job_id: str | None = None) -> AttemptRecord | None:
        excluded = str(exclude_job_id or '').strip()
        return record_lookup(self._store,
            self._layout.ccbd_attempts_path,
            predicate=lambda payload: (
                str(payload.get('message_id') or '') == message_id
                and (not excluded or str(payload.get('job_id') or '') != excluded)
            ),
            loader=AttemptRecord.from_record,
            fields={'message_id':message_id},
            exclude={'job_id':excluded} if excluded else None,
        )

    def get_latest_by_message_agent(self, message_id: str, agent_name: str) -> AttemptRecord | None:
        normalized = normalize_agent_name(agent_name)
        return record_lookup(self._store,
            self._layout.ccbd_attempts_path,
            predicate=lambda payload: (
                str(payload.get('message_id') or '') == message_id
                and normalize_agent_name(str(payload.get('agent_name') or '')) == normalized
            ),
            loader=AttemptRecord.from_record,
            fields={'message_id':message_id,'agent_name':normalized},
        )

    def list_message(self, message_id: str) -> list[AttemptRecord]:
        return [record for record in self.list_all() if record.message_id == message_id]

    def list_agent(self, agent_name: str) -> list[AttemptRecord]:
        normalized = normalize_agent_name(agent_name)
        return [record for record in self.list_all() if record.agent_name == normalized]


class ReplyStore:
    def __init__(self, layout: PathLayout, store: JsonlStore | None = None) -> None:
        self._layout = layout
        self._store = record_backend(layout, store)
        self._native = getattr(self._store, '_native', None)

    def append(self, record: ReplyRecord) -> None:
        self._store.append(self._layout.ccbd_replies_path, record, serializer=lambda value: value.to_record())

    def list_all(self) -> list[ReplyRecord]:
        return self._store.read_all(self._layout.ccbd_replies_path, loader=ReplyRecord.from_record)

    def get_latest(self, reply_id: str) -> ReplyRecord | None:
        return record_lookup(self._store,
            self._layout.ccbd_replies_path,
            predicate=lambda payload: str(payload.get('reply_id') or '') == reply_id,
            loader=ReplyRecord.from_record,
            fields={'reply_id':reply_id},
        )

    def list_message(self, message_id: str) -> list[ReplyRecord]:
        return [record for record in self.list_all() if record.message_id == message_id]


__all__ = ['AttemptStore', 'MessageStore', 'ReplyStore']

guard_legacy_writes(MessageStore,'append')
guard_legacy_writes(AttemptStore,'append')
guard_legacy_writes(ReplyStore,'append')
