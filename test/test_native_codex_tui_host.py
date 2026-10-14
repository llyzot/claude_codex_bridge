"""Real installed Codex protocol, private empty homes, and no model turns."""
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import time

import pytest
from test_native_cli_security import BIN
from test_native_daemon import Server

CODEX = Path('C:/Users/user/scoop/apps/nodejs-lts/current/bin/node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe')
pytestmark = pytest.mark.skipif(os.name != 'nt' or not CODEX.is_file(), reason='Installed Windows Codex')

def wait_for(fn, timeout=20):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        result = fn()
        if result:
            return result
        time.sleep(.05)
    pytest.fail('Codex TUI host condition did not arrive')

@pytest.fixture
def host(tmp_path):
    home = tmp_path/'home'; home.mkdir()
    root = tmp_path/'host'
    env = dict(os.environ, CODEX_HOME=str(home))
    proc = subprocess.Popen([str(BIN), 'codex-tui-host', str(root), str(CODEX)], env=env,
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, encoding='utf-8', creationflags=0x08000000)
    output = queue.Queue()
    def read():
        for line in proc.stdout:
            output.put(json.loads(line))
        output.put(None)
    threading.Thread(target=read, daemon=True).start()
    def request(method, params, identity, expect_error=False):
        proc.stdin.write(json.dumps(dict(id=identity, method=method, params=params))+'\n'); proc.stdin.flush()
        deadline = time.monotonic()+20
        while time.monotonic() < deadline:
            row = output.get(timeout=max(.1, deadline-time.monotonic()))
            assert row is not None, proc.stderr.read()
            if row.get('id') == identity:
                if expect_error:
                    assert isinstance(row.get('error'),dict), row
                    return row['error']
                assert 'error' not in row, row
                return row['result']
        pytest.fail('RPC response missing')
    try:
        request('initialize', {'clientInfo':{'name':'ccb-host-test','version':'1'}}, 'init')
        proc.stdin.write('{"method":"initialized","params":{}}\n'); proc.stdin.flush()
        result = request('thread/start', {'ephemeral':True, 'cwd':str(tmp_path), 'approvalPolicy':'never', 'sandbox':'read-only'}, 'thread')
        state = json.loads((root/'host.json').read_text(encoding='utf-8'))
        assert state['thread_id'] == result['thread']['id']
        yield dict(proc=proc, root=root, env=env, request=request, state=state)
    finally:
        if proc.poll() is None:
            proc.stdin.close()
            try: proc.wait(timeout=5)
            except subprocess.TimeoutExpired: proc.kill(); proc.wait(timeout=5)
        assert not (home/'auth.json').exists(), 'No credential copy or login is allowed'

def test_actual_protocol_thread_and_eof_retire_server(host):
    p=host
    result=p['request']('thread/read', {'threadId':p['state']['thread_id'], 'includeTurns':False}, 'read')
    assert result['thread']['id']==p['state']['thread_id']
    p['proc'].stdin.close(); assert p['proc'].wait(timeout=5)==0
    assert json.loads((p['root']/'host.json').read_text(encoding='utf-8'))['status']=='stopped'
    pid=p['state']['server_process']['pid']
    wait_for(lambda:subprocess.run([str(BIN),'process-identity',str(pid)],capture_output=True).returncode!=0)


def test_actual_steer_rejects_unbound_turn_without_starting_a_model(host):
    p=host
    error=p['request']('turn/steer', {'threadId':p['state']['thread_id'], 'expectedTurnId':'ccb-nonexistent-turn',
                                    'input':[{'type':'text','text':'MUST_NOT_START_A_MODEL'}], 'clientUserMessageId':'fup_test'},
                       'steer-invalid-turn', expect_error=True)
    assert error['code'] != -32601, error
    assert 'turn' in error['message'].lower(), error
    result=p['request']('thread/read', {'threadId':p['state']['thread_id'],'includeTurns':False}, 'read-after-steer')
    assert result['thread']['status']['type'] == 'idle'

def test_wrong_token_cannot_queue_into_host(host):
    p=host
    r=subprocess.run([str(CODEX),'queue','--remote',p['state']['endpoint'],'--remote-auth-token-env','CCB_WRONG_TEST_TOKEN',
                      '--thread',p['state']['thread_id'],'--message','MUST_NOT_REACH_MODEL'],
                     env=dict(p['env'],CCB_WRONG_TEST_TOKEN='invalid-token'),capture_output=True,encoding='utf-8',timeout=15)
    assert r.returncode!=0
    assert '401' in r.stderr or 'unauthorized' in r.stderr.lower(), r.stderr
    assert p['proc'].poll() is None

def test_existing_host_is_not_replaced(host):
    p=host; before=(p['root']/'host.json').read_bytes()
    r=subprocess.run([str(BIN),'codex-tui-host',str(p['root']),str(CODEX)],env=p['env'],capture_output=True,timeout=5)
    assert r.returncode!=0 and (p['root']/'host.json').read_bytes()==before
    assert p['proc'].poll() is None

def test_malformed_frame_closes_host_without_terminal_fallback(host):
    p=host; p['proc'].stdin.write('not-json\n'); p['proc'].stdin.flush()
    assert p['proc'].wait(timeout=5)!=0
    assert json.loads((p['root']/'host.json').read_text(encoding='utf-8'))['status']=='stopped'

def test_host_process_death_retires_owned_app_server(host):
    p=host; p['proc'].kill(); p['proc'].wait(timeout=5)
    pid=p['state']['server_process']['pid']
    wait_for(lambda:subprocess.run([str(BIN),'process-identity',str(pid)],capture_output=True).returncode!=0)
    r=subprocess.run([str(BIN),'codex-tui-attach',str(p['root'])],capture_output=True,timeout=5)
    assert r.returncode!=0

def test_native_driver_uses_host_and_publishes_matching_thread(tmp_path):
    home=tmp_path/'home'; home.mkdir()
    spec=dict(provider='codex',driver_kind='codex_app_server',profile_family='protocol_turn',
              command=[str(CODEX),'app-server','--stdio'],workspace_path=str(tmp_path),
              tui_host_root=str(tmp_path/'hosts'),thread_options={'ephemeral':True},env={'CODEX_HOME':str(home)})
    server=Server(tmp_path/'state',{'worker':spec})
    try:
        path=server.root/'providers/codex/worker/session.json'
        wait_for(path.exists)
        session=json.loads(path.read_text(encoding='utf-8'))
        host=Path(session['state']['tui_host']['path'])
        state=json.loads((host/'host.json').read_text(encoding='utf-8'))
        assert state['host_process']['pid']==session['pid']
        assert state['thread_id']==session['state']['thread_id']
        assert session['state']['status']['type']=='idle'
    finally:server.close()
    wait_for(lambda:subprocess.run([str(BIN),'process-identity',str(state['server_process']['pid'])],capture_output=True).returncode!=0)


def test_real_host_preserves_global_instructions_for_interactive_attachment(tmp_path):
    home=tmp_path/'home';home.mkdir()
    (home/'config.toml').write_text('developer_instructions = "USER_GLOBAL_SENTINEL"\n',encoding='utf-8')
    spec=dict(provider='codex',driver_kind='codex_app_server',profile_family='protocol_turn',
              command=[str(CODEX),'app-server','--stdio'],workspace_path=str(tmp_path),
              tui_host_root=str(tmp_path/'hosts'),env={'CODEX_HOME':str(home)})
    server=Server(tmp_path/'state',{'codex':spec})
    try:
        path=server.root/'providers/codex/codex/session.json';wait_for(path.exists)
        session=json.loads(path.read_text(encoding='utf-8'))
        root=Path(session['state']['tui_host']['path'])
        host=json.loads((root/'host.json').read_text(encoding='utf-8'))
        instructions=json.loads((root/'developer-instructions.json').read_text(encoding='utf-8'))
        assert host['coordination_bound'] and instructions['thread_id']==session['state']['thread_id']
        assert instructions['text'].startswith('USER_GLOBAL_SENTINEL')
        assert '<ccb_interactive_coordination>' in instructions['text']
        assert not (home/'auth.json').exists()
    finally:server.close()

def test_installed_pi_accepts_canonical_windows_workspace(tmp_path):
    node=Path('C:/Users/user/scoop/apps/nodejs-lts/current/node.exe')
    pi=Path('C:/Users/user/scoop/apps/nodejs-lts/current/bin/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js')
    if not node.exists() or not pi.exists():pytest.skip('Installed Pi unavailable')
    home=tmp_path/'pi-home';home.mkdir()
    spec=dict(provider='pi',driver_kind='pi_rpc',profile_family='protocol_turn',
              command=[str(node),str(pi),'--mode','rpc','--no-extensions'],
              workspace_path='\\\\?\\'+str(tmp_path),env={'PI_CODING_AGENT_DIR':str(home)})
    server=Server(tmp_path/'state',{'lead':spec})
    try:
        path=server.root/'providers/pi/lead/session.json'
        wait_for(path.exists)
        state=json.loads(path.read_text(encoding='utf-8'))['state']
        assert state.get('sessionId'),state
        assert '?' not in state['sessionFile'],state
    finally:server.close()

def test_clear_persists_hosted_thread_for_immediate_display_resume(tmp_path):
    from test_native_context import control,settled
    home=tmp_path/'home';home.mkdir()
    spec=dict(provider='codex',driver_kind='codex_app_server',profile_family='protocol_turn',
              command=[str(CODEX),'app-server','--stdio'],workspace_path=str(tmp_path),
              tui_host_root=str(tmp_path/'hosts'),env={'CODEX_HOME':str(home)})
    server=Server(tmp_path/'state',{'worker':spec})
    try:
        path=server.root/'providers/codex/worker/session.json';wait_for(path.exists)
        before=json.loads(path.read_text(encoding='utf-8'))
        result=settled(server,control(server),timeout=20);assert result['status']=='completed',result
        after=json.loads(path.read_text(encoding='utf-8'))
        assert after['pid']==before['pid'] and after['state']['thread_id']!=before['state']['thread_id']
        assert Path(before['state']['path']).is_file()
        assert Path(after['state']['path']).is_file(), 'Clear succeeded but its new thread cannot be resumed by a TUI'
        host=json.loads((Path(after['state']['tui_host']['path'])/'host.json').read_text(encoding='utf-8'))
        assert host['thread_id']==after['state']['thread_id']
    finally:server.close()
    restored=Server(tmp_path/'state',{'worker':spec})
    try:
        def resumed():
            value=json.loads(path.read_text(encoding='utf-8'))
            return value if value['runtime_instance_id']!=after['runtime_instance_id'] else None
        resumed_state=wait_for(resumed)
        assert resumed_state['state']['thread_id']==after['state']['thread_id']
    finally:restored.close()
