"""Wire adapters for Rust provider authority and acceptance policies."""
from completion.snapshot_store import _decision_from_record


def validate_native_completion(native, submission, decision, job):
    payload=None
    if submission is not None:
        kind=getattr(submission,'source_kind',None)
        state=dict(getattr(submission,'runtime_state',{}) or {})
        payload={'provider':getattr(submission,'provider',''),
                 'source_kind':getattr(kind,'value',kind),
                 'runtime_state':{key:state[key] for key in ('mode','delivery_state','anchor_seen','no_wrap',
                                                           'reply_delivery_complete_on_dispatch') if key in state}}
    identity=None if job is None else {
        'request':{'message_type':getattr(job.request,'message_type','')},
        'provider_options':dict(getattr(job,'provider_options',{}) or {}),
    }
    result=native.call('completion.gate',submission=payload,decision=None if decision is None else decision.to_record(),job=identity)
    return None if result is None else _decision_from_record(result)
