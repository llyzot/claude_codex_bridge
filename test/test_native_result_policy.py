"""Differential result-policy decisions using the existing Python oracle."""
from dataclasses import replace
import pytest

from test_native_finalization import setup, decision, BIN, NOW
from test_native_admission import activate,canonical
from completion.models import CompletionStatus
from ccbd.api_models import JobStatus
from message_bureau import AttemptStore,MessageStore
from native_kernel import close_kernels
from ccbd.services.dispatcher_runtime.finalization_runtime.message_bureau_retry import reply_decision_without_automatic_retry
from ccbd.services.dispatcher_runtime.finalization_runtime.empty_result_notice import is_empty_result_outcome,with_empty_result_notice,with_abnormal_result_guidance
from ccbd.services.dispatcher_runtime.finalization_retry import automatic_retry_plan

@pytest.fixture(autouse=True)
def environment(monkeypatch):
    monkeypatch.setenv('CCB_KERNEL_BIN',str(BIN))
    close_kernels()
    yield
    close_kernels()

@pytest.mark.parametrize('status,reason,reply,diagnostics,resume,runtime_retry,max_attempts',[
    ('failed','api_error','error',{},False,True,3),
    ('failed','api_error','error',{},False,True,1),
    ('failed','pane_dead','error',{},True,False,3),
    ('failed','pane_dead','error',{},True,True,3),
    ('failed','api_error','error',{'error_code':'Forbidden','error_message':'invalid api key'},True,True,3),
    ('failed','api_error','error',{'error_code':'InvalidApiKey'},True,True,3),
    ('failed','api_error','error',{'error_code':'InsufficientQuota'},True,True,3),
    ('incomplete','timeout','',{},False,True,3),
    ('incomplete','pi_empty_reply','',{'delivery_retryable':True},True,True,3),
    ('incomplete','opaque_error','',{'delivery_retryable':True},False,True,3),
    ('incomplete','opaque_error','',{},False,True,3),
    ('completed','task_complete','',{},False,True,3),
    ('completed','task_complete','done',{},False,True,3),
    ('cancelled','cancel_info','',{},False,True,3),
])
def test_result_policy_matches_python_oracle(tmp_path,monkeypatch,status,reason,reply,diagnostics,resume,runtime_retry,max_attempts):
    values=[]
    for mode in ['python','rust']:
        d=setup(tmp_path/mode,mode,monkeypatch)
        current=activate(d)
        result=decision(reply,CompletionStatus(status),reason,diagnostics)
        terminal=replace(current,status=JobStatus(status),terminal_decision=result.to_record())
        attempt=AttemptStore(d._layout).get_latest_by_job_id(current.job_id)
        store=MessageStore(d._layout);message=store.get_latest(attempt.message_id)
        policy=dict(message.retry_policy,runtime_when_resume_supported=False,max_attempts=max_attempts,
                    retry_runtime_when_resume_supported=runtime_retry)
        store.append(replace(message,retry_policy=policy))
        d._provider_catalog=type('Catalog',(),{'get':lambda self,name:type('Manifest',(),{'supports_resume':resume})()})()
        if mode=='python':
            retry=automatic_retry_plan(d,current,result)
            if retry is not None:values.append(('retry',result.to_record()))
            else:
                rendered=reply_decision_without_automatic_retry(d,current,terminal,result,finished_at=NOW)
                if is_empty_result_outcome(rendered):rendered=with_empty_result_notice(rendered,terminal,finished_at=NOW)
                else:rendered=with_abnormal_result_guidance(rendered,terminal)
                values.append(('reply',rendered.to_record()))
        else:
            plan=d._state._native.call('result.plan',job=terminal.to_record(),decision=result.to_record(),supports_resume=resume)
            values.append((plan['action'],plan['decision']))
        close_kernels()
    assert canonical(list(values[0]))==canonical(list(values[1]))
