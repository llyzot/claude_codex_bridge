"""A provider that exits before admitting work must not create a restart loop."""
import pytest
from test_native_daemon import Server, submit, await_watch, spec
from test_native_provider_faults import provider_spec, fault, recover, healthy_task
from test_native_transport_eof import wait_for


@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
@pytest.mark.parametrize('code', [0, 17])
def test_empty_startup_exit_blocks_once_and_recovers_explicitly(tmp_path, provider, code):
    mode='startup-exit' if code else 'startup-exit-zero'
    agents={'worker':provider_spec(provider, mode, tmp_path), 'healthy':spec('worker')}
    root=tmp_path/'state';server=Server(root, agents)
    try:
        wait_for(lambda:(tmp_path/'startup-waiting').exists())
        (tmp_path/'release-startup').touch()
        record=fault(server)
        assert record['phase']=='startup' and record['reason']=='provider_start_failed'
        assert record['exit_code']==code and record['job_id'] is None
        assert record['pid'] is not None
        assert len((tmp_path/'fault-launches').read_text().splitlines())==1
        healthy_task(server)
        with pytest.raises(AssertionError, match='provider is faulted'):submit(server)
    finally:
        (tmp_path/'release-startup').touch();server.close()
    restored=Server(root, agents)
    try:
        assert fault(restored)['fault_id']==record['fault_id']
        assert len((tmp_path/'fault-launches').read_text().splitlines())==1
        recover(restored, record)
        task=submit(restored)
        assert await_watch(restored,task['job_id'],lambda r:r['terminal'])['status']=='completed'
        assert len((tmp_path/'fault-launches').read_text().splitlines())==2
    finally:restored.close()


@pytest.mark.parametrize('provider', ['pi', 'codex'])
def test_preaccepted_jobs_fail_without_dispatch_or_automatic_respawn(tmp_path, provider):
    server=Server(tmp_path/'state',{'worker':provider_spec(provider,'startup-exit',tmp_path),'healthy':spec('worker')})
    try:
        wait_for(lambda:(tmp_path/'startup-waiting').exists())
        ids=[submit(server,body=f'queued {n}')['job_id'] for n in range(2)]
        (tmp_path/'release-startup').touch()
        for job in ids:
            result=await_watch(server,job,lambda r:r['terminal'])
            assert result['status']=='failed' and result['completion_reason']=='provider_start_failed'
        jobs=server.rpc('records.latest',{'stream':'jobs','identity':'job_id','agent_name':'worker'},version=1)['result']
        for job in jobs:
            if job['job_id'] in ids:assert job['terminal_decision']['diagnostics']['provider_prompt_sent'] is False
        healthy_task(server)
        assert len((tmp_path/'fault-launches').read_text().splitlines())==1
    finally:
        (tmp_path/'release-startup').touch();server.close()


def test_failed_worker_startup_returns_through_registered_lead_chain(tmp_path):
    server=Server(tmp_path/'state',{'lead':spec('lead-echo'),'worker':provider_spec('codex','startup-exit',tmp_path)})
    try:
        parent=submit(server,'lead')['job_id']
        wait_for(lambda:server.rpc('records.list',{'stream':'callbacks'},version=1)['result'])
        (tmp_path/'release-startup').touch()
        result=await_watch(server,parent,lambda r:r['visible_reply_source']=='message_bureau_reply')
        assert 'Child status: failed' in result['reply'] and 'Provider is faulted' in result['reply']
        edges=server.rpc('records.latest',{'stream':'callbacks','identity':'edge_id'},version=1)['result']
        assert len(edges)==1 and edges[0]['state']=='done' and edges[0]['child_status']=='failed'
        assert len((tmp_path/'fault-launches').read_text().splitlines())==1
    finally:
        (tmp_path/'release-startup').touch();server.close()
