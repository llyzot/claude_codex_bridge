"""Native role roots must be registered and verified before Codex task input."""
import json
from pathlib import Path
import time
import os
import queue
import subprocess
import threading

import pytest
from test_native_daemon import Server, submit, await_watch, spec
from test_native_codex_rpc import codex_spec
from test_native_role_memory import install
from test_native_pi_rpc import pi_spec

def setup(project, preview=False):
    role = install(project, 'agent-role/preview-0.1' if preview else 'rolepack/v1')
    skill = role / 'skills/reader'
    skill.mkdir(parents=True)
    (skill / 'SKILL.md').write_text('---\nname: reader\ndescription: Read a sample document.\n---\nRead ./note.txt.\n', encoding='utf-8')
    (skill / 'note.txt').write_text('live reference', encoding='utf-8')
    manifest = role / 'role.toml'
    text = manifest.read_text(encoding='utf-8')
    if preview: text += 'skills=["skills/reader"]\n'
    else: text += '\n[skills]\ncodex=["skills/reader"]\n'
    manifest.write_text(text, encoding='utf-8')
    trace = project / 'wire.jsonl'
    behavior = project / 'behavior.txt'
    agent = dict(codex_spec(), role='agentroles.reader', workspace_path=str(project),
                 env={'CCB_FIXTURE_TRACE':str(trace),'CCB_SKILL_BEHAVIOR':str(behavior)})
    return role, skill, trace, behavior, agent

def wire(path):
    if not path.exists(): return []
    return [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines()]

def await_method(server, trace, method):
    deadline = time.monotonic()+8
    while time.monotonic()<deadline:
        rows = [r for r in wire(trace) if r.get('method')==method]
        if rows: return rows
        assert server.proc.poll() is None
        time.sleep(.025)
    pytest.fail(f'{method} was not observed')

@pytest.mark.parametrize('preview', [False, True])
def test_live_roots_are_registered_verified_and_removed_on_same_session(tmp_path, preview):
    role, skill, trace, behavior, agent = setup(tmp_path, preview)
    server = Server(tmp_path/'state', {'worker':agent})
    try:
        before = None
        for iteration in range(3):
            if iteration == 1:
                (skill/'SKILL.md').write_text('---\nname: reader\ndescription: Updated description.\n---\nNew body\n')
            if iteration == 2:
                p = role/'role.toml'
                p.write_text(p.read_text().replace('["skills/reader"]','[]'))
            receipt = submit(server)
            row = await_watch(server, receipt['job_id'], lambda r:r['terminal'])
            assert row['status']=='completed', row
            calls = [r for r in wire(trace) if r.get('method') in {'skills/extraRoots/set','skills/list','turn/start'}]
            assert [r['method'] for r in calls] == ['skills/extraRoots/set','skills/list','turn/start']*(iteration+1)
            roots = calls[-3]['params']['extraRoots']
            assert roots==[] if iteration==2 else len(roots)==1 and Path(roots[0]).samefile(skill)
            assert calls[-2]['params']['forceReload'] is True
            state = json.loads((server.root/'providers/codex/worker/session.json').read_text())
            assert state['state']['role_skills']['verified'] is True
            assert state['state']['role_skills']['job_id'] == receipt['job_id']
            if before: assert before['runtime_instance_id']==state['runtime_instance_id']
            before=state
        assert not (tmp_path/'.agents').exists()
        assert not (tmp_path/'.codex').exists()
    finally: server.close()

@pytest.mark.parametrize('failure', ['reject-set','reject-list','missing','disabled','wrong-cwd','malformed'])
def test_rejected_or_unverified_skills_never_send_task_and_can_recover(tmp_path, failure):
    role, skill, trace, behavior, agent = setup(tmp_path)
    behavior.write_text(failure)
    server = Server(tmp_path/'state', {'worker':agent,'peer':spec('worker')})
    try:
        receipt = submit(server)
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='failed' and row['completion_reason'].startswith('codex_role_skills_'), row
        assert not [r for r in wire(trace) if r.get('method')=='turn/start']
        jobs=server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result']
        assert len(jobs)==1 and jobs[0]['terminal_decision']['diagnostics']['provider_prompt_sent'] is False
        assert 'SECRET_SKILL_ERROR' not in json.dumps(jobs)
        peer=submit(server,'peer')
        assert await_watch(server,peer['job_id'],lambda r:r['terminal'])['status']=='completed'
        before=json.loads((server.root/'providers/codex/worker/session.json').read_text())
        behavior.write_text('')
        receipt=submit(server)
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        after=json.loads((server.root/'providers/codex/worker/session.json').read_text())
        assert before['runtime_instance_id']==after['runtime_instance_id']
    finally: server.close()

@pytest.mark.parametrize('phase', ['set','list'])
def test_silent_cancel_during_preparation_waits_for_ack_and_never_starts_turn(tmp_path, phase):
    role,skill,trace,behavior,agent=setup(tmp_path)
    behavior.write_text('delay-'+phase)
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server)
        await_method(server,trace,'skills/extraRoots/set' if phase=='set' else 'skills/list')
        server.rpc('cancel',dict(job_id=receipt['job_id'],record_reply=False))
        row=server.rpc('watch',{'target':receipt['job_id']})
        assert row['status']=='running'
        behavior.write_text('')
        second=submit(server)
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='cancelled' and row['completion_reason']=='codex_cancelled_before_turn'
        assert await_watch(server,second['job_id'],lambda r:r['terminal'])['status']=='completed'
        turns=[r for r in wire(trace) if r.get('method')=='turn/start']
        assert len(turns)==1 and second['job_id'] in turns[0]['params']['input'][0]['text']
        replies=server.rpc('records.list',{'stream':'replies'},version=1)['result']
        assert len(replies)==1
    finally: server.close()

def test_foreign_list_response_and_old_turn_completion_do_not_release_preparation(tmp_path):
    role,skill,trace,behavior,agent=setup(tmp_path)
    behavior.write_text('foreign')
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server)
        await_method(server,trace,'skills/list')
        assert not [r for r in wire(trace) if r.get('method')=='turn/start']
        assert server.rpc('watch',{'target':receipt['job_id']})['status']=='running'
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
    finally: server.close()

@pytest.mark.parametrize('kind', ['missing-directory','missing-skill','escape','bad-list','too-many','wrong-provider'])
def test_role_skill_declarations_are_scoped_and_validated(tmp_path,kind):
    role,skill,trace,behavior,agent=setup(tmp_path)
    p=role/'role.toml'
    text=p.read_text()
    if kind=='missing-directory':text=text.replace('skills/reader','skills/missing')
    elif kind=='missing-skill':(skill/'SKILL.md').unlink()
    elif kind=='escape':text=text.replace('skills/reader','../escape')
    elif kind=='bad-list':text=text.replace('["skills/reader"]','"skills/reader"')
    elif kind=='too-many':text=text.replace('["skills/reader"]',json.dumps(['skills/reader']*65))
    elif kind=='wrong-provider':text=text.replace('codex=[','claude=[')
    p.write_text(text)
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server)
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        if kind=='wrong-provider':assert row['status']=='completed'
        else:assert row['status']=='failed' and row['completion_reason']=='role_context_invalid'
        assert not [r for r in wire(trace) if r.get('method')=='skills/extraRoots/set']
    finally: server.close()


@pytest.mark.parametrize('phase', ['set','list'])
def test_deadline_during_skill_preparation_cancels_without_model_input(tmp_path,phase):
    role,skill,trace,behavior,agent=setup(tmp_path)
    behavior.write_text('delay-'+phase)
    agent['timeout_s']=.15
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server)
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='cancelled' and row['completion_reason']=='codex_cancelled_before_turn'
        assert not [r for r in wire(trace) if r.get('method')=='turn/start']
        jobs=server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result']
        assert jobs[0]['terminal_decision']['diagnostics']['deadline_triggered'] is True
    finally: server.close()


def test_invalid_response_envelope_quarantines_unconfirmed_preparation(tmp_path):
    role,skill,trace,behavior,agent=setup(tmp_path)
    behavior.write_text('bad-envelope')
    server=Server(tmp_path/'state',{'worker':agent,'peer':spec('worker')})
    try:
        receipt=submit(server)
        await_method(server,trace,'skills/extraRoots/set')
        deadline=time.monotonic()+8
        fault=server.root/'providers/codex/worker/fault.json'
        while not fault.exists() and time.monotonic()<deadline:time.sleep(.025)
        assert fault.exists()
        assert server.rpc('watch',{'target':receipt['job_id']})['status']=='running'
        assert not [r for r in wire(trace) if r.get('method')=='turn/start']
        peer=submit(server,'peer')
        assert await_watch(server,peer['job_id'],lambda r:r['terminal'])['status']=='completed'
    finally: server.close()


def test_preview_adapter_preserves_order_and_clears_filtered_skills(tmp_path):
    role,skill,trace,behavior,agent=setup(tmp_path,True)
    extra=role/'skills/extra';extra.mkdir()
    (extra/'SKILL.md').write_text('---\nname: extra\ndescription: Another skill.\n---\nExtra body.')
    adapter=role/'adapters/ccb/adapter.toml'
    adapter.write_text('supported_providers=["codex"]\nskills=["skills/extra"]\n')
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        receipt=submit(server)
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        roots=[r for r in wire(trace) if r.get('method')=='skills/extraRoots/set'][-1]['params']['extraRoots']
        assert len(roots)==2 and Path(roots[0]).samefile(skill) and Path(roots[1]).samefile(extra)
        adapter.write_text('supported_providers=["claude"]\nskills=["skills/extra"]\n')
        receipt=submit(server)
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        assert [r for r in wire(trace) if r.get('method')=='skills/extraRoots/set'][-1]['params']['extraRoots']==[]
    finally: server.close()


@pytest.mark.skipif(os.name!='nt',reason='Installed Windows Codex protocol probe')
def test_installed_codex_skill_catalog_reloads_and_removes_live_roots_without_model(tmp_path):
    exe=Path('C:/Users/user/scoop/apps/nodejs-lts/current/bin/node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe')
    if not exe.is_file():pytest.skip('Installed Codex not present')
    role,skill,trace,behavior,agent=setup(tmp_path)
    home=tmp_path/'probe-home';home.mkdir()
    env=dict(os.environ,CODEX_HOME=str(home),HOME=str(home),USERPROFILE=str(home))
    proc=subprocess.Popen([str(exe),'app-server','--stdio'],cwd=tmp_path,env=env,
        stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,
        text=True,encoding='utf-8',creationflags=0x08000000)
    responses=queue.Queue()
    def read():
        for line in proc.stdout:responses.put(json.loads(line))
    threading.Thread(target=read,daemon=True).start()
    def send(value):proc.stdin.write(json.dumps(value)+'\n');proc.stdin.flush()
    def rpc(identity,method,params):
        send(dict(id=identity,method=method,params=params))
        deadline=time.monotonic()+12
        while time.monotonic()<deadline:
            result=responses.get(timeout=max(.01,deadline-time.monotonic()))
            if result.get('id')==identity:
                assert 'error' not in result, result
                return result['result']
        pytest.fail(f'{method} response not observed')
    def discovered(identity):
        result=rpc(identity,'skills/list',dict(cwds=[str(tmp_path)],forceReload=True))
        return [s for row in result['data'] for s in row['skills'] if Path(s['path']).samefile(skill/'SKILL.md')]
    try:
        rpc(1,'initialize',dict(clientInfo={'name':'ccb-skill-test','version':'1'},capabilities={'experimentalApi':False}))
        send(dict(method='initialized'))
        rpc(2,'skills/extraRoots/set',dict(extraRoots=[str(skill)]))
        assert discovered(3)[0]['description']=='Read a sample document.'
        path=skill/'SKILL.md'
        path.write_text(path.read_text().replace('Read a sample document.','Description changed live.'))
        assert discovered(4)[0]['description']=='Description changed live.'
        rpc(5,'skills/extraRoots/set',dict(extraRoots=[]))
        assert discovered(6)==[]
        assert not (home/'config.toml').exists()
    finally:
        proc.stdin.close()
        try:proc.wait(timeout=5)
        except subprocess.TimeoutExpired:proc.kill();proc.wait(timeout=5)


def test_codex_role_skills_preserve_automatic_pi_lead_callback(tmp_path):
    role,skill,trace,behavior,agent=setup(tmp_path)
    server=Server(tmp_path/'state',{'lead':pi_spec('lead'),'worker':agent})
    try:
        receipt=submit(server,'lead')
        row=await_watch(server,receipt['job_id'],lambda r:r['visible_reply_source']=='message_bureau_reply')
        assert row['reply']=='persistent Pi reviewed result'
        edge=server.rpc('records.latest',dict(stream='callbacks',identity='edge_id'),version=1)['result']
        assert len(edge)==1 and edge[0]['state']=='done'
        metadata=json.loads((server.root/'providers/pi/lead/session.json').read_text())
        assert metadata['state']['messageCount']==4
        methods=[r['method'] for r in wire(trace) if r.get('method') in {'skills/extraRoots/set','skills/list','turn/start'}]
        assert methods==['skills/extraRoots/set','skills/list','turn/start']
    finally: server.close()
