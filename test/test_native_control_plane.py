"""Real loopback peers against the Rust listener; no user servers or Agents."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import socket
import subprocess
import sys

import pytest

from ccbd.control_plane_transport.token_auth import client_authenticate
from native_kernel import KernelClient

ROOT=Path(__file__).resolve().parents[1]
BIN=ROOT/'rust'/'target'/'debug'/('ccb-kernel.exe' if os.name=='nt' else 'ccb-kernel')

@pytest.fixture
def server(tmp_path,monkeypatch):
    monkeypatch.setenv('CCB_KERNEL_BIN',str(BIN))
    client=KernelClient(tmp_path/'state',transport='tcp')
    yield client
    client.close()

def request(server,op='ping',args=None,*,generation=None,token=None):
    with socket.create_connection(server._tcp_address,timeout=2) as stream:
        client_authenticate(stream,server._token.token if token is None else token)
        stream.sendall((json.dumps({'api_version':1,'id':41,'op':op,'args':args or {},
                                   'generation':server._token.generation if generation is None else generation})+'\n').encode())
        with stream.makefile('rb') as reader:
            return json.loads(reader.readline())

def test_rejects_foreign_tokens_and_stale_generations_without_stopping_server(server):
    before=server.call('ping')['pid']
    for options in [{'token':'wrong-token'},{'generation':'stale-generation'}]:
        with pytest.raises((OSError,ValueError)):
            request(server,**options)
    assert server.call('ping')['pid']==before

def test_slow_peer_does_not_block_authenticated_request(server):
    with socket.create_connection(server._tcp_address,timeout=2) as slow:
        slow.sendall(b'{')
        assert request(server)['result']['pid']==server.process.pid

def test_concurrent_requests_share_one_state_writer(server):
    def append(index):
        record={'schema_version':1,'record_type':'inbound_event_record','inbound_event_id':f'event-{index}',
                'agent_name':'worker','event_type':'task_request','message_id':f'message-{index}','attempt_id':None,
                'payload_ref':None,'priority':100,'status':'queued','created_at':'2026-10-11T00:00:00Z','started_at':None,'finished_at':None}
        return request(server,'inbox.append',{'agent_name':'worker','record':record})
    with ThreadPoolExecutor(max_workers=8) as workers:
        responses=list(workers.map(append,range(30)))
    assert all(row['ok'] for row in responses)
    rows=server.call('inbox.list',agent_name='worker')
    assert len(rows)==len({row['inbound_event_id'] for row in rows})==30

def test_attached_client_close_does_not_stop_owner(server):
    descriptor=json.loads((server.root/'kernel'/'control-plane.json').read_text())
    attached=KernelClient.attach(server.root,descriptor)
    assert attached.call('ping')['pid']==server.process.pid
    attached.close()
    assert server.call('ping')['pid']==server.process.pid

def test_separate_process_can_attach_without_a_second_writer(server):
    code="""import sys,json
sys.path.insert(0,sys.argv[1])
from native_kernel import KernelClient
from pathlib import Path
root=Path(sys.argv[2])
client=KernelClient.attach(root,json.loads((root/'kernel'/'control-plane.json').read_text()))
print(json.dumps(client.call('ping')))
client.close()
"""
    result=subprocess.run([sys.executable,'-c',code,str(ROOT/'lib'),str(server.root)],capture_output=True,text=True,timeout=10)
    assert result.returncode==0,result.stderr
    assert json.loads(result.stdout)['pid']==server.process.pid
    assert server.process.poll() is None

def test_creator_pipe_eof_releases_only_its_endpoint(server):
    descriptor_path=server.root/'kernel'/'control-plane.json'
    assert descriptor_path.is_file()
    server.process.stdin.close()
    assert server.process.wait(timeout=5)==0
    assert not descriptor_path.exists()

def test_late_owner_shutdown_preserves_new_generation_descriptor(server):
    path=server.root/'kernel'/'control-plane.json'
    descriptor=json.loads(path.read_text())
    descriptor['generation']='new-generation'
    path.write_text(json.dumps(descriptor))
    server.process.stdin.close()
    server.process.wait(timeout=5)
    assert json.loads(path.read_text())['generation']=='new-generation'


def test_recovery_requires_confirmed_owner_exit(tmp_path,monkeypatch):
    from native_kernel import kernel_for_layout,close_kernels
    from storage.paths import PathLayout
    monkeypatch.setenv('CCB_KERNEL_BIN',str(BIN))
    monkeypatch.setenv('CCB_CORE_RUNTIME','rust')
    layout=PathLayout(tmp_path/'repo')
    owner=kernel_for_layout(layout)
    before=owner.process.pid
    owner.process.kill()
    owner.process.wait(timeout=5)
    close_kernels()
    replacement=kernel_for_layout(layout)
    assert replacement.call('ping')['pid']!=before
    close_kernels()
