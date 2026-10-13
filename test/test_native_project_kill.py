"""Public project shutdown preserves state and retires only owned Herdr topology."""
import json
import os
import subprocess
import sys
import pytest
from test_native_cli_security import cli
from test_native_keeper import start,stop,status,wait_for
from test_native_herdr_open import project
from test_native_herdr_namespace import setup,server,mutate_count,fault
from test_native_herdr_namespace import run as namespace_run

pytestmark=pytest.mark.skipif(os.name!='nt',reason='Windows native project lifecycle')

def test_kill_help_and_absent_project_do_not_bootstrap_state(tmp_path):
    before=set(tmp_path.iterdir())
    help=cli('kill','--help',cwd=tmp_path)
    assert help.returncode==0 and 'kill' in help.stdout
    result=cli('kill',cwd=tmp_path)
    assert result.returncode==0,result.stderr
    assert json.loads(result.stdout)['status']=='not_running'
    assert set(tmp_path.iterdir())==before

def test_kill_stops_keeper_and_daemon_and_is_repeatable_without_source_config(tmp_path):
    root,path,first=start(tmp_path);path.unlink()
    try:
        result=cli('--state-root',root,'kill',timeout=20)
        assert result.returncode==0,result.stderr
        assert json.loads(result.stdout)['status']=='stopped'
        assert status(root)['alive'] is False
        assert cli('process-identity',first['pid']).returncode!=0
        assert (root/'configuration.json').is_file()
        assert cli('--state-root',root,'kill').returncode==0
    finally:
        if status(root)['alive']:stop(root)

@pytest.mark.parametrize('mode',[None,'after-error','before-error'])
def test_kill_closes_owned_workspace_once_and_preserves_history(project,mode):
    p=project;opened=p['open']();assert opened.returncode==0,opened.stderr
    wait_for(p['ready'],timeout=20)
    before=server(p['backend']);before_history=p['log'].read_bytes()
    (p['root']/'.ccb/ccb.config').unlink()
    if mode:fault(p['backend'],['workspace','close'],mode)
    result=cli('kill',cwd=p['root'],timeout=25)
    assert result.returncode==0,result.stderr
    expected='closing' if mode=='before-error' else 'closed'
    assert json.loads(result.stdout)['status']==expected
    if expected=='closed':
        assert server(p['backend'])['workspaces']=={'external':before['workspaces']['external']}
        assert server(p['backend'])['panes']=={'external:p1':before['panes']['external:p1']}
    else:assert server(p['backend'])['workspaces']==before['workspaces']
    assert p['log'].read_bytes()==before_history
    assert (p['native']/'configuration.json').exists()
    repeated=cli('kill',cwd=p['root'],timeout=25)
    assert repeated.returncode==0 and json.loads(repeated.stdout)['status']==expected
    assert mutate_count(p['backend'],['workspace','close'])==1

def test_unsupported_kill_options_do_not_stop_runtime(tmp_path):
    root,path,first=start(tmp_path)
    try:
        result=cli('--state-root',root,'kill','--force')
        assert result.returncode!=0
        assert status(root)['alive']
        assert cli('--state-root',root,'ping').returncode==0
    finally:stop(root)

def test_runtime_reference_change_cannot_redirect_namespace_close(project):
    p=project;result=p['open']();assert result.returncode==0,result.stderr
    wait_for(p['ready'],timeout=20)
    config=json.loads((p['native']/'herdr/herdr-open.json').read_text())
    other=p['root']/'other-runtime';(other/'ccbd').mkdir(parents=True)
    (p['root']/'.ccb/runtime-root-ref.json').write_text(json.dumps({'record_type':'ccb_runtime_root_ref','project_id':config['project_id'],'runtime_state_root':str(other)}))
    result=cli('--state-root',p['native'],'kill',timeout=25)
    assert result.returncode!=0 and 'runtime reference changed' in result.stderr
    assert mutate_count(p['backend'],['workspace','close'])==0

def test_legacy_ownership_is_not_controlled_by_native_kill(tmp_path):
    root=tmp_path/'state';root.mkdir()
    (root/'lease.json').write_text(json.dumps({'mount_state':'mounted','ccbd_pid':os.getpid()}))
    before=(root/'lease.json').read_bytes()
    result=cli('--state-root',root,'kill')
    assert result.returncode!=0 and 'original shutdown path' in result.stderr
    assert (root/'lease.json').read_bytes()==before and not (root/'kernel').exists()

def test_zero_observation_budget_keeps_the_same_shutdown_in_progress(tmp_path):
    from test_native_claude_context import provider
    root,path,first=start(tmp_path,{'worker':provider(tmp_path,'eof-hang')})
    before=(root/'kernel/native-start.json').read_bytes()
    try:
        result=cli('--state-root',root,'kill','--timeout-ms','0',timeout=20)
        assert result.returncode==0,result.stderr
        receipt=json.loads(result.stdout)
        assert receipt['status'] in ['stopping','stopped']
        if receipt['status']=='stopping':assert receipt['observation_expired'] is True
        else:assert cli('process-identity',first['pid']).returncode!=0
        wait_for(lambda:not status(root)['alive'])
        again=cli('--state-root',root,'kill',timeout=20)
        assert again.returncode==0 and json.loads(again.stdout)['status']=='stopped'
        assert (root/'kernel/native-start.json').read_bytes()==before
    finally:
        if status(root)['alive']:stop(root)

def test_another_project_runtime_is_rejected_before_stop(tmp_path):
    root,path,first=start(tmp_path)
    project=tmp_path/'other-project';(project/'.ccb').mkdir(parents=True)
    try:
        result=cli('--project',project,'--state-root',root,'kill')
        assert result.returncode!=0 and 'another project' in result.stderr
        assert status(root)['alive'] and cli('--state-root',root,'ping').returncode==0
        assert not (root/'kernel/native-keeper-stop.json').exists()
    finally:stop(root)

def test_kill_pre_backend_namespace_closes_once_and_preserves_other_workspaces(setup):
    project,backend,request=setup
    ns=namespace_run(request)['namespace'];before=server(backend)
    result=cli('kill',cwd=project,timeout=25)
    assert result.returncode==0,result.stderr
    assert json.loads(result.stdout)['status']=='closed'
    assert not (project/'.ccb/ccbd/configuration.json').exists()
    assert server(backend)['workspaces']=={'external':before['workspaces']['external']}
    assert cli('kill',cwd=project,timeout=25).returncode==0
    assert mutate_count(backend,['workspace','close'])==1
    assert ns['workspace_id'] not in server(backend)['workspaces']

@pytest.mark.parametrize('change',['extra-pane','foreign-owner'])
def test_pre_backend_kill_refuses_unowned_live_contents(setup,change):
    project,backend,request=setup;ns=namespace_run(request)['namespace'];state=server(backend)
    if change=='extra-pane':state['panes']['foreign']={'pane_id':'foreign','workspace_id':ns['workspace_id'],'tokens':{}}
    else:state['workspaces'][ns['workspace_id']]['tokens']['ccb_native_owner']='foreign'
    (backend/'server.json').write_text(json.dumps(state))
    result=cli('kill',cwd=project,timeout=25)
    assert result.returncode!=0 and mutate_count(backend,['workspace','close'])==0

def test_closed_pre_backend_namespace_reopens_only_via_public_open_intent(setup):
    project,backend,request=setup;ns=namespace_run(request)['namespace']
    result=cli('kill',cwd=project,timeout=25);assert result.returncode==0,result.stderr
    assert namespace_run({**request,'action':'inspect'})['namespace']['status']=='retired'

def test_missing_initial_authority_is_not_reconstructed(setup):
    project,backend,request=setup
    namespace_run(request)
    (project/'.ccb/ccbd/herdr/open-configuration.json').unlink()
    result=cli('kill',cwd=project,timeout=25)
    assert result.returncode!=0 and 'refusing to reconstruct' in result.stderr
    assert mutate_count(backend,['workspace','close'])==0
