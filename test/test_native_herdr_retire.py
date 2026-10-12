"""Durable namespace closure using offline Herdr; no account server controls."""
import json
import os
import pytest
from test_native_herdr_namespace import setup,run,call,server,journal,fault,mutate_count


@pytest.mark.parametrize('mode',[None,'after-error','before-error'])
def test_retirement_is_scoped_and_uncertain_close_is_never_replayed(setup,mode):
    project,backend,request=setup
    mounted=run(request)['namespace'];before=server(backend)
    if mode:fault(backend,['workspace','close'],mode)
    retired=run({**request,'action':'retire'})['namespace']
    expected='retiring' if mode=='before-error' else 'retired'
    assert retired['status']==expected
    assert retired['retirement']['close_sent'] is True
    assert mutate_count(backend,['workspace','close'])==1
    repeated=run({**request,'action':'retire'})['namespace']
    assert repeated['retirement']['operation_id']==retired['retirement']['operation_id']
    assert repeated['status']==expected and mutate_count(backend,['workspace','close'])==1
    after=server(backend)
    assert after['workspaces']['external']==before['workspaces']['external']
    assert after['panes']['external:p1']==before['panes']['external:p1']
    assert (mounted['workspace_id'] in after['workspaces'])==(mode=='before-error')
    assert call(request).returncode!=0,'Retired state must not silently recreate a namespace'


@pytest.mark.parametrize('change',['foreign-owner','extra-pane'])
def test_foreign_or_untracked_namespace_contents_block_retirement(setup,change):
    project,backend,request=setup;ns=run(request)['namespace'];state=server(backend)
    if change=='foreign-owner':state['workspaces'][ns['workspace_id']]['tokens']['ccb_native_owner']='someone-else'
    else:state['panes']['extra']={'pane_id':'extra','workspace_id':ns['workspace_id'],'tokens':{}}
    (backend/'server.json').write_text(json.dumps(state))
    previous=journal(project).read_bytes()
    assert call({**request,'action':'retire'}).returncode!=0
    assert mutate_count(backend,['workspace','close'])==0
    assert journal(project).read_bytes()==previous


def test_live_daemon_fences_namespace_retirement(setup):
    project,backend,request=setup;run(request)
    (project/'.ccb/ccbd/lease.json').write_text(json.dumps({'mount_state':'mounted','ccbd_pid':os.getpid()}))
    result=call({**request,'action':'retire'})
    assert result.returncode!=0 and 'ownership is live' in result.stderr
    assert mutate_count(backend,['workspace','close'])==0


def test_missing_namespace_retirement_does_not_create_state(setup):
    project,backend,request=setup
    result=run({**request,'action':'retire'})
    assert result['status']=='not_found'
    assert not (project/'.ccb/ccbd').exists()
    assert not (backend/'calls.jsonl').exists()


def test_reused_retired_workspace_id_is_never_closed_again(setup):
    project,backend,request=setup;ns=run(request)['namespace']
    run({**request,'action':'retire'})
    state=server(backend)
    state['workspaces'][ns['workspace_id']]={'workspace_id':ns['workspace_id'],'label':'new user workspace','tokens':{}}
    (backend/'server.json').write_text(json.dumps(state))
    result=call({**request,'action':'retire'})
    assert result.returncode!=0 and 'reappeared' in result.stderr
    assert mutate_count(backend,['workspace','close'])==1
    assert server(backend)['workspaces'][ns['workspace_id']]['label']=='new user workspace'


@pytest.mark.skipif(os.name!='nt',reason='Windows launch lock interoperability')
@pytest.mark.parametrize('relative',['herdr/native-open-launch.lock','herdr/native-open.lock','kernel/native-keeper-launch.lock','kernel/native-launch.lock'])
def test_concurrent_launch_lock_prevents_close(setup,relative):
    import msvcrt
    project,backend,request=setup;run(request)
    path=project/'.ccb/ccbd'/relative;path.parent.mkdir(parents=True,exist_ok=True)
    with path.open('a+b') as held:
        held.write(b'0');held.flush();held.seek(0)
        msvcrt.locking(held.fileno(),msvcrt.LK_NBLCK,1)
        try:
            result=call({**request,'action':'retire'})
            assert result.returncode!=0 and 'still in progress' in result.stderr
            assert mutate_count(backend,['workspace','close'])==0
        finally:
            held.seek(0);msvcrt.locking(held.fileno(),msvcrt.LK_UNLCK,1)
