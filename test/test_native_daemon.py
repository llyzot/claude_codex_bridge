"""A standalone Rust actor, real TCP clients and controlled child processes."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time

import pytest
from ccbd.control_plane_transport.token_auth import create_token_file,client_authenticate

ROOT=Path(__file__).resolve().parents[1]
BIN=ROOT/'rust'/'target'/'debug'/('ccb-kernel.exe' if os.name=='nt' else 'ccb-kernel')
BIN=Path(os.environ.get('CCB_TEST_KERNEL_BIN') or BIN)
STUB=ROOT/'test'/'stubs'/'native_command_worker.py'

class Server:
    def __init__(self,root,agents,*,standalone=False):
        self.root=root
        self.token=create_token_file(root/'kernel'/f'token-{time.monotonic_ns()}.json')
        config=root/'daemon.json'
        config.write_text(json.dumps(dict(schema_version=2,project_id='native-test',agents=agents)),encoding='utf-8')
        args=[str(BIN),'--state-root',str(root),'--token-file',self.token.token_ref,'--daemon-config',str(config)]
        if standalone:args+=['--standalone']
        self.proc=subprocess.Popen(args,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                                   text=True,encoding='utf-8',creationflags=0x08000000 if os.name=='nt' else 0)
        line=self.proc.stdout.readline()
        assert line, self.proc.stderr.read()
        self.endpoint=json.loads(line)
    def rpc(self,op,request=None,*,version=2):
        with socket.create_connection((self.endpoint['host'],self.endpoint['port']),timeout=3) as stream:
            client_authenticate(stream,self.token.token)
            envelope=dict(api_version=version,op=op,request=request or {})
            if version==1:envelope=dict(api_version=1,id=1,op=op,args=request or {},generation=self.token.generation)
            stream.sendall((json.dumps(envelope)+'\n').encode())
            with stream.makefile('rb') as reader:result=json.loads(reader.readline())
        assert result['ok'], result
        return result
    def close(self):
        if self.proc.poll() is None:
            try:self.rpc('shutdown')
            except (OSError,ValueError,AssertionError):pass
            try:self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:self.proc.kill();self.proc.wait(timeout=3)
        assert self.proc.returncode==0, self.proc.stderr.read()

def spec(mode=None):
    return dict(provider='test',profile_family='structured_result',command=[sys.executable,'-I',str(STUB),mode] if mode else None)

@pytest.fixture
def servers(tmp_path):
    active=[]
    def create(agents,**options):
        root=tmp_path/f'runtime-{len(active)}'/'ccbd'
        server=Server(root,agents,**options)
        active.append(server)
        return server
    yield create
    for server in active:server.close()

def submit(server,target='worker',actor='user',body='task'):
    return server.rpc('submit',dict(project_id='native-test',to_agent=target,from_actor=actor,body=body))

def await_watch(server,job,predicate,timeout=8):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        row=server.rpc('watch',{'target':job})
        if predicate(row):return row
        assert server.proc.poll() is None, server.proc.stderr.read()
        time.sleep(.025)
    pytest.fail(f'job did not reach expected state: {row}')

def test_submit_returns_before_command_and_slow_stdin_does_not_block_ping(servers):
    server=servers({'worker':spec('slow-input')})
    begin=time.monotonic();receipt=submit(server,body='x'*4090)
    assert time.monotonic()-begin<.6
    begin=time.monotonic();assert server.rpc('ping')['runtime']=='rust'
    assert time.monotonic()-begin<.6
    row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
    assert row['status']=='completed'
    assert row['reply']=='worker result'

def test_worker_finish_automatically_submits_and_executes_lead_continuation(servers):
    server=servers({'lead':spec('lead'),'worker':spec('worker')})
    receipt=submit(server,'lead')
    row=await_watch(server,receipt['job_id'],lambda r:r['visible_reply_source']=='message_bureau_reply')
    assert row['reply']=='reviewed: worker result'
    edges=server.rpc('records.latest',dict(stream='callbacks',identity='edge_id'),version=1)['result']
    assert len(edges)==1 and edges[0]['state']=='done'
    assert edges[0]['continuation_job_id']
    replies=server.rpc('records.list',{'stream':'replies'},version=1)['result']
    assert len(replies)==2

def test_retry_is_scheduled_and_completed_without_python_core(servers):
    server=servers({'worker':spec('retry-once')})
    receipt=submit(server)
    attempts=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
    deadline=time.monotonic()+8
    while time.monotonic()<deadline:
        queue=server.rpc('queue',{'target':'worker'})['queues'][0]
        rows=server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result']
        if len(rows)==2 and rows[-1]['status']=='completed':break
        time.sleep(.025)
    assert len(rows)==2 and rows[-1]['terminal_decision']['reply']=='retry succeeded'
    replies=server.rpc('records.list',{'stream':'replies'},version=1)['result']
    assert len(replies)==1 and replies[0]['reply']=='retry succeeded'

def test_ordinary_reply_delivery_wakes_idle_receiver_and_consumes_fifo_head(servers):
    server=servers({'lead':spec('lead'),'worker':spec('worker')})
    receipt=submit(server,'worker','lead')
    await_watch(server,receipt['job_id'],lambda r:r['terminal'])
    deadline=time.monotonic()+8
    while time.monotonic()<deadline:
        rows=server.rpc('records.latest',dict(stream='jobs',agent_name='lead',identity='job_id'),version=1)['result']
        if rows and rows[-1]['status']=='completed':break
        time.sleep(.025)
    assert len(rows)==1 and rows[0]['request']['message_type']=='reply_delivery'
    assert rows[0]['terminal_decision']['reply'].startswith('received: CCB_REPLY')
    assert server.rpc('mailbox-head',{'agent_name':'lead'})['head'] is None

def test_foreign_completion_does_not_become_task_evidence(servers):
    server=servers({'worker':spec('foreign')})
    receipt=submit(server)
    row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
    assert row['status']=='incomplete'
    assert row['completion_reason']=='missing_structured_result'
    assert row['reply']!='worker result'

def test_standalone_survives_creator_pipe_eof_and_stops_by_authenticated_request(servers):
    server=servers({'worker':spec()},standalone=True)
    server.proc.stdin.close()
    assert server.rpc('ping')['runtime']=='rust'
    server.rpc('shutdown')
    server.proc.wait(timeout=3)
    lease=json.loads((server.root/'lease.json').read_text(encoding='utf-8'))
    assert lease['mount_state']=='unmounted'

def test_manual_work_requires_current_execution_instance(servers):
    server=servers({'worker':spec()})
    receipt=submit(server)
    job=server.rpc('work',{'agent_name':'worker'})['job']
    assert job['job_id']==receipt['job_id']
    with pytest.raises(AssertionError,match='instance mismatch'):
        server.rpc('complete',dict(job_id=job['job_id'],agent_name='worker',execution_instance_id='foreign',decision={}))
    assert server.rpc('watch',{'target':job['job_id']})['status']=='running'


def test_live_prior_owner_is_fenced_before_journal_recovery(tmp_path):
    root=tmp_path/'existing-root';(root/'kernel').mkdir(parents=True)
    lease=dict(schema_version=2,record_type='ccbd_lease',api_version=2,project_id='native-test',
               ccbd_pid=os.getpid(),mount_state='mounted',generation=1,daemon_instance_id='existing')
    (root/'lease.json').write_text(json.dumps(lease),encoding='utf-8')
    pending=b'not-a-valid-journal'
    (root/'kernel'/'pending.json').write_bytes(pending)
    with pytest.raises(AssertionError,match='ownership is live'):
        Server(root,{'worker':spec()})
    assert json.loads((root/'lease.json').read_text(encoding='utf-8'))==lease
    assert (root/'kernel'/'pending.json').read_bytes()==pending


def test_queued_fifo_survives_daemon_restart_without_automatic_replay(tmp_path):
    root=tmp_path/'restart';first=Server(root,{'worker':spec()})
    try:
        a=submit(first,body='first')['job_id'];b=submit(first,body='second')['job_id']
    finally:first.close()
    restored=Server(root,{'worker':spec()})
    try:
        job=restored.rpc('work',{'agent_name':'worker'})['job']
        assert job['job_id']==a
        assert restored.rpc('queue',{'target':'worker'})['queues'][0]['queued_job_ids']==[b]
        lease=json.loads((root/'lease.json').read_text(encoding='utf-8'))
        assert lease['generation']==2
    finally:restored.close()


def test_artifact_escape_and_negative_watch_cursor_are_rejected_without_admission(servers,tmp_path):
    server=servers({'worker':spec()})
    outside=tmp_path/'outside.txt';outside.write_text('outside',encoding='utf-8')
    with pytest.raises(AssertionError,match='escapes CCB artifact directory'):
        server.rpc('submit',dict(project_id='native-test',to_agent='worker',from_actor='user',body='stub',
                                body_artifact={'path':str(outside)}))
    assert server.rpc('queue',{'target':'worker'})['queues'][0]['queued_job_ids']==[]
    receipt=submit(server)
    with pytest.raises(AssertionError,match='cursor cannot be negative'):
        server.rpc('watch',{'target':receipt['job_id'],'cursor':-1})
