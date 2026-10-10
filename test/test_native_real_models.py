"""Explicit opt-in real model checks in new private homes/workspaces only."""
import json
import os
from pathlib import Path
import subprocess
import time

import pytest
from test_native_daemon import Server,submit

ROOT=Path(__file__).resolve().parents[1]
NATIVE=ROOT/'rust'/'target'/'debug'/'ccb-native.exe'
CODEX=Path('C:/Users/user/scoop/apps/nodejs-lts/current/bin/node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe')
NODE=Path('C:/Users/user/scoop/apps/nodejs-lts/current/node.exe')
PI=Path('C:/Users/user/scoop/apps/nodejs-lts/current/bin/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js')
pytestmark=pytest.mark.skipif(os.environ.get('CCB_RUN_REAL_MODELS')!='1',reason='Explicit real-model opt-in required')

def protected_copy(source,destination):
    result=subprocess.run([str(NATIVE),'private-copy',str(source),str(destination)],capture_output=True,text=True,timeout=8)
    assert result.returncode==0,result.stderr

def configs(tmp_path):
    workspace=tmp_path/'workspace';workspace.mkdir()
    codex_home=tmp_path/'codex-home';codex_home.mkdir()
    protected_copy(Path('C:/Users/user/.codex/auth.json'),codex_home/'auth.json')
    worker=dict(provider='codex',driver_kind='codex_app_server',profile_family='protocol_turn',workspace_path=str(workspace),
        command=[str(CODEX),'app-server','--stdio'],env={'CODEX_HOME':str(codex_home)},
        thread_options={'model':'gpt-6.1-sol','approvalPolicy':'never','sandbox':'read-only'},turn_options={'effort':'low'})
    pi_home=tmp_path/'pi-home';pi_home.mkdir()
    shell=Path('C:/Users/user/scoop/apps/git/current/usr/bin/bash.exe')
    assert shell.is_file(),'Existing Git Bash required for the Pi bash tool'
    (pi_home/'settings.json').write_text(json.dumps({'shellPath':str(shell)}),encoding='utf-8')
    protected_copy(Path('C:/Users/user/.pi/agent/auth.json'),pi_home/'auth.json')
    for name in ['models.json']:
        source=Path('C:/Users/user/.pi/agent')/name
        if source.exists():protected_copy(source,pi_home/name)
    lead=dict(provider='pi',driver_kind='pi_rpc',profile_family='protocol_turn',workspace_path=str(workspace),
        command=[str(NODE),str(PI),'--mode','rpc','--provider','openai','--model','gpt-6-astra','--thinking','low',
                 '--no-extensions','--no-skills','--no-prompt-templates','--no-themes','--session-dir',str(pi_home/'sessions'),'--tools','bash'],
        env={'PI_CODING_AGENT_DIR':str(pi_home),'PI_SKIP_VERSION_CHECK':'1','PI_TELEMETRY':'0'},timeout_s=120)
    return worker,lead

def wait_result(server,id,callback=False):
    deadline=time.monotonic()+150
    while time.monotonic()<deadline:
        assert server.proc.poll() is None,server.proc.stderr.read()
        row=server.rpc('watch',{'target':id})
        if callback and row['visible_reply_source']=='message_bureau_reply':return row
        if callback and row['terminal'] and row['visible_reply_source']!='callback_delegated_pending':
            pytest.fail('Lead ended without a delegated callback: '+row['reply'])
        if not callback and row['terminal']:return row
        time.sleep(.1)
    pytest.fail('Real model did not reach a confirmed result')

def test_real_codex_worker_model_round_trip(tmp_path):
    worker,_=configs(tmp_path)
    server=Server(tmp_path/'state',{'worker':worker})
    try:
        receipt=submit(server,body='Reply exactly CCB_WORKER_OK. Do not call tools or edit files.')
        result=wait_result(server,receipt['job_id'])
        assert result['status']=='completed',result['reply']
        assert 'CCB_WORKER_OK' in result['reply']
    finally:server.close()

def test_real_pi_lead_codex_worker_automatic_callback(tmp_path):
    worker,lead=configs(tmp_path)
    server=Server(tmp_path/'state',{'lead':lead,'worker':worker})
    command=f"'{NATIVE.as_posix()}' --state-root '{server.root.as_posix()}' ask worker --project native-test --from lead --chain -- 'Reply exactly CCB_WORKER_OK. Do not call tools or edit files.'"
    prompt=('This is an isolated CCB integration test. Use the bash tool exactly once to execute this command: '+command+
        '\nAfter the submission is accepted, end this response with DELEGATED. Do not wait or poll. '
        'In the later CCB result-chain continuation, read the worker result and reply exactly CCB_LEAD_REVIEW_OK if it contains CCB_WORKER_OK. '
        'Do not run any other tool, command, or modify any file.')
    try:
        receipt=submit(server,'lead',body=prompt)
        result=wait_result(server,receipt['job_id'],callback=True)
        assert 'CCB_LEAD_REVIEW_OK' in result['reply'],result['reply']
        edges=server.rpc('records.latest',dict(stream='callbacks',identity='edge_id'),version=1)['result']
        assert len(edges)==1 and edges[0]['state']=='done'
        metadata=json.loads((server.root/'providers/pi/lead/session.json').read_text())
        assert metadata['state']['messageCount']>=4
    finally:server.close()
