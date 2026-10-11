"""Rust Herdr transport tested only with an owned offline executable fixture."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

import pytest
from test_native_cli_security import BIN
from native_kernel import _owner_stopped

STUB=Path(__file__).parent/'stubs/herdr_transport.py'
SESSION='test 独立 session'


def request(tmp_path,mode='normal',args=None,**kwargs):
    return dict(schema_version=1,operation='fixture',session=SESSION,
                command=[sys.executable,'-I',str(STUB),mode,str(tmp_path)],
                args=args or ['workspace','list'],expect_json=True,timeout_ms=1000,**kwargs)


def call(value,**kwargs):
    return subprocess.run([str(BIN),'herdr-transport'],input=json.dumps(value,ensure_ascii=False),
                          capture_output=True,text=True,encoding='utf-8',timeout=8,**kwargs)


def run(value,**kwargs):
    result=call(value,**kwargs)
    assert result.returncode==0,result.stderr
    return json.loads(result.stdout)


def calls(tmp_path):
    path=tmp_path/'calls.jsonl'
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def test_explicit_session_unicode_arguments_and_inherited_scope_are_fenced(tmp_path):
    env=os.environ.copy();env.update(HERDR_ENV='fixture-parent',HERDR_SESSION='unrelated',HERDR_PANE_ID='foreign:p1',HERDR_SOCKET_PATH='foreign',HERDR_CONFIG_PATH='retained-config')
    result=run(request(tmp_path,args=['pane','run','w1:p1','Unicode 中文 "quote" & $literal']),env=env)
    assert result['ok'] and result['outcome']=='acknowledged' and result['task_completion'] is False
    assert result['automatic_replay'] is False and result['read_only'] is False
    row=calls(tmp_path)[0]
    assert row['args']==['--session',SESSION,'pane','run','w1:p1','Unicode 中文 "quote" & $literal']
    assert row['env']=={'HERDR_CONFIG_PATH':'retained-config'}


@pytest.mark.parametrize('args',[
    ['server','stop'],['session','close'],['workspace'],['pane','run'],
    ['pane','split','--current'],['pane','read','w1:p1','--machine','remote'],
    ['pane','run','w1:p1','body','--session=other'],['agent','prompt','worker','body','--wait'],
    ['pane','process-info'],
])
def test_invalid_or_implicit_targets_never_launch_a_child(tmp_path,args):
    result=call(request(tmp_path,args=args))
    assert result.returncode!=0 and not calls(tmp_path)


@pytest.mark.parametrize('field,value',[
    ('timeout_ms',-1),('timeout_ms','secret-value'),('args','secret-value'),('command','secret-value'),('session','--other'),
])
def test_invalid_request_values_are_not_echoed_or_executed(tmp_path,field,value):
    data=request(tmp_path);data[field]=value
    result=call(data)
    assert result.returncode!=0 and not calls(tmp_path)
    assert 'secret-value' not in result.stderr+result.stdout


@pytest.mark.parametrize('mode,failure',[
    ('missing-server','command_failed'),('failed-envelope','server_rejected'),('bad-json','invalid_json'),
    ('unknown-status','server_rejected'),('boolean-error','server_rejected'),
    ('bad-utf8','invalid_utf8'),('stdout-flood','output_limit'),('stderr-flood','output_limit'),('hang','observation_timeout'),
])
def test_failed_mutations_are_unknown_and_are_never_replayed(tmp_path,mode,failure):
    data=request(tmp_path,mode,args=['pane','run','w1:p1','do-not-echo-this-error-or-prompt'])
    result=run(data)
    assert not result['ok'] and result['failure']==failure and result['outcome']=='unknown'
    assert len(calls(tmp_path))==1 and result['stdout'] is None
    assert 'do-not-echo' not in json.dumps(result)
    if mode=='hang':assert _owner_stopped(int((tmp_path/'cli.pid').read_text()))


def test_read_failure_is_not_missing_server_proof_and_plain_capture_preserves_utf8(tmp_path):
    failed=run(request(tmp_path,'missing-server'))
    assert failed['outcome']=='unobserved' and failed['automatic_replay'] is False
    data=request(tmp_path,args=['pane','read','w1:p1','--lines','10']);data['expect_json']=False
    captured=run(data)
    assert captured['ok'] and captured['read_only'] and '中文 output' in captured['stdout']


def test_json_rejection_is_not_success_when_legacy_caller_did_not_require_json(tmp_path):
    data=request(tmp_path,'failed-envelope',args=['pane','run','w1:p1','body']);data['expect_json']=False
    result=run(data)
    assert not result['ok'] and result['failure']=='server_rejected'


def test_cli_stdin_is_closed_and_not_inherited_from_request_stream(tmp_path):
    assert run(request(tmp_path,'stdin-eof'))['ok']

@pytest.mark.parametrize('mode,ok',[('server-status-running',True),('server-status-stopped',True),('server-status-conflict',False)])
def test_real_flat_server_status_is_data_only_for_the_status_query(tmp_path,mode,ok):
    result=run(request(tmp_path,mode,args=['status','server','--json']))
    assert result['ok'] is ok
    mutation=run(request(tmp_path,mode,args=['agent','prompt','worker','task']))
    assert mutation['ok'] is False


@pytest.mark.skipif(os.name!='nt',reason='Windows job-owned descendant lifetime')
def test_parent_exit_and_json_are_not_enough_while_descendant_keeps_pipe_open(tmp_path):
    result=run(request(tmp_path,'descendant',args=['pane','run','w1:p1','body']))
    assert not result['ok'] and result['failure']=='observation_timeout' and result['exit_code']==0
    pid=int((tmp_path/'descendant.pid').read_text())
    deadline=time.monotonic()+3
    while not _owner_stopped(pid) and time.monotonic()<deadline:time.sleep(.02)
    assert _owner_stopped(pid)


def adapter(tmp_path,monkeypatch,mode):
    from platforms.windows.herdr.runtime.cli import HerdrCliRequestAdapter
    from platforms.windows.herdr.runtime import native_transport
    monkeypatch.setenv('CCB_CORE_RUNTIME','rust')
    monkeypatch.setenv('CCB_NATIVE_BIN',str(BIN))
    monkeypatch.setenv('CCB_HERDR_COMMAND_TIMEOUT_MS','1000')
    original=native_transport._invoke
    def fixture(request,**kwargs):
        # The adapter still creates the real session-scoped request; replace only
        # the executable prefix to reach the offline Python protocol fixture.
        request={**request,'command':[sys.executable,'-I',str(STUB),mode,str(tmp_path)]}
        return original(request,**kwargs)
    monkeypatch.setattr(native_transport,'_invoke',fixture)
    def forbidden(*args,**kwargs):raise AssertionError('legacy process/server fallback was called')
    result=HerdrCliRequestAdapter(session_name=SESSION,herdr_executable=sys.executable,run_fn=forbidden,popen_fn=forbidden)
    return result


def test_transitional_rust_mode_uses_native_command_driver_and_preserves_existing_dto(tmp_path,monkeypatch):
    bridge=adapter(tmp_path,monkeypatch,'normal')
    info=bridge('server_info',{})
    assert info['api_schema']=='Herdr API' and info['version']=='fixture'
    assert len(calls(tmp_path))==3


@pytest.mark.parametrize('mode',['missing-server','hang','failed-envelope'])
def test_transitional_adapter_never_starts_server_or_retries_ambiguous_mutation(tmp_path,monkeypatch,mode):
    from terminal_runtime.mux_backend_contract import MuxCommandErrorV2
    bridge=adapter(tmp_path,monkeypatch,mode)
    with pytest.raises(MuxCommandErrorV2) as failure:
        bridge('send_text',{'pane_id':'w1:p1','text':'secret-body'})
    assert len(calls(tmp_path))==1
    assert failure.value.evidence['outcome']=='unknown' and failure.value.evidence['automatic_replay'] is False
    assert 'secret-body' not in str(failure.value)


@pytest.mark.parametrize('field,value',[
    ('session','foreign-session'),('task_completion',True),('automatic_replay',True),('stdout',None),
])
def test_transitional_bridge_rejects_mismatched_or_unsafe_helper_receipt(monkeypatch,field,value):
    from platforms.windows.herdr.runtime import native_transport
    from terminal_runtime.mux_backend_contract import MuxCommandErrorV2
    monkeypatch.setenv('CCB_NATIVE_BIN',str(BIN))
    reply=dict(schema_version=1,operation='send_text',session=SESSION,ok=True,read_only=False,
               task_completion=False,automatic_replay=False,outcome='acknowledged',stdout='{}')
    reply[field]=value
    observed=[]
    def fake(argv,**kwargs):
        observed.append(argv)
        return subprocess.CompletedProcess(argv,0,json.dumps(reply),'')
    monkeypatch.setattr(native_transport.subprocess,'run',fake)
    with pytest.raises(MuxCommandErrorV2) as failure:
        native_transport.run('send_text',['herdr','--session',SESSION,'pane','run','w1:p1','secret-body'],
                             expect_json=False,session=SESSION,env={},ipc_ref='fixture')
    assert len(observed)==1 and failure.value.evidence['outcome']=='unknown'


def test_python_mode_preserves_legacy_injected_command_runner(monkeypatch):
    from platforms.windows.herdr.runtime.cli import HerdrCliRequestAdapter
    monkeypatch.setenv('CCB_CORE_RUNTIME','python')
    observed=[]
    def fake(command,**kwargs):
        observed.append(command)
        return subprocess.CompletedProcess(command,0,'','')
    bridge=HerdrCliRequestAdapter(session_name=SESSION,herdr_executable=sys.executable,run_fn=fake)
    assert bridge('send_text',{'pane_id':'w1:p1','text':'body'})['status']=='ok'
    assert observed==[[sys.executable,'--session',SESSION,'pane','run','w1:p1','body']]
