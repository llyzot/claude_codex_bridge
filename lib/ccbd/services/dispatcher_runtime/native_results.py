"""Provider availability adapters around Rust result policy and admission."""
import logging
from native_kernel import KernelUnavailable
from completion.snapshot_store import _decision_from_record
from message_bureau import CallbackEdgeRecord
from .records import append_event
from .runtime_state import sync_runtime

_LOG = logging.getLogger(__name__)

def retry_native(dispatcher, target):
    client=dispatcher._state._native
    source=client.call('retry.source',target=target)
    agent=source['agent_name']
    spec=dispatcher._registry.spec_for(agent)
    if bool(getattr(spec,'dispatch_disabled',False)):
        raise dispatcher._dispatch_rejected_error(f'agent {agent} is dispatch-disabled')
    dispatcher._validate_targets_available((agent,))
    try:
        result=client.call('dispatcher.retry',target=target,state_id=dispatcher._state._id,
                           accepted_at=dispatcher._clock(),dispatch_disabled=False)
    except ValueError as error:
        raise dispatcher._dispatch_error(str(error)) from error
    try:
        marker=getattr(dispatcher,'mark_project_view_dirty',None)
        if callable(marker):marker()
        sync_runtime(dispatcher,agent)
    except Exception:
        _LOG.warning('Retry committed; runtime/project-view projection failed',exc_info=True)
    return result

def complete_native_result(dispatcher,current,terminal,decision,*,finished_at,prior_snapshot):
    client=dispatcher._state._native
    try:
        resume=bool(dispatcher._provider_catalog.get(current.provider).supports_resume)
    except Exception:
        resume=False
    args=dict(job=terminal.to_record(),decision=decision.to_record(),supports_resume=resume)
    plan=client.call('result.plan',**args)
    if plan['action'] in {'delegated','delivery','silenced'}:
        return terminal,decision,False
    if plan['action']=='retry':
        try:
            receipt=dispatcher.retry(current.job_id)
        except KernelUnavailable:
            raise # Unknown outcomes must be resolved from the durable attempt lineage.
        except Exception as error:
            plan=client.call('result.plan',**args,scheduling_error=str(error))
        else:
            context=plan['context']
            append_event(dispatcher,terminal,'job_retry_scheduled',dict(message_id=receipt.get('message_id'),
                original_attempt_id=receipt.get('original_attempt_id'),attempt_id=receipt.get('attempt_id'),
                job_id=receipt.get('job_id'),agent_name=receipt.get('agent_name'),
                attempt_number=context['attempt_number']+1,max_attempts=context['max_attempts'],reason=context['reason']),
                timestamp=str(receipt.get('accepted_at') or finished_at))
            return terminal,decision,True
    result=_decision_from_record(plan['decision'])
    for event in plan['events']:
        append_event(dispatcher,terminal,event['type'],event['payload'],timestamp=finished_at)
    if result!=decision:
        from .native_finalization import persist_native_terminal
        terminal,result,_=persist_native_terminal(dispatcher,current,result,finished_at=finished_at,
                                                  revise=True,prior_snapshot=prior_snapshot)
    record=client.call('result.record',job=terminal.to_record(),decision=result.to_record(),finished_at=finished_at,
                       deliver_to_caller=plan['deliver_to_caller'],known_mailboxes=list(dispatcher._config.agents))
    continuation=record['continuation']
    if continuation and continuation['action']=='submit':
        from .native_callbacks import submit_native_callback
        submit_native_callback(dispatcher,CallbackEdgeRecord.from_record(continuation['edge']),child_job=terminal,
                               child_reply_id=record['reply_id'],decision=result,finished_at=finished_at)
    return terminal,result,False
