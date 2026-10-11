"""Owned, copy-free discovery views and correlated Claude skill reloads."""
import json
from pathlib import Path
import time
import os
import queue
import subprocess
import threading
import uuid

import pytest
from test_native_daemon import Server, submit, await_watch, spec
from test_native_claude_stream import claude_spec
from test_native_codex_role_skills import setup as codex_setup, wire

def setup(project,preview=False,mode='worker'):
    role,skill,trace,behavior,_=codex_setup(project,preview)
    p=role/'role.toml';p.write_text(p.read_text().replace('codex=[','claude=['))
    agent=dict(claude_spec(mode),role='agentroles.reader',workspace_path=str(project),
        env={'CCB_FIXTURE_TRACE':str(trace),'CCB_CLAUDE_SKILL_BEHAVIOR':str(behavior)})
    return role,skill,trace,behavior,agent

def inputs(trace):return [r for r in wire(trace) if r.get('type')=='user']

def await_refresh(server,trace):
    deadline=time.monotonic()+8
    while time.monotonic()<deadline:
        rows=[r for r in wire(trace) if r.get('request',{}).get('subtype')=='reload_skills']
        if rows:return rows[-1]
        assert server.proc.poll() is None
        time.sleep(.025)
    pytest.fail('Claude refresh not observed')

def view(server):return server.root/'providers/claude/worker/role-view'

@pytest.mark.parametrize('preview',[False,True])
def test_live_skill_bindings_add_remove_and_refresh_on_one_session(tmp_path,preview):
    role,skill,trace,behavior,agent=setup(tmp_path,preview)
    original=(skill/'SKILL.md').read_text()
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        previous=None
        for index in range(4):
            if index==1:(skill/'SKILL.md').write_text(original.replace('Read a sample document.','Updated live description.'))
            p=role/'role.toml'
            if index==2:p.write_text(p.read_text().replace('["skills/reader"]','[]'))
            if index==3:p.write_text(p.read_text().replace('skills=[]','skills=["skills/reader"]') if preview else p.read_text().replace('claude=[]','claude=["skills/reader"]'))
            receipt=submit(server)
            assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
            state=json.loads((server.root/'providers/claude/worker/session.json').read_text())
            assert state['state']['role_skills']['verified'] is True
            links=list((view(server)/'.claude/skills').iterdir())
            if index==2:assert links==[]
            else:
                assert len(links)==1 and links[0].samefile(skill)
                assert (links[0]/'SKILL.md').read_text()==(skill/'SKILL.md').read_text()
                assert links[0].name not in inputs(trace)[-1]['message']['content']
                assert state['state']['role_skills']['bindings'][0]['alias']==links[0].name
            if previous:assert state['runtime_instance_id']==previous['runtime_instance_id'] and state['state']['session_id']==previous['state']['session_id']
            previous=state
        assert (skill/'note.txt').read_text()=='live reference'
        assert len(inputs(trace))==4
        assert len([r for r in wire(trace) if r.get('request',{}).get('subtype')=='initialize'])==1
    finally:server.close()

@pytest.mark.parametrize('failure',['missing','duplicate','builtin','reject','collision'])
def test_failed_catalogue_sends_no_input_and_recovers_same_process(tmp_path,failure):
    role,skill,trace,behavior,agent=setup(tmp_path);behavior.write_text(failure)
    server=Server(tmp_path/'state',{'worker':agent,'peer':spec('worker')})
    try:
        receipt=submit(server)
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='failed' and row['completion_reason'].startswith('claude_role_skills_')
        assert inputs(trace)==[]
        before=json.loads((server.root/'providers/claude/worker/session.json').read_text())
        jobs=server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result']
        assert len(jobs)==1 and 'PRIVATE_REFRESH_ERROR' not in json.dumps(jobs)
        peer=submit(server,'peer');assert await_watch(server,peer['job_id'],lambda r:r['terminal'])['status']=='completed'
        behavior.write_text('');receipt=submit(server)
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        after=json.loads((server.root/'providers/claude/worker/session.json').read_text())
        assert before['runtime_instance_id']==after['runtime_instance_id']
    finally:server.close()

def test_stale_removed_alias_is_rejected(tmp_path):
    role,skill,trace,behavior,agent=setup(tmp_path)
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server);await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        p=role/'role.toml';p.write_text(p.read_text().replace('["skills/reader"]','[]'))
        behavior.write_text('stale');receipt=submit(server)
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='failed' and row['completion_reason']=='claude_role_skills_stale'
        assert len(inputs(trace))==1 and (skill/'SKILL.md').is_file()
    finally:server.close()

@pytest.mark.parametrize('deadline',[False,True])
def test_cancel_and_deadline_during_refresh_never_send_user_input(tmp_path,deadline):
    role,skill,trace,behavior,agent=setup(tmp_path);behavior.write_text('delay')
    if deadline:agent['timeout_s']=.15
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server);await_refresh(server,trace)
        if not deadline:server.rpc('cancel',dict(job_id=receipt['job_id'],record_reply=False))
        assert server.rpc('watch',{'target':receipt['job_id']})['status']=='running'
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='cancelled' and row['completion_reason']=='claude_cancelled_before_prompt'
        assert inputs(trace)==[]
        assert not [r for r in wire(trace) if r.get('request',{}).get('subtype')=='interrupt']
        behavior.write_text('');receipt=submit(server)
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
    finally:server.close()

@pytest.mark.parametrize('failure',['malformed','init-error','init-exit'])
def test_protocol_and_initialization_faults_are_isolated(tmp_path,failure):
    role,skill,trace,behavior,agent=setup(tmp_path);behavior.write_text(failure)
    server=Server(tmp_path/'state',{'worker':agent,'peer':spec('worker')})
    try:
        fault=server.root/'providers/claude/worker/fault.json'
        if failure=='malformed':submit(server)
        until=time.monotonic()+8
        while not fault.exists() and time.monotonic()<until:time.sleep(.025)
        assert fault.exists()
        assert inputs(trace)==[]
        peer=submit(server,'peer');assert await_watch(server,peer['job_id'],lambda r:r['terminal'])['status']=='completed'
    finally:server.close()

def test_foreign_response_cannot_release_refresh(tmp_path):
    role,skill,trace,behavior,agent=setup(tmp_path);behavior.write_text('foreign')
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server);await_refresh(server,trace)
        assert inputs(trace)==[] and server.rpc('watch',{'target':receipt['job_id']})['status']=='running'
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
    finally:server.close()

def test_unowned_view_content_is_preserved_and_blocks_only_task(tmp_path):
    role,skill,trace,behavior,agent=setup(tmp_path)
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server);await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        marker=view(server)/'.claude/skills/user-file.txt';marker.write_text('preserve')
        receipt=submit(server)
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='failed' and row['completion_reason']=='role_context_invalid'
        assert marker.read_text()=='preserve' and len(inputs(trace))==1
        assert (skill/'note.txt').read_text()=='live reference'
    finally:server.close()

@pytest.mark.parametrize('component',['.claude-plugin/plugin.json','.mcp.json','hooks/hooks.json','settings.json'])
def test_role_plugin_components_are_not_activated_implicitly(tmp_path,component):
    role,skill,trace,behavior,agent=setup(tmp_path)
    target=skill/component;target.parent.mkdir(parents=True,exist_ok=True);target.write_text('{}')
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server)
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='failed' and row['completion_reason']=='role_context_invalid'
        assert inputs(trace)==[] and list((view(server)/'.claude/skills').iterdir())==[]
    finally:server.close()

def test_role_enabled_claude_lead_receives_worker_result_automatically(tmp_path):
    role,skill,trace,behavior,agent=setup(tmp_path,mode='lead')
    server=Server(tmp_path/'state',{'lead':agent,'worker':spec('worker')})
    try:
        receipt=submit(server,'lead')
        row=await_watch(server,receipt['job_id'],lambda r:r['visible_reply_source']=='message_bureau_reply')
        assert row['reply']=='Claude reviewed result' and len(inputs(trace))==2
        metadata=json.loads((server.root/'providers/claude/lead/session.json').read_text())
        assert metadata['state']['role_skills']['verified'] is True
    finally:server.close()


def test_saved_conversation_rebuilds_same_owned_aliases_after_restart(tmp_path):
    role,skill,trace,behavior,agent=setup(tmp_path);root=tmp_path/'state'
    server=Server(root,{'worker':agent})
    try:
        receipt=submit(server);await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        before=json.loads((root/'providers/claude/worker/session.json').read_text())
    finally:server.close()
    restored=Server(root,{'worker':agent})
    try:
        receipt=submit(restored)
        assert await_watch(restored,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        after=json.loads((root/'providers/claude/worker/session.json').read_text())
        assert before['state']['session_id']==after['state']['session_id']
        assert before['runtime_instance_id']!=after['runtime_instance_id']
        assert before['state']['role_skills']['bindings']==after['state']['role_skills']['bindings']
    finally:restored.close()


def test_user_only_skill_metadata_is_not_inserted_into_task_text(tmp_path):
    role,skill,trace,behavior,agent=setup(tmp_path)
    p=skill/'SKILL.md';p.write_text('---\nname: manual-only-secret-name\ndescription: Manual only.\ndisable-model-invocation: true\n---\nInstructions.')
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server)
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        text=inputs(trace)[0]['message']['content']
        assert 'manual-only-secret-name' not in text and 'ccb-role-' not in text
        link=next((view(server)/'.claude/skills').iterdir())
        assert (link/'SKILL.md').read_text()==p.read_text()
    finally:server.close()


@pytest.mark.skipif(os.name!='nt',reason='Installed Windows Claude probe')
@pytest.mark.parametrize('bare',[False,True])
def test_installed_claude_refreshes_rust_created_links_without_inference(tmp_path,bare):
    exe=Path('C:/Users/user/.local/bin/claude.exe')
    if not exe.is_file():pytest.skip('Claude not installed')
    role,skill,trace,behavior,agent=setup(tmp_path)
    # Bound ancestor discovery to this private fixture repository.
    initialized=subprocess.run(['git','init','-q',str(tmp_path)],capture_output=True,text=True,timeout=8)
    assert initialized.returncode==0,initialized.stderr
    server=Server(tmp_path/'state',{'worker':agent})
    proc=None
    try:
        receipt=submit(server);await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        alias=next((view(server)/'.claude/skills').iterdir()).name
        home=tmp_path/'cli-home';home.mkdir()
        env={k:v for k,v in os.environ.items() if k.upper() in ('PATH','SYSTEMROOT','WINDIR','COMSPEC','TEMP','TMP','APPDATA','LOCALAPPDATA')}
        env.update(CLAUDE_CONFIG_DIR=str(home),HOME=str(home),USERPROFILE=str(home),CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC='1')
        args=[str(exe),'-p','--verbose','--input-format','stream-json','--output-format','stream-json',
            '--no-session-persistence','--setting-sources','project','--strict-mcp-config','--mcp-config','{"mcpServers":{}}',
            '--settings','{"disableAllHooks":true}']
        if bare:args+=['--bare']
        args+=['--add-dir',str(view(server))]
        proc=subprocess.Popen(args,cwd=tmp_path,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,
            text=True,encoding='utf-8',creationflags=0x08000000)
        responses=queue.Queue()
        def read():
            for line in proc.stdout:responses.put(json.loads(line))
        threading.Thread(target=read,daemon=True).start()
        def rpc(kind):
            identity=str(uuid.uuid4())
            proc.stdin.write(json.dumps(dict(type='control_request',request_id=identity,request={'subtype':kind}))+'\n');proc.stdin.flush()
            until=time.monotonic()+15
            while time.monotonic()<until:
                row=responses.get(timeout=max(.01,until-time.monotonic()))
                if row.get('response',{}).get('request_id')==identity:
                    assert row['response']['subtype']=='success'
                    return row['response']['response']
            pytest.fail('Claude control response not observed')
        def matching():
            return [r for r in rpc('reload_skills')['skills'] if r['name']==alias or alias in r.get('aliases',[])]
        rpc('initialize')
        assert matching()[0]['name']=='reader'
        p=skill/'SKILL.md';p.write_text(p.read_text().replace('Read a sample document.','Fresh native description.'))
        assert matching()[0]['description'].startswith('Fresh native description.')
        p=role/'role.toml';p.write_text(p.read_text().replace('["skills/reader"]','[]'))
        receipt=submit(server);assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        assert matching()==[]
        assert (skill/'note.txt').read_text()=='live reference'
    finally:
        if proc is not None:
            proc.stdin.close()
            try:proc.wait(timeout=5)
            except subprocess.TimeoutExpired:proc.kill();proc.wait(timeout=5)
        server.close()
