"""Pi reload controls are never model tasks or completion authority."""
import json
from pathlib import Path
import time

import pytest
from test_native_daemon import Server, submit, await_watch, spec
from test_native_pi_rpc import pi_spec
from test_native_codex_role_skills import setup as codex_setup, wire

def setup(project, preview=False, mode='worker'):
    role,skill,trace,behavior,_=codex_setup(project,preview)
    manifest=role/'role.toml'
    manifest.write_text(manifest.read_text().replace('codex=[','pi=['))
    if preview:
        adapter=role/'adapters/ccb/adapter.toml'
        adapter.write_text(adapter.read_text()+'supported_providers=["pi"]\n')
    agent=dict(pi_spec(mode),role='agentroles.reader',workspace_path=str(project),
               env={'CCB_FIXTURE_TRACE':str(trace),'CCB_PI_SKILL_BEHAVIOR':str(behavior)})
    return role,skill,trace,behavior,agent

def tasks(trace):
    return [r for r in wire(trace) if r.get('type')=='prompt' and r.get('message','').startswith('CCB_REQ_ID:')]

def phase(server,trace,name):
    deadline=time.monotonic()+8
    while time.monotonic()<deadline:
        rows=[r for r in wire(trace) if r.get('id','').startswith('role-'+name+'-')]
        if rows:return rows[-1]
        assert server.proc.poll() is None
        time.sleep(.025)
    pytest.fail(f'Pi role phase {name} not observed')

@pytest.mark.parametrize('preview',[False,True])
def test_role_skills_reload_and_clear_without_replacing_pi_session(tmp_path,preview):
    role,skill,trace,behavior,agent=setup(tmp_path,preview)
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        before=None
        for index in range(3):
            if index==1:(skill/'SKILL.md').write_text('---\nname: reader\ndescription: Updated.\n---\nFresh body.')
            if index==2:
                p=role/'role.toml';p.write_text(p.read_text().replace('["skills/reader"]','[]'))
            receipt=submit(server)
            assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
            metadata=json.loads((server.root/'providers/pi/worker/session.json').read_text())
            assert metadata['state']['role_skills']['verified'] is True
            roots=metadata['state']['role_skills']['roots']
            assert roots==[] if index==2 else len(roots)==1 and Path(roots[0]).samefile(skill)
            if before:
                assert before['runtime_instance_id']==metadata['runtime_instance_id']
                assert before['state']['sessionId']==metadata['state']['sessionId']
            before=metadata
            assert metadata['state']['messageCount']==2*(index+1)
            stages=[r['id'].split('-')[1] for r in wire(trace) if r.get('id','').startswith('role-')]
            assert stages==['inspect','reload','verify','state']*(index+1)
        assert len(tasks(trace))==3
        plan=json.loads((server.root/'providers/pi/worker/role-plan.json').read_text())
        assert plan['roots']==[]
        assert not (tmp_path/'.pi').exists()
    finally:server.close()

@pytest.mark.parametrize('failure',['missing-bridge','reload-error','stale-generation','missing-skill'])
def test_failed_registration_sends_no_task_and_repair_keeps_session(tmp_path,failure):
    role,skill,trace,behavior,agent=setup(tmp_path)
    behavior.write_text(failure)
    server=Server(tmp_path/'state',{'worker':agent,'peer':spec('worker')})
    try:
        receipt=submit(server)
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='failed' and row['completion_reason'].startswith('pi_role_')
        assert tasks(trace)==[]
        if failure=='missing-bridge':assert not [r for r in wire(trace) if r.get('type')=='prompt']
        jobs=server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result']
        assert len(jobs)==1 and 'PRIVATE_RELOAD_ERROR' not in json.dumps(jobs)
        before=json.loads((server.root/'providers/pi/worker/session.json').read_text())
        peer=submit(server,'peer')
        assert await_watch(server,peer['job_id'],lambda r:r['terminal'])['status']=='completed'
        behavior.write_text('')
        receipt=submit(server)
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        after=json.loads((server.root/'providers/pi/worker/session.json').read_text())
        assert before['runtime_instance_id']==after['runtime_instance_id']
        assert len(tasks(trace))==1
    finally:server.close()

@pytest.mark.parametrize('failure',['bad-envelope','reload-started','foreign-session','busy'])
def test_uncertain_reload_is_quarantined_without_task_input(tmp_path,failure):
    role,skill,trace,behavior,agent=setup(tmp_path)
    behavior.write_text(failure)
    server=Server(tmp_path/'state',{'worker':agent,'peer':spec('worker')})
    try:
        receipt=submit(server)
        deadline=time.monotonic()+8
        fault=server.root/'providers/pi/worker/fault.json'
        while not fault.exists() and time.monotonic()<deadline:time.sleep(.025)
        assert fault.exists()
        assert server.rpc('watch',{'target':receipt['job_id']})['status']=='running'
        assert tasks(trace)==[]
        peer=submit(server,'peer')
        assert await_watch(server,peer['job_id'],lambda r:r['terminal'])['status']=='completed'
    finally:server.close()

@pytest.mark.parametrize('stage',['inspect','reload','verify','state'])
def test_silent_cancel_waits_for_preparation_response_before_fifo_continues(tmp_path,stage):
    role,skill,trace,behavior,agent=setup(tmp_path)
    behavior.write_text('delay-'+stage)
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server)
        phase(server,trace,stage)
        server.rpc('cancel',dict(job_id=receipt['job_id'],record_reply=False))
        assert server.rpc('watch',{'target':receipt['job_id']})['status']=='running'
        behavior.write_text('')
        second=submit(server)
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='cancelled' and row['completion_reason']=='pi_cancelled_before_prompt'
        assert await_watch(server,second['job_id'],lambda r:r['terminal'])['status']=='completed'
        assert len(tasks(trace))==1 and second['job_id'] in tasks(trace)[0]['message']
        assert not [r for r in wire(trace) if r.get('type')=='abort']
        assert len(server.rpc('records.list',{'stream':'replies'},version=1)['result'])==1
    finally:server.close()

@pytest.mark.parametrize('stage',['reload','state'])
def test_deadline_during_reload_never_aborts_or_starts_model_task(tmp_path,stage):
    role,skill,trace,behavior,agent=setup(tmp_path)
    behavior.write_text('delay-'+stage);agent['timeout_s']=.15
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server)
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='cancelled' and row['completion_reason']=='pi_cancelled_before_prompt'
        assert tasks(trace)==[]
        assert not [r for r in wire(trace) if r.get('type')=='abort']
        jobs=server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result']
        assert jobs[0]['terminal_decision']['diagnostics']['deadline_triggered'] is True
    finally:server.close()

def test_foreign_resource_response_cannot_release_pending_prompt(tmp_path):
    role,skill,trace,behavior,agent=setup(tmp_path);behavior.write_text('foreign')
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server);phase(server,trace,'verify')
        assert tasks(trace)==[]
        assert server.rpc('watch',{'target':receipt['job_id']})['status']=='running'
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
    finally:server.close()

def test_pi_lead_reloads_role_skills_when_worker_result_wakes_same_session(tmp_path):
    role,skill,trace,behavior,lead=setup(tmp_path,mode='lead')
    server=Server(tmp_path/'state',{'lead':lead,'worker':spec('worker')})
    try:
        receipt=submit(server,'lead')
        row=await_watch(server,receipt['job_id'],lambda r:r['visible_reply_source']=='message_bureau_reply')
        assert row['reply']=='persistent Pi reviewed result'
        assert len(tasks(trace))==2
        assert len([r for r in wire(trace) if r.get('id','').startswith('role-reload-')])==2
        meta=json.loads((server.root/'providers/pi/lead/session.json').read_text())
        assert meta['state']['messageCount']==4
        assert meta['state']['role_skills']['verified'] is True
    finally:server.close()

def test_pi_preview_skills_require_explicit_provider_support(tmp_path):
    role,skill,trace,behavior,agent=setup(tmp_path,True)
    adapter=role/'adapters/ccb/adapter.toml'
    adapter.write_text('supported_providers=["codex","claude"]\n')
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server)
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        assert not [r for r in wire(trace) if r.get('id','').startswith('role-')]
    finally:server.close()


def test_saved_pi_session_gets_new_bridge_owner_and_live_resources_after_restart(tmp_path):
    role,skill,trace,behavior,agent=setup(tmp_path,mode='persist')
    agent['command'].append(str(tmp_path/'pi-session.json'))
    root=tmp_path/'state'
    server=Server(root,{'worker':agent})
    try:
        receipt=submit(server)
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        before=json.loads((root/'providers/pi/worker/session.json').read_text())
        old_plan=json.loads((root/'providers/pi/worker/role-plan.json').read_text())
    finally:server.close()
    restored=Server(root,{'worker':agent})
    try:
        receipt=submit(restored)
        assert await_watch(restored,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        after=json.loads((root/'providers/pi/worker/session.json').read_text())
        plan=json.loads((root/'providers/pi/worker/role-plan.json').read_text())
        assert before['state']['sessionId']==after['state']['sessionId']
        assert before['runtime_instance_id']!=after['runtime_instance_id']
        assert old_plan['owner']!=plan['owner'] and old_plan['command']!=plan['command']
        assert after['state']['messageCount']==4 and after['state']['role_skills']['verified'] is True
    finally:restored.close()
