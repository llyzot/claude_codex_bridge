from __future__ import annotations

from dataclasses import replace

from ccbd.api_models import JobRecord, JobStatus
from completion.models import CompletionDecision
from mailbox_kernel import InboundEventStatus

from .facade_recording_common import attempt_state_for_status
from .facade_state import rebuild_mailbox_summary, refresh_message_state, set_message_state
from .models import AttemptState, MessageState

_TERMINAL_INBOUND_STATUSES = {
    InboundEventStatus.CONSUMED,
    InboundEventStatus.SUPERSEDED,
    InboundEventStatus.ABANDONED,
}


def mark_attempt_started(service, job: JobRecord, *, started_at: str) -> None:
    native = getattr(service._attempt_store,'_native',None)
    if native is not None:
        native.call('bureau.attempt_started',job=job.to_record(),timestamp=started_at,
                    lease_ttl_seconds=service._mailbox_kernel._lease_ttl_seconds)
        return
    attempt = service._attempt_store.get_latest_by_job_id(job.job_id)
    if attempt is None:
        return
    service._attempt_store.append(
        replace(
            attempt,
            started_at=attempt.started_at or started_at,
            updated_at=started_at,
            attempt_state=AttemptState.RUNNING,
        )
    )
    inbound = _resolve_inbound_for_attempt_start(service, job, attempt_id=attempt.attempt_id)
    if inbound is not None and inbound.status not in _TERMINAL_INBOUND_STATUSES:
        if inbound.status is InboundEventStatus.DELIVERING:
            mailbox_updated = True
        else:
            service._mailbox_kernel.claim(
                job.agent_name,
                inbound.inbound_event_id,
                started_at=started_at,
            )
            mailbox_updated = True
    else:
        mailbox_updated = False
    set_message_state(service, attempt.message_id, MessageState.RUNNING, updated_at=started_at)
    if not mailbox_updated:
        rebuild_mailbox_summary(service, job.agent_name, updated_at=started_at)


def record_attempt_terminal(service, job: JobRecord, decision: CompletionDecision, *, finished_at: str) -> None:
    native = getattr(service._attempt_store,'_native',None)
    if native is not None:
        native.call('bureau.attempt_terminal',job=job.to_record(),timestamp=finished_at)
        return
    del decision
    attempt = service._attempt_store.get_latest_by_job_id(job.job_id)
    if attempt is None:
        return

    service._attempt_store.append(
        replace(
            attempt,
            updated_at=finished_at,
            attempt_state=attempt_state_for_status(job.status),
        )
    )

    inbound = service._inbound_store.get_latest_for_attempt(job.agent_name, attempt.attempt_id)
    if inbound is not None and inbound.status not in _TERMINAL_INBOUND_STATUSES:
        if inbound.status in {InboundEventStatus.CREATED, InboundEventStatus.QUEUED} and job.status is JobStatus.CANCELLED:
            service._mailbox_kernel.abandon(job.agent_name, inbound.inbound_event_id, finished_at=finished_at)
        else:
            service._mailbox_kernel.consume(job.agent_name, inbound.inbound_event_id, finished_at=finished_at)
    else:
        rebuild_mailbox_summary(service, job.agent_name, updated_at=finished_at)

    refresh_message_state(service, attempt.message_id, updated_at=finished_at)


def _resolve_inbound_for_attempt_start(service, job: JobRecord, *, attempt_id: str):
    if str(job.request.message_type or '').strip().lower() == 'reply_delivery' or bool(job.provider_options.get('reply_delivery')):
        inbound_event_id = str(job.provider_options.get('reply_delivery_inbound_event_id') or '').strip() or None
        if inbound_event_id:
            inbound = service._inbound_store.get_latest(job.agent_name, inbound_event_id)
            if inbound is not None:
                return inbound
    return service._inbound_store.get_latest_for_attempt(job.agent_name, attempt_id)


__all__ = [
    'mark_attempt_started',
    'record_attempt_terminal',
]
