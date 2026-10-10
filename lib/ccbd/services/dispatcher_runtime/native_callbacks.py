"""Provider/config adapters around the Rust callback transaction."""
from __future__ import annotations

import logging

from message_bureau import CallbackEdgeRecord
from native_kernel import KernelUnavailable
from .runtime_state import sync_runtime

_LOG = logging.getLogger(__name__)


def repair_native_callbacks(dispatcher):
    from completion.snapshot_store import _decision_from_record
    from jobs.store import _job_record_from_record
    result = dispatcher._state._native.call('callback.repair',state_id=dispatcher._state._id,
                                             agent_names=list(dispatcher._config.agents))
    repaired = [CallbackEdgeRecord.from_record(row) for row in result['repaired']]
    for row in result['actions']:
        child = _job_record_from_record(row['child_job'])
        decision = _decision_from_record(row['decision'])
        if row['kind'] == 'finalize_result':
            from .finalization_runtime.message_bureau import record_message_bureau_completion, record_message_bureau_cancellation
            if decision.status.value == 'cancelled' and decision.diagnostics.get('result_reply_enabled') is True:
                record_message_bureau_cancellation(dispatcher, child, decision, finished_at=row['finished_at'],
                                                  record_reply=True, attempt_recorded=True)
            else:
                record_message_bureau_completion(dispatcher, child, child, decision, finished_at=row['finished_at'],
                    prior_snapshot=dispatcher._snapshot_writer.load(child.job_id))
            repaired.append(dispatcher._message_bureau.callback_edge(row['edge']['edge_id']))
        else:
            repaired.append(submit_native_callback(dispatcher, CallbackEdgeRecord.from_record(row['edge']),
                child_job=child, child_reply_id=row['child_reply_id'], decision=decision, finished_at=row['finished_at']))
    return tuple(repaired)


def submit_native_callback(dispatcher, edge, *, child_job, child_reply_id, decision, finished_at):
    from .callbacks import fail_callback_edge, get_job
    native = dispatcher._state._native
    prepared = native.call('callback.prepare',edge=edge.to_record(),child_job=child_job.to_record(),
                           child_reply_id=child_reply_id,finished_at=finished_at)
    latest = CallbackEdgeRecord.from_record(prepared['edge'])
    if prepared['action'] != 'submit':
        return latest
    try:
        target = latest.callback_target_agent
        dispatcher._validate_targets_available((target,))
        spec = dispatcher._registry.spec_for(target)
        parent = get_job(dispatcher,latest.parent_job_id)
        runtime = dispatcher._registry.get(target)
        result = native.call('callback.enqueue',edge_id=latest.edge_id,
                             child_job=child_job.to_record(),decision=decision.to_record(),
                             provider=spec.provider,workspace_path=getattr(parent,'workspace_path',None) or getattr(runtime,'workspace_path',None),
                             state_id=dispatcher._state._id,finished_at=finished_at)
    except KernelUnavailable:
        raise  # A lost acknowledgement must be resolved from durable state.
    except Exception as error:
        return fail_callback_edge(dispatcher,latest,reason='chain_continuation_submit_failed',
                                  detail=str(error),updated_at=finished_at)
    final = CallbackEdgeRecord.from_record(result['edge'])
    if result['created']:
        marker = getattr(dispatcher,'mark_project_view_dirty',None)
        try:
            if callable(marker):
                marker()
            sync_runtime(dispatcher,final.callback_target_agent)
        except Exception:
            _LOG.warning('Callback committed; runtime/project-view projection failed', exc_info=True)
    return final
