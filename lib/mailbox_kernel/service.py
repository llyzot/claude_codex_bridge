from __future__ import annotations

from mailbox_runtime.targets import normalize_mailbox_owner_name
from storage.paths import PathLayout

from .models import (
    DeliveryLease,
    InboundEventRecord,
    InboundEventStatus,
    InboundEventType,
    LeaseState,
    MailboxRecord,
    MailboxState,
)
from .service_state import MailboxKernelRuntimeState, MailboxKernelStateMixin
from .store import DeliveryLeaseStore, InboundEventStore, MailboxStore
from .native import connect_native, native_mailbox_service
from .service_runtime import (
    ack_reply,
    apply_incremental_summary_update,
    apply_transition_summary_update,
    claim,
    claim_next,
    expire_lease,
    expired_delivery_leases,
    head_pending_event,
    latest_events,
    mark_terminal,
    peek_next,
    pending_events,
    project_mailbox_summary,
    rebuild_mailbox_summary,
    rewrite_head,
)

_TERMINAL_EVENT_STATES = frozenset(
    {
        InboundEventStatus.CONSUMED,
        InboundEventStatus.SUPERSEDED,
        InboundEventStatus.ABANDONED,
    }
)
_CLAIMABLE_EVENT_STATES = frozenset({InboundEventStatus.CREATED, InboundEventStatus.QUEUED})


@native_mailbox_service
class MailboxKernelService(MailboxKernelStateMixin):
    def __init__(
        self,
        layout: PathLayout,
        *,
        clock,
        mailbox_store: MailboxStore | None = None,
        inbound_store: InboundEventStore | None = None,
        lease_store: DeliveryLeaseStore | None = None,
        lease_ttl_seconds: float | None = None,
    ) -> None:
        self._native = connect_native(layout, (mailbox_store, inbound_store, lease_store))
        self._runtime_state = MailboxKernelRuntimeState(
            layout=layout,
            clock=clock,
            mailbox_store=mailbox_store or MailboxStore(layout),
            inbound_store=inbound_store or InboundEventStore(layout),
            lease_store=lease_store or DeliveryLeaseStore(layout),
            normalize_agent_name=normalize_mailbox_owner_name,
            terminal_event_states=_TERMINAL_EVENT_STATES,
            claimable_event_states=_CLAIMABLE_EVENT_STATES,
            mailbox_record_cls=MailboxRecord,
            delivery_lease_cls=DeliveryLease,
            reply_event_type=InboundEventType.TASK_REPLY,
            lease_state_acquired=LeaseState.ACQUIRED,
            mailbox_state_delivering=MailboxState.DELIVERING,
            mailbox_state_blocked=MailboxState.BLOCKED,
            mailbox_state_idle=MailboxState.IDLE,
            status_delivering=InboundEventStatus.DELIVERING,
            status_consumed=InboundEventStatus.CONSUMED,
            lease_ttl_seconds=lease_ttl_seconds,
        )

    def latest_events(self, agent_name: str) -> tuple[InboundEventRecord, ...]:
        return latest_events(self, agent_name)

    def pending_events(
        self,
        agent_name: str,
        *,
        event_type: InboundEventType | None = None,
    ) -> tuple[InboundEventRecord, ...]:
        return pending_events(self, agent_name, event_type=event_type)

    def head_pending_event(self, agent_name: str) -> InboundEventRecord | None:
        return head_pending_event(self, agent_name)

    def peek_next(
        self,
        agent_name: str,
        *,
        event_type: InboundEventType | None = None,
    ) -> InboundEventRecord | None:
        return peek_next(self, agent_name, event_type=event_type)

    def claim(
        self,
        agent_name: str,
        inbound_event_id: str,
        *,
        started_at: str | None = None,
    ) -> InboundEventRecord | None:
        return claim(self, agent_name, inbound_event_id, started_at=started_at)

    def claim_next(
        self,
        agent_name: str,
        *,
        event_type: InboundEventType | None = None,
        started_at: str | None = None,
    ) -> InboundEventRecord | None:
        return claim_next(self, agent_name, event_type=event_type, started_at=started_at)

    def ack_reply(
        self,
        agent_name: str,
        inbound_event_id: str,
        *,
        started_at: str | None = None,
        finished_at: str | None = None,
    ) -> InboundEventRecord | None:
        return ack_reply(
            self,
            agent_name,
            inbound_event_id,
            started_at=started_at,
            finished_at=finished_at,
        )

    def consume(
        self,
        agent_name: str,
        inbound_event_id: str,
        *,
        finished_at: str | None = None,
    ) -> InboundEventRecord | None:
        return mark_terminal(self, agent_name, inbound_event_id, status=InboundEventStatus.CONSUMED, finished_at=finished_at)

    def abandon(
        self,
        agent_name: str,
        inbound_event_id: str,
        *,
        finished_at: str | None = None,
    ) -> InboundEventRecord | None:
        return mark_terminal(self, agent_name, inbound_event_id, status=InboundEventStatus.ABANDONED, finished_at=finished_at)

    def supersede(
        self,
        agent_name: str,
        inbound_event_id: str,
        *,
        finished_at: str | None = None,
    ) -> InboundEventRecord | None:
        return mark_terminal(self, agent_name, inbound_event_id, status=InboundEventStatus.SUPERSEDED, finished_at=finished_at)

    def expired_delivery_leases(self, *, now: str | None = None) -> tuple[DeliveryLease, ...]:
        return expired_delivery_leases(self, now=now)

    def expire_lease(
        self,
        agent_name: str,
        *,
        finished_at: str | None = None,
    ) -> InboundEventRecord | None:
        return expire_lease(self, agent_name, finished_at=finished_at)

    def rebuild_mailbox_summary(self, agent_name: str, *, updated_at: str | None = None) -> MailboxRecord:
        return rebuild_mailbox_summary(self, agent_name, updated_at=updated_at)

    def project_mailbox_summary(
        self,
        agent_name: str,
        *,
        updated_at: str | None = None,
        prior=Ellipsis,
    ) -> MailboxRecord:
        return project_mailbox_summary(
            self,
            agent_name,
            updated_at=updated_at,
            prior=prior,
        )

    def rewrite_head(
        self,
        agent_name: str,
        inbound_event_id: str,
        *,
        payload_ref: str | None,
        status,
        updated_at: str | None = None,
        clear_progress: bool = False,
    ) -> InboundEventRecord | None:
        return rewrite_head(
            self,
            agent_name,
            inbound_event_id,
            payload_ref=payload_ref,
            status=status,
            updated_at=updated_at,
            clear_progress=clear_progress,
        )

    def apply_incremental_summary_update(
        self,
        agent_name: str,
        *,
        queue_delta: int = 0,
        pending_reply_delta: int = 0,
        active_inbound_event_id=Ellipsis,
        last_started_at: str | None = None,
        last_finished_at: str | None = None,
        updated_at: str | None = None,
    ) -> MailboxRecord:
        return apply_incremental_summary_update(
            self,
            agent_name,
            queue_delta=queue_delta,
            pending_reply_delta=pending_reply_delta,
            active_inbound_event_id=active_inbound_event_id,
            last_started_at=last_started_at,
            last_finished_at=last_finished_at,
            updated_at=updated_at,
        )

    def apply_transition_summary_update(
        self,
        agent_name: str,
        *,
        queue_depth: int,
        pending_reply_count: int,
        active_inbound_event_id,
        last_started_at: str | None = None,
        last_finished_at: str | None = None,
        updated_at: str | None = None,
        summary_source: str,
        summary_head=Ellipsis,
    ) -> MailboxRecord:
        return apply_transition_summary_update(
            self,
            agent_name,
            queue_depth=queue_depth,
            pending_reply_count=pending_reply_count,
            active_inbound_event_id=active_inbound_event_id,
            last_started_at=last_started_at,
            last_finished_at=last_finished_at,
            updated_at=updated_at,
            summary_source=summary_source,
            summary_head=summary_head,
        )

    def refresh_mailbox(self, agent_name: str, *, updated_at: str | None = None) -> MailboxRecord:
        return self.rebuild_mailbox_summary(agent_name, updated_at=updated_at)

    def upsert_mailbox_summary(
        self,
        agent_name: str,
        *,
        queue_delta: int = 0,
        pending_reply_delta: int = 0,
        active_inbound_event_id=Ellipsis,
        last_started_at: str | None = None,
        last_finished_at: str | None = None,
        updated_at: str | None = None,
    ) -> MailboxRecord:
        return self.apply_incremental_summary_update(
            agent_name,
            queue_delta=queue_delta,
            pending_reply_delta=pending_reply_delta,
            active_inbound_event_id=active_inbound_event_id,
            last_started_at=last_started_at,
            last_finished_at=last_finished_at,
            updated_at=updated_at,
        )


__all__ = ['MailboxKernelService']
