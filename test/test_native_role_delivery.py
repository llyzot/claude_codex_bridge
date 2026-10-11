"""A resource failure must retain a Worker reply without a hot retry loop."""
import json
import time
import pytest
from test_native_daemon import Server,submit,await_watch,spec
from test_native_pi_role_skills import setup as pi_setup
from test_native_codex_role_skills import setup as codex_setup
from test_native_claude_role_skills import setup as claude_setup

SETUPS={'pi':(pi_setup,'missing-skill'),'codex':(codex_setup,'missing'),'claude':(claude_setup,'missing')}

def jobs(server):
    return server.rpc('records.latest',dict(stream='jobs',agent_name='lead',identity='job_id'),version=1)['result']

def held(server,provider):
    path=server.root/f'providers/{provider}/lead/fault.json'
    until=time.monotonic()+8
    while not path.exists() and time.monotonic()<until:time.sleep(.025)
    assert path.exists()
    return json.loads(path.read_text())

def delivered(server):
    until=time.monotonic()+8
    while time.monotonic()<until:
        rows=jobs(server)
        if len(rows)==2 and rows[-1]['status']=='completed':return rows
        time.sleep(.025)
    pytest.fail('retained reply did not resume')

@pytest.mark.parametrize('provider',SETUPS)
def test_role_failure_holds_original_reply_until_exact_recovery_without_restart(tmp_path,provider):
    factory,failure=SETUPS[provider]
    role,skill,trace,behavior,agent=factory(tmp_path)
    behavior.write_text(failure)
    server=Server(tmp_path/'state',{'lead':agent,'worker':spec('worker')})
    try:
        receipt=submit(server,'worker','lead')
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        fault=held(server,provider)
        assert fault['delivery_failure'] is True and fault['idle_recovery'] is True
        first=jobs(server);assert len(first)==1 and first[0]['status']=='failed'
        head=server.rpc('mailbox-head',{'agent_name':'lead'})['head']
        assert head['event_type']=='task_reply' and head['payload_ref'].startswith('reply:')
        time.sleep(.35)
        assert len(jobs(server))==1
        before=json.loads((server.root/f'providers/{provider}/lead/session.json').read_text())
        with pytest.raises(AssertionError,match='identity mismatch'):
            server.rpc('provider-recover',{'agent_name':'lead','fault_id':'foreign'})
        behavior.write_text('')
        result=server.rpc('provider-recover',{'agent_name':'lead','fault_id':fault['fault_id']})
        assert result['kept_runtime'] is True
        rows=delivered(server)
        assert rows[-1]['provider_options']['reply_delivery_inbound_event_id']==head['inbound_event_id']
        assert server.rpc('mailbox-head',{'agent_name':'lead'})['head'] is None
        after=json.loads((server.root/f'providers/{provider}/lead/session.json').read_text())
        assert before['runtime_instance_id']==after['runtime_instance_id']
        replies=server.rpc('records.list',{'stream':'replies'},version=1)['result']
        assert len(replies)==1 and replies[0]['reply']=='worker result'
    finally:server.close()

def test_reply_hold_survives_daemon_restart_and_keeps_original_head(tmp_path):
    role,skill,trace,behavior,agent=claude_setup(tmp_path)
    behavior.write_text('missing');root=tmp_path/'state'
    server=Server(root,{'lead':agent,'worker':spec('worker')})
    try:
        receipt=submit(server,'worker','lead');await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        fault=held(server,'claude')
        head=server.rpc('mailbox-head',{'agent_name':'lead'})['head']
    finally:server.close()
    restored=Server(root,{'lead':agent,'worker':spec('worker')})
    try:
        time.sleep(.2)
        assert len(jobs(restored))==1
        assert restored.rpc('mailbox-head',{'agent_name':'lead'})['head']['inbound_event_id']==head['inbound_event_id']
        behavior.write_text('')
        restored.rpc('provider-recover',{'agent_name':'lead','fault_id':fault['fault_id']})
        delivered(restored)
        assert restored.rpc('mailbox-head',{'agent_name':'lead'})['head'] is None
    finally:restored.close()
