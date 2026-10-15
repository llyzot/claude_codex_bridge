"""Native persistent Pi transport exercised through the public daemon."""
import json
import os
from pathlib import Path
# Captured at import: test fixtures isolate HOME/USERPROFILE per test.
REAL_HOME = Path.home()
import sys
import time

import pytest
from test_native_daemon import Server,spec,submit,await_watch

ROOT=Path(__file__).resolve().parents[1]
STUB=ROOT/'test'/'stubs'/'pi_rpc_worker.py'

def pi_spec(mode):
    return dict(provider='pi',driver_kind='pi_rpc',profile_family='protocol_turn',command=[sys.executable,'-I',str(STUB),mode])

def test_pi_agent_end_is_not_terminal_and_same_process_handles_later_turn(tmp_path):
    server=Server(tmp_path/'state',{'pi_agent':pi_spec('worker')})
    try:
        first=submit(server,'pi_agent')
        row=await_watch(server,first['job_id'],lambda r:r['terminal'])
        assert row['reply']=='persistent Pi result'
        assert row['completion_reason']=='pi_agent_settled'
        before=json.loads((server.root/'providers/pi/pi_agent/session.json').read_text())
        second=submit(server,'pi_agent',body='second task')
        second_row=await_watch(server,second['job_id'],lambda r:r['terminal'])
        after=json.loads((server.root/'providers/pi/pi_agent/session.json').read_text())
        assert second_row['reply']=='persistent Pi result'
        assert before['pid']==after['pid']
        assert before['runtime_instance_id']==after['runtime_instance_id']
        assert 'never-persist-this' not in json.dumps(after)
        assert after['state']['messageCount']==4
    finally:server.close()

def test_persistent_pi_lead_receives_callback_without_polling_or_process_restart(tmp_path):
    server=Server(tmp_path/'state',{'lead':pi_spec('lead'),'worker':spec('worker')})
    try:
        receipt=submit(server,'lead')
        row=await_watch(server,receipt['job_id'],lambda r:r['visible_reply_source']=='message_bureau_reply')
        assert row['reply']=='persistent Pi reviewed result'
        metadata=json.loads((server.root/'providers/pi/lead/session.json').read_text())
        assert metadata['state']['messageCount']==4
        edges=server.rpc('records.latest',dict(stream='callbacks',identity='edge_id'),version=1)['result']
        assert len(edges)==1 and edges[0]['state']=='done'
    finally:server.close()


def test_settled_before_prompt_ack_is_reconciled_by_correlated_state_query(tmp_path):
    server=Server(tmp_path/'state',{'pi_agent':pi_spec('late-ack')})
    try:
        receipt=submit(server,'pi_agent')
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['reply']=='persistent Pi result'
        assert row['completion_reason']=='pi_agent_settled'
    finally:server.close()


def test_foreign_prompt_anchor_cannot_complete_current_task(tmp_path):
    server=Server(tmp_path/'state',{'pi_agent':pi_spec('foreign-anchor')})
    try:
        receipt=submit(server,'pi_agent')
        await_watch(server,receipt['job_id'],lambda r:r['status']=='running')
        time.sleep(.4)
        assert server.rpc('watch',{'target':receipt['job_id']})['status']=='running'
        assert server.rpc('records.list',{'stream':'replies'},version=1)['result']==[]
    finally:server.close()


def test_clean_restart_uses_exact_saved_pi_session(tmp_path):
    root=tmp_path/'state';config=pi_spec('persist');config['command'].append(str(tmp_path/'session.json'))
    server=Server(root,{'pi_agent':config})
    try:
        receipt=submit(server,'pi_agent');await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        before=json.loads((root/'providers/pi/pi_agent/session.json').read_text())
    finally:server.close()
    restored=Server(root,{'pi_agent':config})
    try:
        receipt=submit(restored,'pi_agent');await_watch(restored,receipt['job_id'],lambda r:r['terminal'])
        after=json.loads((root/'providers/pi/pi_agent/session.json').read_text())
        assert before['state']['sessionId']==after['state']['sessionId']
        assert before['runtime_instance_id']!=after['runtime_instance_id']
        assert after['state']['messageCount']==4
    finally:restored.close()


def test_pi_cancel_ack_is_not_completion_and_silent_cancel_emits_no_reply(tmp_path):
    server=Server(tmp_path/'state',{'pi_agent':pi_spec('hang')})
    try:
        receipt=submit(server,'pi_agent');await_watch(server,receipt['job_id'],lambda r:r['status']=='running')
        time.sleep(.1)
        result=server.rpc('cancel',{'job_id':receipt['job_id'],'record_reply':False})
        assert result['status']=='cancellation_requested'
        row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='cancelled'
        assert row['completion_reason']=='pi_aborted'
        assert server.rpc('records.list',{'stream':'replies'},version=1)['result']==[]
    finally:server.close()


def test_pi_deadline_requests_abort_and_waits_for_settled_evidence(tmp_path):
    config=pi_spec('hang');config['timeout_s']=.2
    server=Server(tmp_path/'state',{'pi_agent':config})
    try:
        receipt=submit(server,'pi_agent');row=await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        assert row['status']=='cancelled'
        records=server.rpc('records.latest',dict(stream='jobs',agent_name='pi_agent',identity='job_id'),version=1)['result']
        assert records[0]['terminal_decision']['diagnostics']['deadline_triggered'] is True
        assert records[0]['terminal_decision']['diagnostics']['completion_authority']=='pi_rpc_agent_settled'
    finally:server.close()


@pytest.mark.skipif(os.name!='nt',reason='Uses the installed Windows Pi package discovered in this workspace')
def test_installed_pi_read_only_rpc_handshake_in_private_home(tmp_path):
    node=(REAL_HOME/'scoop/apps/nodejs-lts/current/node.exe')
    cli=(REAL_HOME/'scoop/apps/nodejs-lts/current/bin/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js')
    if not node.is_file() or not cli.is_file():pytest.skip('Installed Pi/Node not present')
    home=tmp_path/'pi-home';home.mkdir()
    record=dict(provider='pi',driver_kind='pi_rpc',profile_family='protocol_turn',workspace_path=str(tmp_path),
        command=[str(node),str(cli),'--mode','rpc','--no-session','--no-extensions','--no-skills','--no-prompt-templates','--no-themes'],
        env={'PI_CODING_AGENT_DIR':str(home),'PI_SKIP_VERSION_CHECK':'1','PI_TELEMETRY':'0'})
    server=Server(tmp_path/'state',{'pi_probe':record})
    try:
        path=server.root/'providers/pi/pi_probe/session.json'
        deadline=time.monotonic()+20
        while not path.exists() and time.monotonic()<deadline:
            assert server.proc.poll() is None,server.proc.stderr.read()
            time.sleep(.05)
        assert path.exists(),'Pi did not complete get_state handshake'
        metadata=json.loads(path.read_text(encoding='utf-8'))
        assert metadata['state']['sessionId']
        assert metadata['state']['isStreaming'] is False
        assert metadata['state']['isCompacting'] is False
        assert server.rpc('queue',{'target':'pi_probe'})['queues'][0]['active_job_id'] is None
    finally:server.close()
