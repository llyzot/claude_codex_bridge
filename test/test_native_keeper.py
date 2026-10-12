"""Native supervision of disposable daemons; no existing Workspace controls."""
import json, os, signal, time
import pytest
from test_native_cli_security import cli
from test_native_daemon import spec

pytestmark=pytest.mark.skipif(os.name!='nt',reason='Windows process-lifetime supervision')

def wait_for(fn,timeout=12):
    end=time.monotonic()+timeout
    while time.monotonic()<end:
        value=fn()
        if value:return value
        time.sleep(.04)
    pytest.fail('expected keeper transition not observed')

def start(tmp_path,agents=None):
    root=tmp_path/'state';config=tmp_path/'config.json'
    config.write_text(json.dumps(dict(schema_version=2,project_id='keeper-test',agents=agents or {'worker':spec('worker')})),encoding='utf-8')
    result=cli('--state-root',root,'start','--config',config)
    assert result.returncode==0,result.stderr
    return root,config,json.loads(result.stdout)

def status(root):
    result=cli('--state-root',root,'keeper-status');assert result.returncode==0,result.stderr
    return json.loads(result.stdout)

def ping(root):
    result=cli('--state-root',root,'ping')
    return json.loads(result.stdout) if result.returncode==0 else None

def stop(root):
    result=cli('--state-root',root,'stop');assert result.returncode==0,result.stderr
    wait_for(lambda:status(root)['keeper']['phase']=='stopped' and not status(root)['alive'])

def test_default_native_start_has_one_keeper_and_recovers_daemon_death(tmp_path):
    root,config,first=start(tmp_path)
    try:
        keeper=status(root);assert keeper['alive'] and keeper['keeper']['phase']=='watching'
        again=cli('--state-root',root,'start','--config',config);assert again.returncode==0,again.stderr
        assert json.loads(again.stdout)['pid']==first['pid']
        assert status(root)['keeper']['instance_id']==keeper['keeper']['instance_id']
        config.unlink()
        os.kill(first['pid'],signal.SIGTERM)
        restored=wait_for(lambda:(value if (value:=ping(root)) and value['pid']!=first['pid'] else None))
        assert status(root)['keeper']['instance_id']==keeper['keeper']['instance_id']
        task=cli('--state-root',root,'ask','worker','after crash');assert task.returncode==0,task.stderr
        result=cli('--state-root',root,'watch',json.loads(task.stdout)['job_id'],'--wait','--timeout-ms','5000')
        assert json.loads(result.stdout)['reply']=='worker result'
        assert restored['project_id']=='keeper-test'
    finally:stop(root)

def test_explicit_stop_is_durable_and_does_not_resurrect_daemon(tmp_path):
    root,config,first=start(tmp_path)
    stop(root)
    before=(root/'kernel/native-start.json').read_bytes()
    time.sleep(.8)
    assert (root/'kernel/native-start.json').read_bytes()==before and ping(root) is None
    assert json.loads((root/'lifecycle.json').read_text(encoding='utf-8'))['desired_state']=='stopped'

def test_stop_during_backoff_prevents_the_next_launch(tmp_path):
    root,config,first=start(tmp_path)
    before=(root/'kernel/native-start.json').read_bytes()
    os.kill(first['pid'],signal.SIGTERM)
    wait_for(lambda:status(root)['keeper']['phase']=='backoff')
    stop(root)
    assert (root/'kernel/native-start.json').read_bytes()==before and ping(root) is None

def test_keeper_loss_does_not_kill_daemon_and_explicit_start_replaces_only_keeper(tmp_path):
    root,config,first=start(tmp_path)
    try:
        old=status(root)['keeper'];os.kill(old['process']['pid'],signal.SIGTERM)
        wait_for(lambda:not status(root)['alive'])
        assert ping(root)['pid']==first['pid']
        result=cli('--state-root',root,'start','--config',config);assert result.returncode==0,result.stderr
        assert json.loads(result.stdout)['pid']==first['pid']
        assert status(root)['keeper']['instance_id']!=old['instance_id']
    finally:stop(root)

def test_reload_then_crash_uses_committed_configuration_without_source_file(tmp_path):
    from test_native_reload import connect, applied
    root,path,first=start(tmp_path)
    try:
        desired=json.loads(path.read_text());desired['agents']['peer']=spec('worker')
        applied(connect(root),desired);fingerprint=ping(root)['config_fingerprint'];path.unlink()
        os.kill(first['pid'],signal.SIGTERM)
        restored=wait_for(lambda:(r if (r:=ping(root)) and r['pid']!=first['pid'] else None))
        assert restored['config_fingerprint']==fingerprint
        assert 'peer' in [a['agent_name'] for a in json.loads(cli('--state-root',root,'agents').stdout)['agents']]
    finally:stop(root)

@pytest.mark.parametrize('missing',[False,True])
def test_corrupt_committed_configuration_is_not_replaced_by_bootstrap_copy(tmp_path,missing):
    root,path,first=start(tmp_path)
    record=root/'configuration.json';data=json.loads(record.read_text(encoding='utf-8'))
    data['restart_checkpoint']['ciphertext']='00'*128;record.write_text(json.dumps(data),encoding='utf-8')
    if missing:record.unlink()
    before=(root/'kernel/native-start.json').read_bytes()
    os.kill(first['pid'],signal.SIGTERM)
    wait_for(lambda:status(root)['keeper']['phase']=='failed' and not status(root)['alive'])
    assert ping(root) is None and (root/'kernel/native-start.json').read_bytes()==before

def test_repeated_crashes_reach_a_bounded_terminal_failure(tmp_path):
    root,path,first=start(tmp_path);current=first['pid']
    for iteration in range(4):
        os.kill(current,signal.SIGTERM)
        if iteration<3:
            restored=wait_for(lambda:(r if (r:=ping(root)) and r['pid']!=current else None));current=restored['pid']
    wait_for(lambda:status(root)['keeper']['phase']=='failed' and not status(root)['alive'])
    row=status(root)['keeper']
    assert row['reason']=='restart_limit_reached' and row['restart_count']==3
    assert ping(root) is None

def test_crashed_worker_result_automatically_resumes_saved_pi_lead(tmp_path):
    from test_native_pi_rpc import pi_spec
    from test_native_provider_faults import provider_spec
    from test_native_reload import connect
    lead=pi_spec('persist-lead');lead['command'].append(str(tmp_path/'lead-session.json'))
    lead['env']={'CCB_TEST_PROJECT_ID':'keeper-test'}
    root,path,first=start(tmp_path,{'lead':lead,'worker':provider_spec('codex','hold',tmp_path)})
    try:
        server=connect(root);parent=server.rpc('submit',dict(project_id='keeper-test',to_agent='lead',from_actor='user',body='delegate then recover'))['job_id']
        wait_for(lambda:(tmp_path/'observed-owner.json').exists())
        wait_for(lambda:server.rpc('queue',{'target':'lead'})['queues'][0]['active_job_id'] is None)
        before=json.loads((root/'providers/pi/lead/session.json').read_text(encoding='utf-8'))
        os.kill(first['pid'],signal.SIGTERM)
        wait_for(lambda:(r if (r:=ping(root)) and r['pid']!=first['pid'] else None))
        server=connect(root)
        reply=wait_for(lambda:(r if (r:=server.rpc('watch',{'target':parent}))['visible_reply_source']=='message_bureau_reply' else None))
        assert reply['reply']=='persistent Pi reviewed result'
        after=json.loads((root/'providers/pi/lead/session.json').read_text(encoding='utf-8'))
        assert after['state']['sessionId']==before['state']['sessionId']
        server.token.generation=server.endpoint['generation']
        rows=server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result']
        assert len(rows)==1 and rows[0]['terminal_decision']['reason']=='native_owner_stopped'
        assert rows[0]['terminal_decision']['diagnostics']['automatic_replay'] is False
    finally:stop(root)

def test_direct_daemon_shutdown_is_durable_before_receipt_and_keeper_exit(tmp_path):
    from test_native_claude_context import provider
    root,path,first=start(tmp_path,{'worker':provider(tmp_path,'eof-hang')})
    wait_for(lambda:json.loads(cli('--state-root',root,'ping','worker').stdout)['diagnostics']['protocol_observed'])
    result=cli('--state-root',root,'request','shutdown','{}')
    assert result.returncode==0,result.stderr
    lifecycle=json.loads((root/'lifecycle.json').read_text(encoding='utf-8'))
    assert lifecycle['desired_state']=='stopped' and lifecycle['phase']=='stopping'
    os.kill(first['pid'],signal.SIGTERM)
    wait_for(lambda:status(root)['keeper']['phase']=='stopped' and not status(root)['alive'])
    assert ping(root) is None

def test_stopping_survives_loss_of_keeper_after_the_stop_receipt(tmp_path):
    from test_native_claude_context import provider
    root,path,first=start(tmp_path,{'worker':provider(tmp_path,'eof-hang')})
    wait_for(lambda:json.loads(cli('--state-root',root,'ping','worker').stdout)['diagnostics']['protocol_observed'])
    prior=status(root)['keeper'];before=(root/'kernel/native-start.json').read_bytes()
    result=cli('--state-root',root,'stop');assert result.returncode==0,result.stderr
    assert json.loads((root/'lifecycle.json').read_text(encoding='utf-8'))['desired_state']=='stopped'
    if status(root)['alive']:
        try:os.kill(prior['process']['pid'],signal.SIGTERM)
        except ProcessLookupError:pass
    wait_for(lambda:cli('process-identity',first['pid']).returncode!=0)
    wait_for(lambda:not status(root)['alive'])
    assert (root/'kernel/native-start.json').read_bytes()==before

def test_reused_owner_pid_is_not_mistaken_for_the_old_daemon_or_terminated(tmp_path):
    import subprocess, sys
    root,path,first=start(tmp_path)
    helper=subprocess.Popen([sys._base_executable,'-c','import time;time.sleep(30)'],creationflags=0x08000000)
    try:
        lease_path=root/'lease.json';lease=json.loads(lease_path.read_text(encoding='utf-8'))
        os.kill(first['pid'],signal.SIGTERM)
        wait_for(lambda:cli('process-identity',first['pid']).returncode!=0)
        # Controlled PID-reuse evidence: the current helper has a different
        # creation identity from the recorded daemon lifetime.
        lease['ccbd_pid']=helper.pid;lease['process']['pid']=helper.pid
        lease_path.write_text(json.dumps(lease),encoding='utf-8')
        restored=wait_for(lambda:(r if (r:=ping(root)) and r['pid']!=first['pid'] else None))
        assert restored['pid']!=helper.pid and helper.poll() is None
    finally:
        if status(root)['alive']:stop(root)
        helper.kill();helper.wait(timeout=5)

def test_restart_reconciles_active_work_and_runs_the_existing_fifo_head_once(tmp_path):
    from test_native_provider_faults import provider_spec
    from test_native_reload import connect
    root,path,first=start(tmp_path,{'worker':provider_spec('codex','hold',tmp_path)})
    try:
        request=lambda body:json.loads(cli('--state-root',root,'ask','worker',body).stdout)['job_id']
        active=request('first active task')
        wait_for(lambda:(tmp_path/'observed-owner.json').exists())
        queued=request('already queued second task')
        os.kill(first['pid'],signal.SIGTERM)
        wait_for(lambda:(r if (r:=ping(root)) and r['pid']!=first['pid'] else None))
        server=connect(root)
        old=wait_for(lambda:(r if (r:=server.rpc('watch',{'target':active}))['terminal'] else None))
        assert old['status']=='incomplete' and old['completion_reason']=='native_owner_stopped'
        done=wait_for(lambda:(r if (r:=server.rpc('watch',{'target':queued}))['terminal'] else None))
        assert done['status']=='completed' and done['reply']=='fault fixture result'
        server.token.generation=server.endpoint['generation']
        jobs=server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result']
        assert {j['job_id'] for j in jobs}=={active,queued} and len(jobs)==2
    finally:stop(root)
