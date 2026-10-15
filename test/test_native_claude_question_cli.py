"""Actual installed Claude CLI against a loopback-only question-producing API."""
import json
import os
from pathlib import Path
# Captured at import: test fixtures isolate HOME/USERPROFILE per test.
REAL_HOME = Path.home()
import time
import pytest
from stubs.anthropic_context_backend import Backend
from test_native_real_models import private_claude_context_config
from test_native_daemon import Server,submit,await_watch
from test_native_claude_requests import answer,pending

@pytest.mark.skipif(os.name!='nt' or not (REAL_HOME/'.local/bin/claude.exe').is_file(),reason='Installed Windows Claude')
@pytest.mark.parametrize('mode',['single','multi','response','deny','cancel','canonical-cwd'])
def test_installed_cli_question_roundtrip_without_account_or_remote_model(tmp_path,mode):
    backend=Backend(question='multi' if mode=='multi' else True);server=None
    try:
        config=private_claude_context_config(tmp_path,backend.url)
        if mode=='canonical-cwd':config['workspace_path']='\\\\?\\'+config['workspace_path']
        config['command'][config['command'].index('--bare')]='--safe-mode'
        config['command'][-1]='AskUserQuestion'
        config['env']['NO_PROXY']='127.0.0.1,localhost'
        server=Server(tmp_path/'runtime/ccbd',{'worker':config})
        receipt=submit(server,body='Local protocol fixture: ask for a format, then acknowledge the answer.')
        deadline=time.monotonic()+20;rows=[]
        while time.monotonic()<deadline:
            rows=pending(server)
            if rows and rows[0]['status']=='awaiting_user':break
            if len(backend.requests)>=2 and server.rpc('watch',{'target':receipt['job_id']})['terminal']:break
            time.sleep(.05)
        tool_results=[block for req in backend.requests for m in req.get('messages',[]) if isinstance(m.get('content'),list) for block in m['content'] if block.get('type')=='tool_result']
        assert rows and rows[0]['status']=='awaiting_user',{'pending':rows,'paths':backend.paths,'tool_results':tool_results}
        row=rows[0];assert row['params']['tool_name']=='AskUserQuestion'
        updated={**row['params']['input'],'answers':{'Which format?':'Detailed'}}
        if mode=='cancel':
            server.rpc('cancel',{'job_id':receipt['job_id'],'record_reply':False})
            result=await_watch(server,receipt['job_id'],lambda r:r['terminal'],timeout=20)
            assert result['status']=='cancelled' and pending(server)==[]
            with pytest.raises(AssertionError):server.rpc('provider-respond',answer(row,{'behavior':'allow','updatedInput':updated}))
            return
        expected='Detailed'
        if mode=='multi':updated['answers']['Which sections?']=['Intro','Summary']
        if mode=='response':updated={**row['params']['input'],'response':'LOCAL_GENERAL_REPLY'};expected='LOCAL_GENERAL_REPLY'
        result={'behavior':'allow','updatedInput':updated}
        if mode=='deny':result={'behavior':'deny','message':'LOCAL_QUESTION_DECLINED'};expected='LOCAL_QUESTION_DECLINED'
        server.rpc('provider-respond',answer(row,result))
        result=await_watch(server,receipt['job_id'],lambda r:r['terminal'],timeout=20)
        assert result['status']=='completed',result
        assert len(backend.requests)>=2
        assert any(expected in json.dumps(v.get('messages',[])) for v in backend.requests[1:])
        if mode=='multi':assert all(s in json.dumps(backend.requests[-1]['messages']) for s in ['Intro','Summary'])
        assert not (tmp_path/'claude-home/.credentials.json').exists()
    finally:
        if server:server.close()
        backend.close()
