"""Explicit session-level MCP answers, not implicit tool permissions/completion."""
import json
import time
import pytest
from test_native_daemon import Server, submit, await_watch
from test_native_claude_requests import spec, pending, request, answer, wire
from test_native_cli_security import cli


@pytest.mark.parametrize('action',['accept','decline','cancel'])
@pytest.mark.parametrize('mode',['elicitation-form','elicitation-url','elicitation-duplicate'])
def test_explicit_mcp_answer_is_once_audited_and_not_task_completion(tmp_path,action,mode):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,mode)})
    try:
        job=submit(server)['job_id'];row=request(server,'awaiting_user')
        assert row['method']=='elicitation' and row['params']['mcp_server_name']=='fixture-server'
        assert row['attribution']=='session_request_during_active_execution'
        result={'action':action}
        if action=='accept' and mode!='elicitation-url':result['content']={'name':'SECRET_FORM_ANSWER','count':2,'choices':['one']}
        response=answer(row,result)
        assert server.rpc('provider-respond',response)['status']=='response_queued'
        assert server.rpc('provider-respond',response)['status']=='response_queued'
        assert server.rpc('watch',{'target':job})['status']=='running'
        assert await_watch(server,job,lambda r:r['terminal'])['status']=='completed'
        sent=[r for r in wire(tmp_path) if r.get('type')=='control_response']
        assert len(sent)==1 and sent[0]['response']['response']==result
        assert pending(server)==[]
        for root in [server.root,tmp_path/'agents']:
            for path in root.rglob('*.json*'):
                assert 'SECRET_FORM_ANSWER' not in path.read_text(encoding='utf-8')
                assert 'FIXTURE_URL_TOKEN' not in path.read_text(encoding='utf-8')
    finally:server.close()


@pytest.mark.parametrize('result',[
    {'action':'accept','content':{'name':'a','count':2}},
    {'action':'accept','content':{'name':'Alice','count':8}},
    {'action':'accept','content':{'name':'Alice','count':2,'choices':['unexpected']}},
    {'action':'accept','content':{'name':'Alice','count':2,'extra':'SECRET_UNREQUESTED'}},
    {'action':'accept'}, {'action':'decline','content':{'name':'Alice'}},
    {'action':'accept','content':{'name':'Alice','count':2},'updatedPermissions':[]},
    {'behavior':'allow'},
])
def test_invalid_form_answers_do_not_reach_provider(tmp_path,result):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'elicitation-form')})
    try:
        job=submit(server)['job_id'];row=request(server,'awaiting_user')
        with pytest.raises(AssertionError) as rejected:server.rpc('provider-respond',answer(row,result))
        assert 'SECRET_UNREQUESTED' not in str(rejected.value)
        assert not [r for r in wire(tmp_path) if r.get('type')=='control_response']
        server.rpc('cancel',{'job_id':job});await_watch(server,job,lambda r:r['terminal'])
    finally:server.close()


@pytest.mark.parametrize('mode',['elicitation-schema-ref','elicitation-schema-nested','elicitation-invalid-url','elicitation-foreign-session'])
def test_invalid_or_foreign_requests_never_become_answerable(tmp_path,mode):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,mode)})
    try:
        job=submit(server)['job_id'];await_watch(server,job,lambda r:r['terminal'])
        assert pending(server)==[]
        sent=[r for r in wire(tmp_path) if r.get('type')=='control_response']
        assert len(sent)==1 and sent[0]['response']['subtype']=='error'
        assert 'SECRET_SCHEMA_PATH' not in json.dumps(sent)
    finally:server.close()


def test_url_answer_cannot_transmit_form_data_and_cli_can_decline(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'elicitation-url')})
    try:
        job=submit(server)['job_id'];row=request(server,'awaiting_user')
        with pytest.raises(AssertionError):server.rpc('provider-respond',answer(row,{'action':'accept','content':{'token':'SECRET_URL_FORM'}}))
        path=tmp_path/'answer.json';path.write_text(json.dumps({'action':'decline'}),encoding='utf-8')
        result=cli('--state-root',server.root,'respond','worker',row['request_id'],path)
        assert result.returncode==0,result.stderr
        await_watch(server,job,lambda r:r['terminal'])
    finally:server.close()


def test_missing_execution_echo_does_not_enable_mcp_answers(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'elicitation-no-anchor')})
    try:
        job=submit(server)['job_id'];row=request(server,'awaiting_execution_anchor')
        with pytest.raises(AssertionError,match='execution anchor'):
            server.rpc('provider-respond',answer(row,{'action':'decline'}))
        server.rpc('cancel',{'job_id':job})
        # This fixture never acknowledges the task's replayed request. Neither
        # its cancellation ACK nor an unanchored result proves task completion.
        assert pending(server)==[]
        assert not server.rpc('watch',{'target':job})['terminal']
        assert not [r for r in wire(tmp_path) if r.get('type')=='control_response']
    finally:server.close()


def test_late_echo_binds_existing_request_without_duplicate_admission(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'elicitation-late-anchor')})
    try:
        job=submit(server)['job_id'];before=request(server,'awaiting_execution_anchor')
        row=request(server,'awaiting_user');assert row['request_id']==before['request_id']
        server.rpc('provider-respond',answer(row,{'action':'cancel'}))
        await_watch(server,job,lambda r:r['terminal'])
    finally:server.close()


def test_withdrawal_and_stale_execution_cannot_be_answered(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec(tmp_path,'elicitation-withdraw')})
    try:
        job=submit(server)['job_id'];row=request(server,'awaiting_user')
        stale=answer(row,{'action':'decline'});stale['execution_instance_id']='other'
        with pytest.raises(AssertionError):server.rpc('provider-respond',stale)
        end=time.monotonic()+5
        while pending(server) and time.monotonic()<end:time.sleep(.025)
        assert pending(server)==[]
        with pytest.raises(AssertionError):server.rpc('provider-respond',answer(row,{'action':'decline'}))
        server.rpc('cancel',{'job_id':job});await_watch(server,job,lambda r:r['terminal'])
    finally:server.close()
