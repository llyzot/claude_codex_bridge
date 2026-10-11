"""Explicit answers own one live Claude tool call, never the whole tool policy."""
import json,sys,time
from pathlib import Path
import pytest
from test_native_daemon import Server,submit,await_watch,spec as command_spec
from test_native_cli_security import cli

STUB=Path(__file__).parent/'stubs/claude_interactive.py'
def spec(root,mode):
    return dict(provider='claude',driver_kind='claude_stream',profile_family='structured_result',
        command=[sys.executable,'-I',str(STUB),mode],env={'CCB_FIXTURE_TRACE':str(root/'wire.jsonl')})
def pending(server):return server.rpc('provider-requests',{'agent_name':'worker'})['requests']
def request(server,status=None):
    until=time.monotonic()+8
    while time.monotonic()<until:
        rows=pending(server)
        if rows and (status is None or rows[0]['status']==status):return rows[0]
        assert server.proc.poll() is None
        time.sleep(.025)
    pytest.fail('Claude request was not observed')
def answer(row,result):
    return dict(agent_name='worker',result=result,**{key:row[key] for key in ('request_id','job_id','runtime_instance_id','execution_instance_id')})
def wire(root):
    path=root/'wire.jsonl'
    return [json.loads(s) for s in path.read_text().splitlines()] if path.exists() else []

@pytest.mark.parametrize('mode',['normal','partial','subagent','duplicate'])
@pytest.mark.parametrize('decision',['allow','deny'])
def test_owned_tool_answers_are_one_time_audited_and_not_completion(tmp_path,mode,decision):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,mode)})
    try:
        receipt=submit(server);row=request(server,'awaiting_user')
        assert row['job_id']==receipt['job_id'] and row['params']['default_to_no'] is True
        result={'behavior':'allow'} if decision=='allow' else {'behavior':'deny','message':'SECRET_FIXTURE_DENIAL'}
        payload=answer(row,result)
        assert server.rpc('provider-respond',payload)['status']=='response_queued'
        assert server.rpc('provider-respond',payload)['status']=='response_queued'
        assert server.rpc('watch',{'target':receipt['job_id']})['status']=='running'
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        responses=[r for r in wire(tmp_path) if r.get('type')=='control_response']
        assert len(responses)==1
        actual=responses[0]['response']['response']
        assert actual['behavior']==decision and actual['toolUseID']==row['params']['tool_use_id']
        if decision=='allow':assert actual['updatedInput']==row['params']['input']
        assert pending(server)==[]
        for directory in [server.root,tmp_path/'agents']:
            for path in directory.rglob('*.json*'):
                text=path.read_text()
                assert all(secret not in text for secret in ('SECRET_FIXTURE_INPUT','SECRET_PERMISSION_REASON','SECRET_FIXTURE_DENIAL'))
    finally:server.close()

@pytest.mark.parametrize('result',[
    {'behavior':'allow','updatedInput':{'file_path':'different'}},
    {'behavior':'allow','updatedPermissions':[{'type':'addRules'}]},
    {'behavior':'allow','decisionClassification':'auto_approved'},
    {'behavior':'allow','toolUseID':'other-tool'},
    {'behavior':'deny'}, {'behavior':'deny','message':'x','interrupt':'yes'}, {'behavior':'unknown'},
])
def test_invalid_or_widening_answers_do_not_reach_provider(tmp_path,result):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'normal')})
    try:
        receipt=submit(server);row=request(server,'awaiting_user')
        with pytest.raises(AssertionError):server.rpc('provider-respond',answer(row,result))
        assert not [r for r in wire(tmp_path) if r.get('type')=='control_response']
        assert pending(server)[0]['status']=='awaiting_user'
        server.rpc('cancel',{'job_id':receipt['job_id']})
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='cancelled'
    finally:server.close()

@pytest.mark.parametrize('mode,status',[('no-stamp','awaiting_tool_anchor'),('foreign-stamp','awaiting_tool_anchor'),('foreign-parent','awaiting_tool_anchor'),('mixed-stamp','awaiting_tool_anchor'),('native-ui','native_ui_required'),('question','native_ui_required')])
def test_unproven_or_native_ui_requests_cannot_be_approved(tmp_path,mode,status):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,mode)})
    try:
        receipt=submit(server);row=request(server,status)
        with pytest.raises(AssertionError):server.rpc('provider-respond',answer(row,{'behavior':'allow'}))
        server.rpc('cancel',{'job_id':receipt['job_id'],'record_reply':False})
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='cancelled'
        assert pending(server)==[]
    finally:server.close()

def test_early_request_waits_for_matching_tool_stamp(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'late-anchor')})
    try:
        receipt=submit(server);row=request(server,'awaiting_tool_anchor')
        with pytest.raises(AssertionError):server.rpc('provider-respond',answer(row,{'behavior':'allow'}))
        current=request(server,'awaiting_user');assert current['request_id']==row['request_id']
        server.rpc('provider-respond',answer(current,{'behavior':'deny','message':'No'}))
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
    finally:server.close()

@pytest.mark.parametrize('key',['runtime_instance_id','execution_instance_id','job_id','request_id'])
def test_wrong_response_binding_is_rejected(tmp_path,key):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'normal')})
    try:
        receipt=submit(server);row=request(server,'awaiting_user')
        payload=answer(row,{'behavior':'allow'});payload[key]='foreign'
        with pytest.raises(AssertionError):server.rpc('provider-respond',payload)
        server.rpc('cancel',{'job_id':receipt['job_id']})
        await_watch(server,receipt['job_id'],lambda r:r['terminal'])
    finally:server.close()

def test_withdrawal_invalidates_handle_without_answer(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'withdraw')})
    try:
        receipt=submit(server);row=request(server,'awaiting_user')
        until=time.monotonic()+5
        while pending(server) and time.monotonic()<until:time.sleep(.025)
        assert pending(server)==[]
        with pytest.raises(AssertionError):server.rpc('provider-respond',answer(row,{'behavior':'allow'}))
        server.rpc('cancel',{'job_id':receipt['job_id']});await_watch(server,receipt['job_id'],lambda r:r['terminal'])
    finally:server.close()

def test_conflicting_wire_identity_faults_only_that_provider(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'reuse'),'peer':command_spec('worker')})
    try:
        submit(server);fault=server.root/'providers/claude/worker/fault.json';until=time.monotonic()+8
        while not fault.exists() and time.monotonic()<until:time.sleep(.025)
        assert fault.exists() and pending(server)==[]
        peer=submit(server,'peer');assert await_watch(server,peer['job_id'],lambda r:r['terminal'])['status']=='completed'
    finally:server.close()

@pytest.mark.parametrize('mode',['complete','late-request','unknown','dialog','foreign-session'])
def test_completion_and_unsupported_controls_never_imply_approval(tmp_path,mode):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,mode)})
    try:
        receipt=submit(server)
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
        time.sleep(.4)
        assert pending(server)==[]
        responses=[r for r in wire(tmp_path) if r.get('type')=='control_response']
        assert all(r['response']['subtype']=='error' for r in responses)
        if mode=='dialog':assert responses==[]
        if mode=='late-request':
            assert responses==[]
            assert (server.root/'providers/claude/worker/fault.json').exists()
    finally:server.close()

def test_cli_respond_routes_to_claude(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'normal')})
    try:
        receipt=submit(server);row=request(server,'awaiting_user')
        listed=cli('--state-root',server.root,'pending','worker')
        assert listed.returncode==0 and json.loads(listed.stdout)['requests'][0]['request_id']==row['request_id']
        path=tmp_path/'answer.json';path.write_text(json.dumps({'behavior':'deny','message':'No'}))
        output=cli('--state-root',server.root,'respond','worker',row['request_id'],path)
        assert output.returncode==0,output.stderr
        await_watch(server,receipt['job_id'],lambda r:r['terminal'])
    finally:server.close()


def test_cli_pending_escapes_display_controls_without_changing_values(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'bidi')})
    try:
        receipt=submit(server);row=request(server,'awaiting_user')
        output=cli('--state-root',server.root,'pending','worker')
        assert output.returncode==0,output.stderr
        assert '\u202e' not in output.stdout and '\u2069' not in output.stdout
        assert '\\u202e' in output.stdout and '\\u2069' in output.stdout
        assert json.loads(output.stdout)['requests'][0]['params']==row['params']
        server.rpc('cancel',{'job_id':receipt['job_id']});await_watch(server,receipt['job_id'],lambda r:r['terminal'])
    finally:server.close()


def test_deadline_retires_permissions_and_does_not_approve(tmp_path):
    config=spec(tmp_path,'normal');config['timeout_s']=.35
    server=Server(tmp_path/'state',{'worker':config})
    try:
        receipt=submit(server);row=request(server,'awaiting_user')
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='cancelled'
        with pytest.raises(AssertionError):server.rpc('provider-respond',answer(row,{'behavior':'allow'}))
        assert pending(server)==[]
        assert not [r for r in wire(tmp_path) if r.get('type')=='control_response']
    finally:server.close()


def test_durable_cancel_fences_answer_before_cancel_completion(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'normal')})
    try:
        receipt=submit(server);row=request(server,'awaiting_user')
        server.rpc('cancel',{'job_id':receipt['job_id'],'record_reply':False})
        with pytest.raises(AssertionError):server.rpc('provider-respond',answer(row,{'behavior':'allow'}))
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='cancelled'
        assert not [r for r in wire(tmp_path) if r.get('type')=='control_response']
    finally:server.close()


def test_interrupting_denial_commits_cancellation_before_provider_action(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'normal')})
    try:
        receipt=submit(server);row=request(server,'awaiting_user')
        server.rpc('provider-respond',answer(row,{'behavior':'deny','message':'Stop this task','interrupt':True}))
        jobs=server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result']
        assert jobs[-1]['provider_options']['native_cancel_intent']['record_reply'] is True
        with pytest.raises(AssertionError):server.rpc('provider-respond',answer(row,{'behavior':'allow'}))
        assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='cancelled'
    finally:server.close()


@pytest.mark.parametrize('flag',[['--permission-prompt-tool','other'],['--permission-prompt-tool=other']])
def test_conflicting_permission_transport_is_not_overridden(tmp_path,flag):
    config=spec(tmp_path,'normal');config['command']+=flag
    server=Server(tmp_path/'state',{'worker':config,'peer':command_spec('worker')})
    try:
        fault=server.root/'providers/claude/worker/fault.json';until=time.monotonic()+8
        while not fault.exists() and time.monotonic()<until:time.sleep(.025)
        assert fault.exists() and json.loads(fault.read_text())['phase']=='startup'
        assert wire(tmp_path)==[]
        receipt=submit(server,'peer');assert await_watch(server,receipt['job_id'],lambda r:r['terminal'])['status']=='completed'
    finally:server.close()
