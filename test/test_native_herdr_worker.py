"""Native actor + offline Herdr protocol + externally owned provider lifetimes.

No Herdr server, real provider or account is started by this fixture.
"""
import json
import os
import time

import pytest
from test_native_daemon import Server, await_watch
from test_native_herdr_agents import prepared, run, change
from test_native_herdr_namespace import setup, fault, mutate_count
from test_native_pi_rpc import pi_spec
from test_native_role_memory import install as install_role

pytestmark=pytest.mark.skipif(os.name!='nt',reason='Herdr process identity requires Windows')

def wait_for(fn, timeout=12):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        value=fn()
        if value:return value
        time.sleep(.05)
    pytest.fail('expected fixture state not observed')

@pytest.fixture
def runtime(prepared):
    project, backend, request, processes=prepared
    mount=run(request)['mount']
    history=project/'history';history.mkdir()
    path=history/'rollout.jsonl'
    path.write_text(json.dumps({'type':'session_meta','payload':{'id':'test-session','cwd':str(project),'source':'cli'}})+'\n',encoding='utf-8')
    name=mount['agents']['worker']['herdr_name']
    change(backend,lambda s:s['agents'][name].update(agent_session={'source':'fixture','agent':'codex','kind':'id','value':'test-session'}))
    worker=dict(provider='codex',driver_kind='herdr_tui',command=None,workspace_path=str(project),timeout_s=20,
                herdr_history=dict(root=str(history),path=str(path),cwd=str(project),session_id='test-session'))
    agents={'lead':pi_spec('lead'),'worker':worker}
    agents['lead']['env']={'CCB_TEST_PROJECT_ID':request['config']['project_id']}
    extra={'project_id':request['config']['project_id'],'project_root':str(project),'herdr':request}
    instances=[]
    def start():
        instance=Server(project/'.ccb/ccbd/daemon',agents,config_extra=extra)
        instances.append(instance);return instance
    def submit(instance,target='worker',actor='user',body='task'):
        return instance.rpc('submit',dict(project_id=extra['project_id'],to_agent=target,from_actor=actor,body=body))
    def prompts():
        p=backend/'prompts.jsonl'
        return [json.loads(line) for line in p.read_text(encoding='utf-8').splitlines()] if p.exists() else []
    def append(*rows):
        with path.open('a',encoding='utf-8') as out:
            for row in rows:out.write(json.dumps(row)+'\n')
    def event(kind,**data):return {'type':'event_msg','payload':{'type':kind,**data}}
    def finish(prompt,reply='terminal worker result',turn='turn-current',idle=True,error=None):
        append(event('task_started',turn_id=turn),event('user_message',message=prompt['body']),
               event('task_complete',turn_id=turn,last_agent_message=reply,error=error))
        if idle:change(backend,lambda s:s['agents'][name].update(agent_status='idle',interactive_ready=True))
    yield dict(project=project,backend=backend,request=request,processes=processes,path=path,worker=worker,
               start=start,submit=submit,prompts=prompts,append=append,event=event,finish=finish,name=name)
    for instance in instances:
        if instance.proc.poll() is None:instance.close()
        elif not getattr(instance,'expected_exit',False):assert instance.proc.returncode==0,instance.proc.stderr.read()

def job(server,receipt):
    return server.rpc('records.latest',{'stream':'jobs','identity':'job_id','agent_name':'worker'},version=1)['result'][-1]


def test_ping_external_herdr_does_not_upgrade_historical_readiness_to_health(runtime):
    r=runtime;s=r['start']()
    first=s.rpc('ping',{'target':'worker'})
    assert first['health']=='unknown' and first['diagnostics']['external_last_observed_at'] is None
    assert r['prompts']()==[]
    receipt=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    r['finish'](prompt)
    await_watch(s,receipt['job_id'],lambda row:row['terminal'])
    row=wait_for(lambda:(v if (v:=s.rpc('ping',{'target':'worker'}))['diagnostics']['protocol_idle_last_observed'] is True else None))
    assert row['health']=='unknown' and row['diagnostics']['external_last_observed_at']
    assert row['diagnostics']['reason']=='external_provider_not_freshly_probed'
    assert mutate_count(r['backend'],['agent','prompt'])==1
    assert all(p.poll() is None for p in r['processes'])

def test_pi_lead_yields_then_receives_herdr_result_and_continues(runtime):
    r=runtime;s=r['start']();receipt=r['submit'](s,'lead')
    prompt=wait_for(lambda:r['prompts']())[-1]
    begin=time.monotonic();assert s.rpc('ping')['runtime']=='rust';assert time.monotonic()-begin<.7
    before=json.loads((s.root/'providers/pi/lead/session.json').read_text())
    assert not s.rpc('watch',{'target':receipt['job_id']})['terminal']
    r['finish'](prompt)
    row=await_watch(s,receipt['job_id'],lambda row:row['visible_reply_source']=='message_bureau_reply',timeout=15)
    assert row['reply']=='persistent Pi reviewed result'
    after=json.loads((s.root/'providers/pi/lead/session.json').read_text())
    assert before['pid']==after['pid'] and before['state']['sessionId']==after['state']['sessionId']
    assert after['state']['messageCount']==4
    assert mutate_count(r['backend'],['agent','prompt'])==1
    assert all(p.poll() is None for p in r['processes'])

def test_completion_requires_anchor_exact_turn_and_terminal_not_idle(runtime):
    r=runtime;s=r['start']();receipt=r['submit'](s)
    prompt=wait_for(lambda:r['prompts']())[-1];e=r['event']
    change(r['backend'],lambda state:state['agents'][r['name']].update(agent_status='idle',interactive_ready=True))
    r['append'](e('task_started',turn_id='current'),e('user_message',message='quoted '+prompt['body']),
                e('task_complete',turn_id='current',last_agent_message='wrong anchor'))
    time.sleep(.4);assert not s.rpc('watch',{'target':receipt['job_id']})['terminal']
    r['append'](e('task_started',turn_id='actual'),e('user_message',message=prompt['body']),
                e('agent_message',phase='commentary',message='not final'),
                e('sub_agent_activity',phase='final_answer',message='subagent answer'),
                e('task_complete',turn_id='foreign',last_agent_message='foreign answer'))
    time.sleep(.4);assert not s.rpc('watch',{'target':receipt['job_id']})['terminal']
    r['append'](e('task_complete',turn_id='actual',last_agent_message='correct'))
    assert await_watch(s,receipt['job_id'],lambda row:row['terminal'])['reply']=='correct'

@pytest.mark.parametrize('mode',['after-error','hang-after'])
def test_lost_prompt_ack_does_not_replay_or_block_actor(runtime,mode):
    r=runtime;fault(r['backend'],['agent','prompt'],mode)
    s=r['start']();receipt=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    begin=time.monotonic();s.rpc('ping');assert time.monotonic()-begin<.7
    r['finish'](prompt)
    assert await_watch(s,receipt['job_id'],lambda row:row['terminal'])['reply']=='terminal worker result'
    assert mutate_count(r['backend'],['agent','prompt'])==1

def test_daemon_crash_resumes_history_only_external_provider_survives(runtime):
    r=runtime;s=r['start']();receipt=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    durable=job(s,receipt);assert durable['provider_options']['herdr_dispatch']['baseline']['offset']>0
    s.expected_exit=True;s.proc.kill();s.proc.wait(timeout=5)
    assert all(p.poll() is None for p in r['processes'])
    r['finish'](prompt)
    resumed=r['start']()
    assert await_watch(resumed,receipt['job_id'],lambda row:row['terminal'])['reply']=='terminal worker result'
    assert mutate_count(r['backend'],['agent','prompt'])==1

def test_fifo_waits_for_live_readiness_even_after_known_terminal(runtime):
    r=runtime;s=r['start']();first=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    second=r['submit'](s,body='second');r['finish'](prompt,idle=False)
    assert await_watch(s,first['job_id'],lambda row:row['terminal'])['status']=='completed'
    time.sleep(.4);assert len(r['prompts']())==1
    change(r['backend'],lambda state:state['agents'][r['name']].update(agent_status='idle',interactive_ready=True))
    prompt=wait_for(lambda:r['prompts']() if len(r['prompts']())==2 else None)[-1]
    r['finish'](prompt,turn='turn-next')
    assert await_watch(s,second['job_id'],lambda row:row['terminal'])['status']=='completed'

def test_rotated_history_holds_fifo_without_terminating_provider(runtime):
    r=runtime;s=r['start']();first=r['submit'](s);wait_for(lambda:r['prompts']())
    second=r['submit'](s,body='second')
    wait_for(lambda:s.rpc('provider-status',{'agent_name':'worker'})['owned']['pid'])
    r['path'].rename(r['path'].with_suffix('.old'))
    r['path'].write_text('{}\n',encoding='utf-8')
    wait_for(lambda:(s.root/'providers/codex/worker/fault.json').exists())
    assert not s.rpc('watch',{'target':first['job_id']})['terminal']
    assert not s.rpc('watch',{'target':second['job_id']})['terminal']
    assert len(r['prompts']())==1 and all(p.poll() is None for p in r['processes'])
    with pytest.raises(AssertionError):s.rpc('work',{'agent_name':'worker'})
    with pytest.raises(AssertionError):s.rpc('complete',{'agent_name':'worker','job_id':first['job_id'],'decision':{}})

def test_timeout_keeps_unknown_task_then_explicit_reobserve_finds_result_without_send(runtime):
    r=runtime;r['worker']['timeout_s']=.3;s=r['start']();receipt=r['submit'](s)
    prompt=wait_for(lambda:r['prompts']())[-1]
    wait_for(lambda:(s.root/'providers/codex/worker/fault.json').exists())
    assert not s.rpc('watch',{'target':receipt['job_id']})['terminal']
    current=job(s,receipt);r['finish'](prompt)
    s.rpc('herdr-reobserve',{'agent_name':'worker','job_id':receipt['job_id'],'execution_instance_id':current['provider_options']['native_execution_instance']})
    assert await_watch(s,receipt['job_id'],lambda row:row['terminal'])['reply']=='terminal worker result'
    assert len(r['prompts']())==1

@pytest.mark.parametrize('kind',['session','header','prompt-size'])
def test_preflight_failure_is_terminal_known_unsent_and_does_not_leave_unknown_work(runtime,kind):
    r=runtime
    if kind=='session':
        change(r['backend'],lambda state:state['agents'][r['name']]['agent_session'].update(value='foreign-session'))
    elif kind=='header':
        r['path'].write_text(json.dumps({'type':'session_meta','payload':{'id':'foreign','cwd':str(r['project'])}})+'\n',encoding='utf-8')
    else:
        role=install_role(r['project']);(role/'memory.md').write_text('x'*9000,encoding='utf-8')
        r['worker']['role']='agentroles.reader'
    s=r['start']();receipt=r['submit'](s)
    row=await_watch(s,receipt['job_id'],lambda row:row['terminal'])
    assert row['status']=='failed' and row['completion_reason']=='herdr_preflight_failed'
    current=job(s,receipt)
    assert current['terminal_decision']['diagnostics']['provider_prompt_sent'] is False
    assert not current['provider_options'].get('herdr_dispatch') and not r['prompts']()
    assert all(p.poll() is None for p in r['processes'])

def test_unknown_send_without_log_is_held_across_restart_and_never_resent(runtime):
    r=runtime;fault(r['backend'],['agent','prompt'],'before-error')
    s=r['start']();receipt=r['submit'](s)
    wait_for(lambda:mutate_count(r['backend'],['agent','prompt'])==1)
    assert not r['prompts']();s.close()
    resumed=r['start']();time.sleep(.5)
    assert not resumed.rpc('watch',{'target':receipt['job_id']})['terminal']
    assert mutate_count(r['backend'],['agent','prompt'])==1 and not r['prompts']()
    with pytest.raises(AssertionError):resumed.rpc('cancel',{'job_id':receipt['job_id']})
    assert all(p.poll() is None for p in r['processes'])

def test_provider_task_error_cannot_become_success_or_deliver_final_looking_text(runtime):
    r=runtime;s=r['start']();receipt=r['submit'](s)
    prompt=wait_for(lambda:r['prompts']())[-1]
    r['finish'](prompt,reply='SUCCESS should not escape',error={'message':'provider failed'})
    row=await_watch(s,receipt['job_id'],lambda row:row['terminal'])
    assert row['status']=='failed' and 'SUCCESS' not in row['reply']

def test_live_role_memory_changes_reach_same_terminal_worker_without_profile_copy(runtime):
    r=runtime;role=install_role(r['project']);r['worker']['role']='agentroles.reader'
    s=r['start']();first=r['submit'](s);prompt=wait_for(lambda:r['prompts']())[-1]
    assert 'ROLE_MEMORY_OLD' in prompt['body']
    r['finish'](prompt);await_watch(s,first['job_id'],lambda row:row['terminal'])
    (role/'memory.md').write_text('ROLE_MEMORY_NEW',encoding='utf-8')
    second=r['submit'](s);prompt=wait_for(lambda:r['prompts']() if len(r['prompts']())==2 else None)[-1]
    assert 'ROLE_MEMORY_NEW' in prompt['body'] and 'ROLE_MEMORY_OLD' not in prompt['body']
    r['finish'](prompt,turn='second');await_watch(s,second['job_id'],lambda row:row['terminal'])
    assert mutate_count(r['backend'],['agent','start'])==2 # Original fixture mount only.

def test_restart_between_completion_and_readiness_keeps_next_request_waiting(runtime):
    r=runtime;s=r['start']();first=r['submit'](s)
    prompt=wait_for(lambda:r['prompts']())[-1];second=r['submit'](s,body='next task')
    r['finish'](prompt,idle=False)
    await_watch(s,first['job_id'],lambda row:row['terminal'])
    s.expected_exit=True;s.proc.kill();s.proc.wait(timeout=5)
    resumed=r['start']();time.sleep(.5)
    assert not resumed.rpc('watch',{'target':second['job_id']})['terminal'] and len(r['prompts']())==1
    change(r['backend'],lambda state:state['agents'][r['name']].update(agent_status='idle',interactive_ready=True))
    prompt=wait_for(lambda:r['prompts']() if len(r['prompts']())==2 else None)[-1]
    r['finish'](prompt,turn='after-restart')
    assert await_watch(resumed,second['job_id'],lambda row:row['terminal'])['status']=='completed'
