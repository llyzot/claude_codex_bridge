"""Runtime DTO adapters; Rust owns terminal reconciliation and persistence."""
import logging

from completion.snapshot_store import _completion_snapshot_from_record, _decision_from_record
from jobs.store import _job_record_from_record

_LOG = logging.getLogger(__name__)


def persist_native_terminal(dispatcher, current, decision, *, finished_at, revise=False, prior_snapshot=None):
    native = dispatcher._state._native
    tracker = dispatcher._completion_tracker
    shared = tracker is not None and getattr(tracker, '_native', None) is native
    view = tracker.current(current.job_id) if tracker is not None and not shared and not revise else None
    result = native.call('dispatcher.revise_reply' if revise else 'dispatcher.persist_terminal',
        current=current.to_record(), decision=decision.to_record(), finished_at=finished_at,
        profile_family=dispatcher._profile_family_for_job(current).value,
        preview_limit=getattr(dispatcher._snapshot_writer, '_preview_limit', 240),
        state_id=dispatcher._state._id, bureau_enabled=dispatcher._message_bureau is not None,
        tracker_group=tracker._group if shared else None,
        tracker_view=None if view is None else dict(job_id=view.job_id, agent_name=view.agent_name,
                                                    state=view.state.to_record(), decision=view.decision.to_record()),
        prior_snapshot=prior_snapshot.to_record() if prior_snapshot else None)
    if tracker is not None and not shared and not revise:
        try:
            tracker.finish(current.job_id)
        except Exception:
            _LOG.warning('Terminal state committed; external tracker cleanup failed', exc_info=True)
    marker = getattr(dispatcher, 'mark_project_view_dirty', None)
    if callable(marker):
        try:
            marker()
        except Exception:
            _LOG.warning('Terminal state committed; project-view invalidation failed', exc_info=True)
    return (_job_record_from_record(result['job']), _decision_from_record(result['decision']),
            _completion_snapshot_from_record(result['prior']) if result['prior'] else None)
