"""Committed native restart configuration, without plaintext journal secrets."""
import json, os, time
import pytest
from test_native_daemon import Server, spec
from test_native_cli_security import cli
from test_native_reload import applied, config

pytestmark=pytest.mark.skipif(os.name!='nt',reason='Windows user-bound protected configuration')

def read(root):return json.loads((root/'configuration.json').read_text(encoding='utf-8'))
def stop(root):
    before=cli('--state-root',root,'ping');assert before.returncode==0,before.stderr
    pid=json.loads(before.stdout)['pid']
    result=cli('--state-root',root,'stop');assert result.returncode==0,result.stderr
    end=time.monotonic()+8
    while time.monotonic()<end:
        if cli('process-identity',pid).returncode!=0:return
        time.sleep(.025)
    pytest.fail('resumed test daemon did not exit')

def test_reload_publishes_protected_restart_config_and_resumes_without_input_file(tmp_path):
    root=tmp_path/'state';server=Server(root,{'worker':spec('worker')})
    marker='FIXTURE-RESTART-SECRET-NOT-FOR-JOURNAL'
    try:
        first=read(root)
        assert first['restart_checkpoint']['protection']=='windows-user-dpapi-v1'
        desired=config({'worker':{**spec('worker'),'env':{'CCB_TEST_SECRET':marker}},'peer':spec()})
        applied(server,desired)
        saved=read(root);fingerprint=server.rpc('ping')['config_fingerprint']
        assert saved['config_fingerprint']==fingerprint
        assert saved['restart_checkpoint']!=first['restart_checkpoint']
        assert marker.encode() not in (root/'configuration.json').read_bytes()
    finally:server.close()
    (root/'daemon.json').unlink()
    resumed=cli('--state-root',root,'start','--resume-config')
    assert resumed.returncode==0,resumed.stderr
    try:
        assert json.loads(resumed.stdout)['status']=='running'
        assert json.loads(cli('--state-root',root,'ping').stdout)['config_fingerprint']==fingerprint
        task=cli('--state-root',root,'ask','worker','continue after restart');assert task.returncode==0,task.stderr
        result=cli('--state-root',root,'watch',json.loads(task.stdout)['job_id'],'--wait','--timeout-ms','5000')
        assert result.returncode==0 and json.loads(result.stdout)['reply']=='worker result'
        assert marker not in resumed.stdout+resumed.stderr
    finally:stop(root)

def test_live_owner_is_not_restarted_or_its_checkpoint_rewritten(tmp_path):
    root=tmp_path/'state';server=Server(root,{'worker':spec()})
    try:
        before=(root/'configuration.json').read_bytes()
        result=cli('--state-root',root,'start','--resume-config')
        assert result.returncode!=0 and 'ownership' in result.stderr
        assert (root/'configuration.json').read_bytes()==before and server.proc.poll() is None
    finally:server.close()

@pytest.mark.parametrize('change',['ciphertext','legacy','owner-signature','project'])
def test_unverified_checkpoint_never_launches_a_daemon(tmp_path,change):
    root=tmp_path/'state';server=Server(root,{'worker':spec()});server.close()
    path=root/'configuration.json';row=read(root)
    if change=='ciphertext':row['restart_checkpoint']['ciphertext']='00'*256
    elif change=='legacy':row.pop('restart_checkpoint')
    elif change=='project':row['project_id']='foreign'
    else:
        lease=root/'lease.json';value=json.loads(lease.read_text(encoding='utf-8'));value['config_signature']='0'*64
        lease.write_text(json.dumps(value),encoding='utf-8')
    path.write_text(json.dumps(row),encoding='utf-8');before=path.read_bytes()
    result=cli('--state-root',root,'start','--resume-config')
    assert result.returncode!=0 and path.read_bytes()==before
    assert not (root/'kernel/native-start.json').exists()

def test_missing_state_and_conflicting_arguments_fail_before_creating_state(tmp_path):
    root=tmp_path/'absent'
    for extra in [[],['--config',tmp_path/'missing.json'],['--executables',tmp_path/'missing.json']]:
        result=cli('--state-root',root,'start','--resume-config',*extra)
        assert result.returncode!=0 and not root.exists()

def test_crash_during_reload_preparation_restores_committed_not_desired_config(tmp_path):
    import signal
    from native_kernel import _owner_stopped
    from test_native_reload import gated_worktree, reload, connect
    project,root,server,started,desired,entered,release=gated_worktree(tmp_path)
    running=True
    try:
        before=server.rpc('ping')['config_fingerprint'];operation=reload(server,desired)
        end=time.monotonic()+5
        while not entered.exists() and time.monotonic()<end:time.sleep(.02)
        assert entered.exists()
        os.kill(started['pid'],signal.SIGTERM)
        end=time.monotonic()+5
        while not _owner_stopped(started['pid']) and time.monotonic()<end:time.sleep(.02)
        assert _owner_stopped(started['pid']);running=False
        release.write_text('continue')
        # The source document still requests the uncommitted worktree change.
        result=cli('--state-root',root,'start','--resume-config');assert result.returncode==0,result.stderr
        running=True;restored=connect(root)
        assert restored.rpc('ping')['config_fingerprint']==before
        row=restored.rpc('reload-status')['reload']
        assert row['operation_id']==operation['operation_id'] and row['status']=='interrupted'
    finally:
        release.write_text('continue')
        if running:stop(root)
