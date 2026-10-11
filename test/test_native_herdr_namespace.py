"""Native durable topology against an offline fixture, not a live Herdr Server."""
import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time

import pytest
from test_native_cli_security import BIN
from agents.models_runtime.layout_runtime.parser import parse_layout_spec
from cli.services.tmux_start_layout import _get_specified_percent

STUB=Path(__file__).parent/'stubs/herdr_namespace.py'


@pytest.fixture
def setup(tmp_path,monkeypatch):
    monkeypatch.delenv('CCB_RUNTIME_STATE_HOME',raising=False)
    project=tmp_path/'project';project.mkdir()
    backend=tmp_path/'backend';backend.mkdir()
    state={'next_workspace':0,'focused':'external','workspaces':{'external':{'workspace_id':'external','label':'user','tokens':{}}},
           'panes':{'external:p1':{'pane_id':'external:p1','workspace_id':'external','tokens':{'keep':'unchanged'}}}}
    (backend/'server.json').write_text(json.dumps(state),encoding='utf-8')
    identity=str(project.resolve()).replace('\\','/')
    if os.name=='nt':identity=identity.lower()
    config=dict(schema_version=2,project_id=hashlib.sha256(identity.encode()).hexdigest(),project_root=str(project),
                agents={name:{'provider':provider,'workspace_path':str(project),'command':None} for name,provider in [('lead','pi'),('worker','codex')]},
                source_config={'layout':'lead:pi; worker:codex'})
    request=dict(schema_version=1,action='prepare',session='native-namespace-test',command=[sys.executable,'-I',str(STUB),str(backend)],config=config,timeout_ms=1500)
    return project,backend,request


def call(request):
    return subprocess.run([str(BIN),'herdr-namespace'],input=json.dumps(request),capture_output=True,text=True,encoding='utf-8',timeout=20)


def run(request):
    result=call(request);assert result.returncode==0,result.stderr
    return json.loads(result.stdout)


def server(backend):return json.loads((backend/'server.json').read_text())
def journal(project):return project/'.ccb/ccbd/herdr/namespace.json'
def calls(backend):return [json.loads(line) for line in (backend/'calls.jsonl').read_text().splitlines()] if (backend/'calls.jsonl').exists() else []
def mutate_count(backend,command):return sum(row[:2]==command for row in calls(backend))
def fault(backend,command,mode='after-error'):(backend/'fault.json').write_text(json.dumps({'command':command,'mode':mode}))


def expected_steps(node,output):
    if node.kind=='leaf':return
    right=_get_specified_percent(node.right);left=_get_specified_percent(node.left)
    pct=right if right is not None else 100-left if left is not None else round(node.right.leaf_count*100/node.leaf_count)
    output.append(dict(slot=node.right.iter_leaves()[0].name,parent=node.iter_leaves()[0].name,
                       direction='right' if node.kind=='horizontal' else 'bottom',percent=max(1,min(99,pct))))
    expected_steps(node.left,output);expected_steps(node.right,output)


@pytest.mark.parametrize('layout',[
    'lead; worker','lead@30; worker','lead; worker@20','lead, worker',
    '(lead, worker); (third, fourth@35)','lead; (worker, third); fourth',
    'cmd@15, (lead; worker)','(lead; worker), third@25',
])
def test_layout_plan_preserves_python_split_order_geometry_and_is_read_only(setup,layout):
    project,backend,request=setup
    parsed=parse_layout_spec(layout)
    request['config']['agents']={leaf.name:{'provider':'pi','workspace_path':str(project),'command':None}
                                 for leaf in parsed.iter_leaves() if leaf.name!='cmd'}
    request['config']['source_config']={'layout':layout}
    request['action']='plan'
    planned=run(request)['plan'];expected=[];expected_steps(parsed,expected)
    assert planned['splits']==expected
    assert planned['visible_slots']==[leaf.name for leaf in parsed.iter_leaves()]
    assert not journal(project).exists() and not calls(backend)


def test_balanced_default_layout_and_weighted_rounding(setup):
    project,backend,request=setup
    names=['a','b','c','d','e','f','g','h']
    request['config']['agents']={n:{'provider':'pi','workspace_path':str(project),'command':None} for n in names}
    request['config']['source_config']={'default_agents':names}
    request['action']='plan'
    planned=run(request)['plan']
    assert planned['splits'][0]['percent']==50 and planned['visible_slots']==names
    request['config']['source_config']['layout']='(a,b,c); (d,e,f,g,h)'
    assert run(request)['plan']['splits'][0]['percent']==62


def test_root_agent_uses_its_workspace_and_launch_environment_is_not_persisted(setup):
    project,backend,request=setup
    work=project/'worktree';work.mkdir()
    request['config']['agents']['lead']['workspace_path']=str(work)
    request['config']['agents']['lead']['env']={'SOURCE_HOME':'live-resources'}
    request['slot_env']={'lead':{'PRIVATE_TOKEN':'not-for-the-ledger'},'worker':{'CCB_AGENT_NAME':'worker'}}
    result=run(request)['namespace'];root=server(backend)['panes'][result['bindings']['lead']['pane_id']]
    assert Path(root['cwd']).samefile(work)
    assert root['env']=={'SOURCE_HOME':'live-resources','PRIVATE_TOKEN':'not-for-the-ledger'}
    assert server(backend)['panes'][result['bindings']['worker']['pane_id']]['env']['CCB_AGENT_NAME']=='worker'
    assert 'not-for-the-ledger' not in journal(project).read_text()
    request['slot_env']['lead']['PRIVATE_TOKEN']='changed'
    assert call(request).returncode!=0


@pytest.mark.parametrize('env',[{'HERDR_ENV':'1'},{'herdr_session':'foreign'},{'BAD=KEY':'x'},{'VALUE':'bad\0value'}])
def test_launch_environment_cannot_override_herdr_context_or_invalid_fields(setup,env):
    project,backend,request=setup;request['slot_env']={'lead':env}
    assert call(request).returncode!=0
    assert not journal(project).exists() and not calls(backend)


def test_preparation_is_persistent_idempotent_and_does_not_touch_existing_workspace(setup):
    project,backend,request=setup;before=server(backend)
    result=run(request);record=result['namespace']
    assert record['status']=='prepared' and not result['provider_commands_sent']
    assert set(record['bindings'])=={'lead','worker'}
    after=server(backend)
    assert after['focused']==before['focused']
    assert after['workspaces']['external']==before['workspaces']['external'] and after['panes']['external:p1']==before['panes']['external:p1']
    assert mutate_count(backend,['workspace','create'])==1 and mutate_count(backend,['pane','split'])==1
    repeated=run(request)['namespace']
    assert repeated['bindings']==record['bindings']
    assert mutate_count(backend,['workspace','create'])==1 and mutate_count(backend,['pane','split'])==1
    assert not any(row[:2] in [['pane','run'],['agent','start'],['server','stop']] for row in calls(backend))
    assert json.loads(journal(project).read_text())['pending'] is None


@pytest.mark.parametrize('output',['empty','plain'])
def test_metadata_command_acknowledgement_is_confirmed_from_inventory(setup,output):
    project,backend,request=setup
    state=server(backend);state['metadata_output']=output;(backend/'server.json').write_text(json.dumps(state))
    assert run(request)['namespace']['status']=='prepared'


@pytest.mark.parametrize('command',[['workspace','create'],['pane','split']])
def test_lost_creation_ack_stays_uncertain_until_exact_operator_confirmation(setup,command):
    project,backend,request=setup;fault(backend,command)
    partial=run(request)['namespace']
    assert partial['status']=='uncertain' and partial['pending']['sent'] is True
    pending=partial['pending']
    again=run(request)['namespace']
    assert again['pending']['operation_id']==pending['operation_id'] and mutate_count(backend,command)==1
    wrong={**request,'action':'confirm','operation_id':'foreign','confirm_pending_creation':True,'workspace_id':'external','pane_id':'external:p1'}
    assert call(wrong).returncode!=0
    assert json.loads(journal(project).read_text())['bindings']==partial['bindings']
    live=server(backend)
    workspace=next(w for w in live['workspaces'] if w!='external')
    existing={v['pane_id'] for v in partial['bindings'].values()}
    target=next(p for p in live['panes'] if p.startswith(workspace+':') and p not in existing)
    confirmed=run({**request,'action':'confirm','operation_id':pending['operation_id'],
                   'confirm_pending_creation':True,'workspace_id':workspace,'pane_id':target})['namespace']
    assert confirmed['status']=='prepared' and mutate_count(backend,command)==1


@pytest.mark.parametrize('command',[['workspace','report-metadata'],['pane','report-metadata']])
def test_lost_metadata_ack_is_repaired_by_observation_without_repeating_the_write(setup,command):
    project,backend,request=setup;fault(backend,command)
    first=run(request)['namespace'];assert first['status']=='uncertain'
    before=mutate_count(backend,command)
    restored=run(request)['namespace'];assert restored['status']=='prepared'
    expected=1 if command[0]=='workspace' else 2
    assert mutate_count(backend,command)==expected
    assert before==1


def test_unapplied_metadata_needs_confirmation_and_rejects_foreign_owner(setup):
    project,backend,request=setup;fault(backend,['pane','report-metadata'],'before-error')
    first=run(request)['namespace'];assert first['pending']['kind']=='tag_pane'
    assert run(request)['namespace']['status']=='uncertain'
    confirmed=run({**request,'action':'confirm','operation_id':first['pending']['operation_id'],'confirm_pending_creation':True})['namespace']
    assert confirmed['status']=='prepared'
    live=server(backend);pane=confirmed['bindings']['lead']['pane_id'];live['panes'][pane]['tokens']['ccb_native_owner']='foreign'
    (backend/'server.json').write_text(json.dumps(live))
    before=calls(backend)
    failed=call(request);assert failed.returncode!=0
    assert not any(row[1] in ['create','split','report-metadata'] for row in calls(backend)[len(before):])


def test_changed_plan_and_invalid_config_fail_before_external_mutation(setup):
    project,backend,request=setup;run(request)
    before=len(calls(backend));changed=copy.deepcopy(request);changed['config']['source_config']['layout']='worker; lead'
    assert call(changed).returncode!=0 and len(calls(backend))==before
    invalid=copy.deepcopy(request);invalid['config']['source_config']['layout']='lead:claude; worker'
    invalid['action']='plan';assert call(invalid).returncode!=0


@pytest.mark.parametrize('field,value', [('bindings',[]),('pending',{}),('workspace_tagged','true'),('owner_id','foreign')])
def test_corrupt_binding_ledger_is_rejected_without_remote_calls(setup,field,value):
    project,backend,request=setup;run(request)
    record=json.loads(journal(project).read_text());record[field]=value
    journal(project).write_text(json.dumps(record))
    before=len(calls(backend))
    result=call(request)
    assert result.returncode!=0 and len(calls(backend))==before


def test_missing_workspace_and_config_secrets_do_not_create_remote_objects_or_leak(setup):
    project,backend,request=setup
    request['config']['agents']['worker']['workspace_path']=str(project/'absent')
    assert call(request).returncode!=0 and not calls(backend) and not journal(project).exists()
    request['config']['agents']['worker']['workspace_path']=str(project)
    request['config']['agents']['lead']['env']={'PRIVATE':'not-in-namespace-journal'}
    result=run(request)
    assert 'not-in-namespace-journal' not in json.dumps(result)+journal(project).read_text()


def test_inspection_is_local_and_reports_plan_mismatch_without_rebinding(setup):
    project,backend,request=setup;run(request)
    changed=copy.deepcopy(request);changed['action']='inspect';changed['config']['source_config']['layout']='worker; lead'
    before=len(calls(backend));observed=run(changed)
    assert not observed['matches_plan'] and not observed['live_checked'] and len(calls(backend))==before
    changed['session']='different-session';assert call(changed).returncode!=0


@pytest.mark.skipif(os.name!='nt',reason='owned Windows command-tree lifetime')
def test_live_preparation_holds_single_writer_but_inspection_does_not_wait(setup):
    project,backend,request=setup;fault(backend,['workspace','create'],'hang-after');request['timeout_ms']=10000
    process=subprocess.Popen([str(BIN),'herdr-namespace'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                             text=True,encoding='utf-8',creationflags=0x08000000)
    try:
        process.stdin.write(json.dumps(request));process.stdin.close()
        deadline=time.monotonic()+5
        while not (backend/'mutation-finished').exists() and time.monotonic()<deadline:time.sleep(.02)
        assert (backend/'mutation-finished').exists() and process.poll() is None
        observed=run({**request,'action':'inspect'})
        assert observed['namespace']['pending']['sent'] and process.poll() is None
        second=call(request);assert second.returncode!=0 and 'writer' in second.stderr
        assert mutate_count(backend,['workspace','create'])==1
    finally:
        if process.poll() is None:process.kill();process.wait(timeout=3)


@pytest.mark.skipif(os.name!='nt',reason='owned Windows command-tree crash recovery')
def test_controller_crash_after_remote_creation_does_not_replay_it(setup):
    project,backend,request=setup;fault(backend,['workspace','create'],'hang-after')
    request['timeout_ms']=10000
    process=subprocess.Popen([str(BIN),'herdr-namespace'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                             text=True,encoding='utf-8',creationflags=0x08000000)
    try:
        process.stdin.write(json.dumps(request));process.stdin.close()
        deadline=time.monotonic()+5
        while not (backend/'mutation-finished').exists() and time.monotonic()<deadline:time.sleep(.02)
        assert (backend/'mutation-finished').exists()
        process.kill();process.wait(timeout=3)
        observed=run({**request,'action':'inspect'})
        assert observed['live_checked'] is False and observed['namespace']['pending']['sent']
        recovered=run(request)['namespace'];assert recovered['status']=='uncertain'
        assert mutate_count(backend,['workspace','create'])==1
    finally:
        if process.poll() is None:process.kill();process.wait(timeout=3)
