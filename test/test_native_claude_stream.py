import json
from pathlib import Path
import sys
import time
import os
import pytest
from test_native_daemon import Server,submit,await_watch
from test_native_codex_rpc import codex_spec

ROOT=Path(__file__).resolve().parents[1]
STUB=ROOT/'test'/'stubs'/'claude_stream_worker.py'
def claude_spec(mode='worker'):
    return dict(provider='claude',driver_kind='claude_stream',profile_family='structured_result',command=[sys.executable,'-I',str(STUB),mode])

def test_claude_result_fences_session_and_ignores_subagent_output(tmp_path):
    server=Server(tmp_path/'state',{'worker':claude_spec()})
    try:
        receipt=submit(server);row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['reply']=='Claude final result'
        before=json.loads((server.root/'providers/claude/worker/session.json').read_text())
        receipt=submit(server);await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        after=json.loads((server.root/'providers/claude/worker/session.json').read_text())
        assert before['pid']==after['pid'] and before['state']['session_id']==after['state']['session_id']
    finally:server.close()

def test_foreign_claude_result_cannot_complete_active_request(tmp_path):
    server=Server(tmp_path/'state',{'worker':claude_spec('foreign')})
    try:
        receipt=submit(server);await_watch(server,receipt['job_id'],lambda r:r['status']=='running');time.sleep(.2)
        assert server.rpc('watch',{'target':receipt['job_id']})['status']=='running'
    finally:server.close()


def test_duplicate_previous_result_does_not_complete_next_turn(tmp_path):
    server=Server(tmp_path/'state',{'worker':claude_spec('duplicate')})
    try:
        first=submit(server);await_watch(server,first['job_id'],lambda r:r['terminal'])
        second=submit(server);await_watch(server,second['job_id'],lambda r:r['terminal'])
        replies=server.rpc('records.list',{'stream':'replies'},version=1)['result']
        assert len(replies)==2
        assert replies[0]['diagnostics']['decision_diagnostics']['result_id']!=replies[1]['diagnostics']['decision_diagnostics']['result_id']
    finally:server.close()

def test_claude_lead_automatically_receives_codex_callback_same_session(tmp_path):
    server=Server(tmp_path/'state',{'lead':claude_spec('lead'),'worker':codex_spec()})
    try:
        receipt=submit(server,'lead');row=await_watch(server,receipt['job_id'],lambda r:r['visible_reply_source']=='message_bureau_reply')
        assert row['reply']=='Claude reviewed result'
        assert server.rpc('records.latest',dict(stream='callbacks',identity='edge_id'),version=1)['result'][0]['state']=='done'
    finally:server.close()

def test_claude_clean_restart_resumes_only_saved_session(tmp_path):
    root=tmp_path/'state';first=Server(root,{'worker':claude_spec()})
    try:
        receipt=submit(first);await_watch(first,receipt['job_id'],lambda r:r['terminal'])
        before=json.loads((root/'providers/claude/worker/session.json').read_text())
    finally:first.close()
    restored=Server(root,{'worker':claude_spec()})
    try:
        receipt=submit(restored);await_watch(restored,receipt['job_id'],lambda r:r['terminal'])
        after=json.loads((root/'providers/claude/worker/session.json').read_text())
        assert before['state']['session_id']==after['state']['session_id']
        assert before['runtime_instance_id']!=after['runtime_instance_id']
    finally:restored.close()


@pytest.mark.skipif(os.name!='nt',reason='Installed Windows Claude wire check')
def test_installed_unauthenticated_claude_reports_failure_without_daemon_exit(tmp_path):
    executable=Path('C:/Users/user/.local/bin/claude.exe')
    if not executable.is_file():pytest.skip('Claude not installed')
    home=tmp_path/'claude-home';home.mkdir()
    config=dict(provider='claude',driver_kind='claude_stream',profile_family='structured_result',workspace_path=str(tmp_path),
                command=[str(executable),'-p','--verbose','--input-format','stream-json','--output-format','stream-json',
                         '--replay-user-messages','--safe-mode','--tools',''],env={'CLAUDE_CONFIG_DIR':str(home)})
    server=Server(tmp_path/'state',{'worker':config})
    try:
        receipt=submit(server,body='Reply exactly WIRE_OK. Do not call tools.')
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='failed'
        assert 'login' in row['reply'].lower() or 'logged' in row['reply'].lower()
        time.sleep(.1)
        assert server.rpc('ping')['runtime']=='rust'
        records=server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result']
        assert len(records)==1
    finally:server.close()
