"""Owned persistent Codex app-server protocol and installed read-only probe."""
import json
import os
from pathlib import Path
import sys
import time

import pytest
from test_native_daemon import Server,submit,await_watch
from test_native_pi_rpc import pi_spec

ROOT=Path(__file__).resolve().parents[1]
STUB=ROOT/'test'/'stubs'/'codex_app_server.py'
def codex_spec(mode='normal'):
    return dict(provider='codex',driver_kind='codex_app_server',profile_family='protocol_turn',command=[sys.executable,'-I',str(STUB),mode])

@pytest.mark.parametrize('mode',['normal','late-response','late-idle'])
def test_thread_turn_completion_discards_commentary_and_correlates_start(mode,tmp_path):
    server=Server(tmp_path/'state',{'worker':codex_spec(mode)})
    try:
        receipt=submit(server)
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['reply']=='Codex final result'
        assert row['completion_reason']=='codex_turn_completed'
        first=json.loads((server.root/'providers/codex/worker/session.json').read_text())
        second=submit(server)
        await_watch(server,second['job_id'],lambda r:r['terminal'])
        after=json.loads((server.root/'providers/codex/worker/session.json').read_text())
        assert after['pid']==first['pid']
        assert after['state']['thread_id']==first['state']['thread_id']
    finally:server.close()

def test_foreign_completed_turn_does_not_release_current_task(tmp_path):
    server=Server(tmp_path/'state',{'worker':codex_spec('foreign-turn')})
    try:
        receipt=submit(server)
        await_watch(server,receipt['job_id'],lambda r:r['status']=='running')
        time.sleep(.3)
        assert server.rpc('watch',{'target':receipt['job_id']})['status']=='running'
        assert server.rpc('records.list',{'stream':'replies'},version=1)['result']==[]
    finally:server.close()

def test_codex_thread_identity_is_resumed_on_clean_restart(tmp_path):
    root=tmp_path/'state';server=Server(root,{'worker':codex_spec()})
    try:
        receipt=submit(server);await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        before=json.loads((root/'providers/codex/worker/session.json').read_text())
    finally:server.close()
    restored=Server(root,{'worker':codex_spec()})
    try:
        receipt=submit(restored);await_watch(restored,receipt['job_id'],lambda r:r['terminal'])
        after=json.loads((root/'providers/codex/worker/session.json').read_text())
        assert before['state']['thread_id']==after['state']['thread_id']
        assert before['runtime_instance_id']!=after['runtime_instance_id']
    finally:restored.close()

def test_persistent_pi_lead_receives_native_codex_worker_result(tmp_path):
    server=Server(tmp_path/'state',{'lead':pi_spec('lead'),'worker':codex_spec()})
    try:
        receipt=submit(server,'lead')
        row=await_watch(server,receipt['job_id'],lambda r:r['visible_reply_source']=='message_bureau_reply')
        assert row['reply']=='persistent Pi reviewed result'
        replies=server.rpc('records.list',{'stream':'replies'},version=1)['result']
        assert any(r['reply']=='Codex final result' for r in replies)
        edge=server.rpc('records.latest',dict(stream='callbacks',identity='edge_id'),version=1)['result'][0]
        assert edge['state']=='done'
    finally:server.close()

@pytest.mark.skipif(os.name!='nt',reason='Installed Windows Codex probe')
def test_installed_codex_creates_owned_ephemeral_thread_without_model_call(tmp_path):
    executable=Path('C:/Users/user/scoop/apps/nodejs-lts/current/bin/node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe')
    if not executable.is_file():pytest.skip('Installed Codex not present')
    home=tmp_path/'codex-home';home.mkdir()
    spec=dict(provider='codex',driver_kind='codex_app_server',profile_family='protocol_turn',workspace_path=str(tmp_path),
        command=[str(executable),'app-server','--stdio'],thread_options={'ephemeral':True},env={'CODEX_HOME':str(home)})
    server=Server(tmp_path/'state',{'probe':spec})
    try:
        path=server.root/'providers/codex/probe/session.json';deadline=time.monotonic()+20
        while not path.exists() and time.monotonic()<deadline:
            assert server.proc.poll() is None,server.proc.stderr.read()
            time.sleep(.05)
        assert path.exists(),'Codex did not complete initialization/thread start'
        metadata=json.loads(path.read_text())
        assert metadata['state']['thread_id']
        assert metadata['state']['status']['type']=='idle'
        assert server.rpc('queue',{'target':'probe'})['queues'][0]['active_job_id'] is None
    finally:server.close()
