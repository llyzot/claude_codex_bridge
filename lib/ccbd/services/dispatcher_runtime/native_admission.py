"""Config/runtime adapters; Rust owns admission policy and durable mutation."""
import logging

from ccbd.api_models import AcceptedJobReceipt, JobStatus, SubmitReceipt, TargetKind

_LOG = logging.getLogger(__name__)


def callback_context(dispatcher, request):
    from .callbacks import _max_callback_depth
    return dict(request=request.to_record(), state_id=dispatcher._state._id,
                agent_names=list(dispatcher._config.agents),
                bureau_enabled=dispatcher._message_bureau is not None,
                max_callback_depth=_max_callback_depth(dispatcher))


def submit_native_plan(dispatcher, plan, *, accepted_at):
    from .runtime_state import sync_runtime
    value = dict(project_id=plan.project_id, from_actor=plan.from_actor,
                 request=plan.request.to_record(), task_id=plan.task_id,
                 submission_id=plan.submission_id, target_scope=plan.target_scope,
                 origin_message_id=plan.origin_message_id,
                 drafts=[dict(agent_name=draft.agent_name, provider=draft.provider,
                              request=draft.request.to_record(), target_kind=draft.target_kind.value,
                              target_name=draft.target_name, provider_instance=draft.provider_instance,
                              provider_options=draft.provider_options, workspace_path=draft.workspace_path)
                         for draft in plan.drafts])
    context = callback_context(dispatcher, plan.request)
    context.pop('request')
    try:
        result = dispatcher._state._native.call('dispatcher.submit', plan=value,
                                                accepted_at=accepted_at, **context)
    except ValueError as error:
        raise dispatcher._dispatch_error(str(error)) from error
    rows = result['receipt'].get('jobs') or [result['receipt']]
    receipt = SubmitReceipt(accepted_at=accepted_at, submission_id=plan.submission_id,
                            jobs=tuple(AcceptedJobReceipt(job_id=row['job_id'], agent_name=row['agent_name'],
                                target_kind=TargetKind(row['target_kind']), target_name=row['target_name'],
                                provider_instance=row['provider_instance'], status=JobStatus(row['status']),
                                accepted_at=row['accepted_at']) for row in rows))
    marker = getattr(dispatcher, 'mark_project_view_dirty', None)
    if callable(marker):
        try:
            marker()
        except Exception:
            _LOG.warning('Accepted admission; project-view invalidation failed', exc_info=True)
    # Runtime health/projection is derived after the authoritative transaction.
    # Never replay admission if this projection fails or its ack is lost.
    for draft in plan.drafts:
        try:
            sync_runtime(dispatcher, draft.agent_name)
        except Exception:
            _LOG.warning('Accepted admission; runtime projection failed for %s', draft.agent_name, exc_info=True)
    return receipt, result['message_id']
