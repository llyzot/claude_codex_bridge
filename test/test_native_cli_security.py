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
    timeout=kwargs.pop('timeout',8)
    return subprocess.run([str(BIN),*map(str,args)],text=True,capture_output=True,encoding='utf-8',timeout=timeout,**kwargs)


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

def test_ask_with_message_arguments_ignores_a_stdin_that_never_closes(tmp_path):
    # Agent shells such as Claude Code's Bash tool hand children an open pipe.
    root=tmp_path/'state';root.mkdir()
    config=root/'native.json'
    config.write_text(json.dumps(dict(schema_version=2,project_id='native-cli',agents={'worker':spec()})),encoding='utf-8')
    process=subprocess.Popen([str(BIN),'--state-root',str(root),'daemon','--config',str(config)],
                             stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8',
                             creationflags=0x08000000 if os.name=='nt' else 0)
    asks=[]
    try:
        assert process.stdout.readline(),process.stderr.read()
        def ask(*args,stdin_text=None,close=False):
            child=subprocess.Popen([str(BIN),'--state-root',str(root),'ask','worker','--project','native-cli',*args],
                                   stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8')
            asks.append(child)
            if stdin_text is not None:child.stdin.write(stdin_text);child.stdin.flush()
            if close:child.stdin.close()
            started=time.monotonic();child.wait(timeout=8)
            return child,time.monotonic()-started
        held,elapsed=ask('--','argument','message',stdin_text='unrelated script text\n')
        assert held.returncode==0,held.stderr.read()
        assert elapsed<5
        piped,_=ask(stdin_text='﻿heredoc 消息\n',close=True)
        assert piped.returncode==0,piped.stderr.read()
        ids=[json.loads(c.stdout.read())['job_id'] for c in (held,piped)]
        bodies={}
        for line in (p for f in tmp_path.rglob('jobs.jsonl') for p in f.read_text(encoding='utf-8').splitlines()):
            row=json.loads(line);bodies[row['job_id']]=row['request']['body']
        assert bodies[ids[0]]=='argument message'
        assert bodies[ids[1]]=='heredoc 消息'
    finally:
        for child in asks:
            if child.poll() is None:child.kill()
            child.wait(timeout=3)
        cli('--state-root',root,'stop')
        try:process.wait(timeout=4)
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
