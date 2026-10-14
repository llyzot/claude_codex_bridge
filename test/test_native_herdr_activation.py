"""Independent registration, session activation and durable reply delivery."""
import json
import os
import subprocess
import shutil
from pathlib import Path
import pytest
from test_native_herdr_agents import change
from test_native_pi_terminal import HOST
from test_native_herdr_open import NODE
from test_native_herdr_namespace import setup
from test_native_herdr_open import project, cli, read
from test_native_herdr_worker import wait_for
from test_native_herdr_work_recovery import finish_worker


def test_plain_reply_waits_for_offline_lead_without_blocking_worker(project):
    p=project
    (p['native']/'pi-terminal-events/lead/current.json').unlink()
    assert p['open'](0).returncode==0
    wait_for(p['controller_ready'],timeout=20)
    def worker_ready():
        result=cli('--state-root',p['native'],'agents')
        if result.returncode:return None
        actors=json.loads(result.stdout)['agents']
        assert {a['agent_name'] for a in actors}=={'lead','worker'}
        return next(a for a in actors if a['agent_name']=='worker')['activation']['session_verified']
    wait_for(worker_ready,timeout=20)
    result=cli('--state-root',p['native'],'ask','--from','lead','worker','independent worker result')
    assert result.returncode==0,result.stderr
    task=json.loads(result.stdout)['job_id']
    prompt=wait_for(lambda:p['prompts']())[-1]
    finish_worker(p,prompt['body'],'durable result for absent lead','activation-turn')
    p['idle']('worker')
    wait_for(lambda:json.loads(cli('--state-root',p['native'],'watch',task).stdout)['status']=='completed')
    def reply_pending():
        state=json.loads(cli('--state-root',p['native'],'queue','--detail','lead').stdout)
        return state['agent']['pending_reply_count']>=1
    wait_for(reply_pending)
    assert len(p['prompts']())==1
    actors=json.loads(cli('--state-root',p['native'],'agents').stdout)['agents']
    assert next(a for a in actors if a['agent_name']=='lead')['activation']['session_verified'] is False
    before=read(p['native']/'lease.json')['process']
    # Publishing a verified session changes runtime capability, not config.
    p['command'](action='reload')
    followup=wait_for(lambda:p['prompts']()[1] if len(p['prompts']())>=2 else None,timeout=20)
    assert 'durable result for absent lead' in followup['body']
    p['command'](action='prompt',prompt=followup['body'],reply='reviewed automatically')
    p['idle']('lead')
    wait_for(lambda:json.loads(cli('--state-root',p['native'],'queue','--detail','lead').stdout)['agent']['pending_reply_count']==0)
    assert p['open']().returncode==0
    assert read(p['native']/'lease.json')['process']==before
    assert not (p['native']/'reload.json').exists()
    assert len(p['prompts']())==2


@pytest.mark.skipif(not os.environ.get('CCB_PREVIOUS_NATIVE_BIN'),reason='Explicit previous-release migration probe')
def test_previous_release_checkpoint_migrates_without_replacing_native_sessions(project):
    from test_native_cli_security import BIN
    p=project;previous=Path(os.environ['CCB_PREVIOUS_NATIVE_BIN']).parent
    # Model the supported installation operation: replace executables in place.
    # Launching the new version from another directory also changes injected PATH
    # and correctly trips the substantive-configuration fence.
    program=p['root']/'program';program.mkdir()
    executable=program/'ccb-native.exe'
    shutil.copy2(previous/'ccb-native.exe',executable)
    def open_version():
        return subprocess.run([str(executable),'--project',str(p['root']),'herdr','open','--options',str(p['file']),'--no-attach'],capture_output=True,text=True,encoding='utf-8',timeout=25)
    original=json.loads(p['pi'].args[2]);history=Path(original['native_session_path'])
    history.write_text(json.dumps(dict(type='session',id=original['session_id'],cwd=str(p['root'])))+'\n',encoding='utf-8')
    before=history.read_bytes()
    opened=open_version()
    assert opened.returncode==0,opened.stderr
    wait_for(p['ready'],timeout=30)
    wait_for(lambda:read(p['native']/'herdr/herdr-open-start.json')['status']=='finished')
    retired=subprocess.run([str(executable),'--state-root',str(p['native']),'kill'],capture_output=True,text=True,encoding='utf-8',timeout=25)
    assert retired.returncode==0,retired.stderr
    for proc in [p['pi'],p['codex']]:proc.kill();proc.wait(timeout=5)
    shutil.copy2(BIN,executable)
    gate=p['root']/'migration-session-ready';original['start_gate']=str(gate)
    pi=subprocess.Popen([NODE,str(HOST),json.dumps(original)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8',creationflags=0x08000000)
    worker=subprocess.Popen(p['codex'].args,creationflags=0x08000000)
    try:
        change(p['backend'],lambda s:s.update(provider_pids=[pi.pid,worker.pid]))
        result=open_version();assert result.returncode==0,result.stderr
        wait_for(lambda:(p['native']/'herdr/herdr-recovery.json').exists() and read(p['native']/'herdr/herdr-recovery.json')['phase']=='reset',timeout=20)
        gate.touch();assert json.loads(pi.stdout.readline())['ready']
        wait_for(p['ready'],timeout=30)
        actors=json.loads(cli('--state-root',p['native'],'agents').stdout)['agents']
        wait_for(lambda:all(a['activation']['session_verified'] for a in json.loads(cli('--state-root',p['native'],'agents').stdout)['agents']))
        assert {a['agent_name'] for a in actors}=={'lead','worker'}
        assert history.read_bytes()==before
        assert read(p['native']/'herdr/herdr-recovery.json')['sessions']['lead']['session_id']==original['session_id']
    finally:
        for proc in [pi,worker]:
            if proc.poll() is None:proc.kill()
            proc.wait(timeout=5)
