"""Provider/config adapters around the Rust callback transaction."""
from __future__ import annotations

from message_bureau import CallbackEdgeRecord
from native_kernel import KernelUnavailable
from .runtime_state import sync_runtime
from .records import append_event


def submit_native_callback(dispatcher, edge, *, child_job, child_reply_id, decision, finished_at):
    from .callbacks import _continuation_request, fail_callback_edge, get_job
    native = dispatcher._state._native
    prepared = native.call('callback.prepare',edge=edge.to_record(),child_job=child_job.to_record(),
                           child_reply_id=child_reply_id,finished_at=finished_at)
    latest = CallbackEdgeRecord.from_record(prepared['edge'])
    if prepared['action'] != 'submit':
        return latest
    try:
        request = _continuation_request(dispatcher,edge=latest,child_job=child_job,decision=decision)
        dispatcher._registry.spec_for(request.to_agent)
        dispatcher._validate_targets_available((request.to_agent,))
        spec = dispatcher._registry.spec_for(request.to_agent)
        parent = get_job(dispatcher,latest.parent_job_id)
        runtime = dispatcher._registry.get(request.to_agent)
        result = native.call('callback.enqueue',edge_id=latest.edge_id,request=request.to_record(),
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
        if callable(marker):
            marker()
        sync_runtime(dispatcher,request.to_agent)
        append_event(dispatcher,child_job,'chain_continuation_submitted',{
            'edge_id':final.edge_id,'continuation_job_id':final.continuation_job_id,
            'continuation_message_id':final.continuation_message_id,'callback_target_agent':final.callback_target_agent,
        },timestamp=finished_at)
    return final
