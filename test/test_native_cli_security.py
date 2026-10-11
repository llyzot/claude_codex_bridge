"""Native CLI/security against real files and isolated owned processes."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

import pytest
from test_native_daemon import Server,spec,submit,STUB
from native_kernel import _owner_stopped

ROOT=Path(__file__).resolve().parents[1]
BIN=Path(os.environ.get('CCB_TEST_NATIVE_BIN') or ROOT/'rust'/'target'/'debug'/('ccb-native.exe' if os.name=='nt' else 'ccb-native'))

def cli(*args,**kwargs):
    return subprocess.run([str(BIN),*map(str,args)],text=True,capture_output=True,encoding='utf-8',timeout=8,**kwargs)


@pytest.mark.parametrize('flag',['--help','--version','--print-version','--capabilities'])
def test_launcher_forwarded_ask_metadata_needs_no_project_or_daemon(tmp_path,flag):
    result=cli('ask',flag,cwd=tmp_path)
    assert result.returncode==0,result.stderr
    assert not (tmp_path/'.ccb').exists()
    if flag=='--capabilities':assert json.loads(result.stdout)['service']=='ccb-native'
    else:assert 'ccb-native' in result.stdout


def test_forwarded_ask_capabilities_preserves_argument_validation(tmp_path):
    result=cli('ask','--capabilities','unexpected',cwd=tmp_path)
    assert result.returncode!=0 and 'takes no arguments' in result.stderr
    assert 'project' not in result.stderr

def test_native_token_is_private_and_create_new_does_not_delete_existing(tmp_path):
    path=tmp_path/'private'/'token.json'
    result=cli('token-create',path)
    assert result.returncode==0,result.stderr
    metadata=json.loads(result.stdout)
    content=json.loads(path.read_text(encoding='utf-8'))
    assert 'token' not in metadata
    assert len(content['token'])==64
    assert metadata['generation']==content['generation']
    before=path.read_bytes()
    failed=cli('token-create',path)
    assert failed.returncode!=0
    assert path.read_bytes()==before
    if os.name=='nt':
        from ccbd.control_plane_transport.token_auth import _read_windows_acl_proof,_current_windows_sid
        proof=_read_windows_acl_proof(path,command_runner=subprocess.run)
        assert len(proof['access'])==1
        assert not proof['access'][0]['inherited']
        assert 'Write' not in proof['access'][0]['rights']
        assert _current_windows_sid(subprocess.run) in proof['sddl']
    else:assert path.stat().st_mode & 0o077==0

def test_cli_starts_connects_and_stops_daemon_without_python_core(tmp_path):
    root=tmp_path/'state';root.mkdir()
    config=root/'native.json'
    config.write_text(json.dumps(dict(schema_version=2,project_id='native-cli',agents={'worker':spec()})),encoding='utf-8')
    process=subprocess.Popen([str(BIN),'--state-root',str(root),'daemon','--config',str(config)],
                             stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8',
                             creationflags=0x08000000 if os.name=='nt' else 0)
    try:
        line=process.stdout.readline()
        assert line,process.stderr.read()
        ping=cli('--state-root',root,'ping');assert ping.returncode==0,ping.stderr
        assert json.loads(ping.stdout)['runtime']=='rust'
        receipt=cli('--state-root',root,'ask','worker','--project','native-cli','hello','world')
        assert receipt.returncode==0,receipt.stderr
        id=json.loads(receipt.stdout)['job_id']
        watched=cli('--state-root',root,'watch',id)
        assert watched.returncode==0,watched.stderr
        assert json.loads(watched.stdout)['status']=='accepted'
        stopped=cli('--state-root',root,'stop');assert stopped.returncode==0,stopped.stderr
        process.wait(timeout=4)
        assert process.returncode==0,process.stderr.read()
    finally:
        if process.poll() is None:process.kill();process.wait(timeout=3)

@pytest.mark.skipif(os.name!='nt',reason='Windows job-object lifetime contract')
def test_daemon_death_stops_owned_descendants(tmp_path):
    pid_file=tmp_path/'descendant.pid'
    config=spec('descendant');config['command'].append(str(pid_file))
    server=Server(tmp_path/'state',{'worker':config})
    pid=None
    try:
        submit(server)
        deadline=time.monotonic()+5
        while not pid_file.exists() and time.monotonic()<deadline:time.sleep(.025)
        assert pid_file.exists()
        pid=int(pid_file.read_text())
        assert not _owner_stopped(pid)
        server.proc.kill();server.proc.wait(timeout=3)
        deadline=time.monotonic()+3
        while not _owner_stopped(pid) and time.monotonic()<deadline:time.sleep(.025)
        assert _owner_stopped(pid)
    finally:
        if server.proc.poll() is None:server.proc.kill();server.proc.wait(timeout=3)
        # The asserted owned PID is not used for cleanup/restart if uncertain.
