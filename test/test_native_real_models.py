"""Explicit opt-in real model checks in new private homes/workspaces only."""
import json
import os
from pathlib import Path
# Captured at import: test fixtures isolate HOME/USERPROFILE per test.
REAL_HOME = Path.home()
import subprocess
import time

import pytest
from test_native_daemon import Server,submit

ROOT=Path(__file__).resolve().parents[1]
NATIVE=Path(os.environ.get('CCB_TEST_NATIVE_BIN') or ROOT/'rust'/'target'/'debug'/'ccb-native.exe')
CODEX=(REAL_HOME/'scoop/apps/nodejs-lts/current/bin/node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe')
NODE=(REAL_HOME/'scoop/apps/nodejs-lts/current/node.exe')
PI=(REAL_HOME/'scoop/apps/nodejs-lts/current/bin/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js')
pytestmark=pytest.mark.skipif(os.environ.get('CCB_RUN_REAL_MODELS')!='1',reason='Explicit real-model opt-in required')


def private_claude_context_config(tmp_path,base_url=None):
    workspace=tmp_path/'workspace';workspace.mkdir()
    home=tmp_path/'claude-home';home.mkdir()
    return dict(provider='claude',driver_kind='claude_stream',profile_family='protocol_turn',workspace_path=str(workspace),
        command=[str(REAL_HOME/'.local/bin/claude.exe'),'-p','--input-format','stream-json','--output-format','stream-json',
                 '--verbose','--replay-user-messages','--bare','--setting-sources','','--strict-mcp-config','--mcp-config','{"mcpServers":{}}','--tools',''],
        env={'CLAUDE_CONFIG_DIR':str(home),'ANTHROPIC_API_KEY':'local-test-key' if base_url else '',
             'ANTHROPIC_AUTH_TOKEN':'','CLAUDE_CODE_OAUTH_TOKEN':'',
             'ANTHROPIC_BASE_URL':base_url or 'https://api.anthropic.com',
             'DISABLE_AUTOUPDATER':'1','CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC':'1'},timeout_s=60)


def test_real_claude_clear_and_empty_compact_without_model(tmp_path):
    from test_native_restart import ready
    from test_native_restart import settled as restart_settled
    from test_native_context import control,settled
    config=private_claude_context_config(tmp_path)
    server=Server(tmp_path/'runtime/ccbd',{'worker':config})
    try:
        before=ready(server)
        result=settled(server,control(server))
        assert result['status']=='completed',result
        assert ready(server)['runtime_instance_id']==before['runtime_instance_id']
        restarted=restart_settled(server,server.rpc('restart',{'agent_names':['worker']})['reload'])
        assert restarted['status']=='applied'
        assert restarted['idle_provider_shutdown']['worker']['status']=='exited'
        no_op=settled(server,control(server,'compact'))
        assert no_op['results'][0]['completion_reason']=='claude_context_compaction_not_performed',no_op
        assert settled(server,control(server))['status']=='completed'
    finally:server.close()
    restored=Server(server.root,{'worker':config})
    try:
        ready(restored)
        no_op=settled(restored,control(restored,'compact'))
        assert no_op['results'][0]['completion_reason']=='claude_context_compaction_not_performed',no_op
        assert settled(restored,control(restored))['status']=='completed'
    finally:restored.close()


def test_real_claude_compact_against_local_protocol_backend(tmp_path):
    """Exercises the actual CLI compactor; generated text comes from a local fixture."""
    from stubs.anthropic_context_backend import Backend
    from test_native_restart import ready
    from test_native_context import control,settled
    backend=Backend()
    server=None
    try:
        config=private_claude_context_config(tmp_path,backend.url)
        server=Server(tmp_path/'runtime/ccbd',{'worker':config})
        for n in range(2):
            receipt=submit(server,body='Disposable public context test. '*70+f' Reply to turn {n}.')
            row=wait_result(server,receipt['job_id'])
            assert row['status']=='completed',{'status':row['status'],'reason':row.get('completion_reason'),'paths':backend.paths}
        before=ready(server);first=len(backend.responses)
        result=settled(server,control(server,'compact'),timeout=60)
        assert result['status']=='completed',result
        assert len(backend.responses)>first,'Compaction must call the local fixture backend'
        summaries=backend.responses[first:].copy()
        assert ready(server)['runtime_instance_id']==before['runtime_instance_id']
        next_request=len(backend.requests)
        row=wait_result(server,submit(server,body='Continue after compaction.')['job_id'])
        assert row['status']=='completed'
        payloads=json.dumps(backend.requests[next_request:])
        assert any(summary in payloads for summary in summaries),'CLI must use its generated compaction summary in later history'
    finally:
        if server:server.close()
        backend.close()


@pytest.mark.parametrize('kind',['pi','codex'])
def test_real_context_clear_without_model_or_credentials(tmp_path,kind):
    """Only state/new-session RPCs; private empty homes, no model turn or auth copy."""
    from test_native_restart import ready
    from test_native_context import control,settled
    workspace=tmp_path/'workspace';workspace.mkdir()
    home=tmp_path/'provider-home';home.mkdir()
    if kind=='pi':
        config=dict(provider='pi',driver_kind='pi_rpc',profile_family='protocol_turn',
            command=[str(NODE),str(PI),'--mode','rpc','--no-extensions','--no-skills','--no-prompt-templates',
                     '--no-themes','--session-dir',str(home/'sessions')],workspace_path=str(workspace),
            env={'PI_CODING_AGENT_DIR':str(home),'PI_SKIP_VERSION_CHECK':'1','PI_TELEMETRY':'0'})
    else:
        config=dict(provider='codex',driver_kind='codex_app_server',profile_family='protocol_turn',
            command=[str(CODEX),'app-server','--stdio'],workspace_path=str(workspace),env={'CODEX_HOME':str(home)},
            thread_options={'approvalPolicy':'never','sandbox':'read-only'})
    server=Server(tmp_path/'runtime/ccbd',{'worker':config})
    try:
        before=ready(server)
        result=settled(server,control(server,'clear'))
        assert result['status']=='completed',result
        assert ready(server)['runtime_instance_id']==before['runtime_instance_id']
        auth=home/'auth.json'
        # Pi initializes an empty credential store even without authentication.
        empty_credentials=not auth.exists() or json.loads(auth.read_text())=={}
        assert empty_credentials,'Provider unexpectedly populated the private credential store'
    finally:server.close()
    restored=Server(server.root,{'worker':config})
    try:
        assert ready(restored)
        assert restored.rpc('context-status',{'operation_id':result['operation_id']})['context']['status']=='completed'
    finally:restored.close()

def protected_copy(source,destination):
    result=subprocess.run([str(NATIVE),'private-copy',str(source),str(destination)],capture_output=True,text=True,timeout=8)
    assert result.returncode==0,result.stderr

def configs(tmp_path):
    workspace=tmp_path/'workspace';workspace.mkdir()
    codex_home=tmp_path/'codex-home';codex_home.mkdir()
    protected_copy((REAL_HOME/'.codex/auth.json'),codex_home/'auth.json')
    worker=dict(provider='codex',driver_kind='codex_app_server',profile_family='protocol_turn',workspace_path=str(workspace),
        command=[str(CODEX),'app-server','--stdio'],env={'CODEX_HOME':str(codex_home)},
        thread_options={'model':'gpt-6.1-sol','approvalPolicy':'never','sandbox':'read-only'},turn_options={'effort':'low'})
    pi_home=tmp_path/'pi-home';pi_home.mkdir()
    shell=(REAL_HOME/'scoop/apps/git/current/usr/bin/bash.exe')
    assert shell.is_file(),'Existing Git Bash required for the Pi bash tool'
    (pi_home/'settings.json').write_text(json.dumps({'shellPath':str(shell)}),encoding='utf-8')
    protected_copy((Path.home()/'.pi/agent/auth.json'),pi_home/'auth.json')
    for name in ['models.json']:
        source=(Path.home()/'.pi/agent')/name
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


@pytest.mark.parametrize('kind',['pi','codex'])
def test_real_native_compaction_preserves_conversation_and_process(tmp_path,kind):
    from test_native_restart import ready
    from test_native_context import control,settled
    worker,lead=configs(tmp_path)
    provider=lead if kind=='pi' else worker
    if kind=='pi':
        settings=tmp_path/'pi-home/settings.json'
        data=json.loads(settings.read_text())
        data['compaction']={'enabled':True,'reserveTokens':1024,'keepRecentTokens':64}
        settings.write_text(json.dumps(data))
    server=Server(tmp_path/'state',{'worker':provider})
    try:
        for i in range(2):
            receipt=submit(server,body=('This is a disposable context-control validation transcript. '*40)+
                f'\nRemember verification word CCB_COMPACT_OK. Reply only RECEIVED_{i}. Do not call tools or edit files.')
            row=wait_result(server,receipt['job_id'])
            assert row['status']=='completed',row['reply']
        before=ready(server)
        operation=control(server,'compact')
        result=settled(server,operation,timeout=150)
        assert result['status']=='completed',result
        assert ready(server)['runtime_instance_id']==before['runtime_instance_id']
        receipt=submit(server,body='What verification word were you asked to remember? Reply only that word. Do not call tools.')
        row=wait_result(server,receipt['job_id'])
        assert row['status']=='completed' and 'CCB_COMPACT_OK' in row['reply'],row['reply']
    finally:server.close()


def test_real_codex_role_skill_reads_updated_body_in_same_thread(tmp_path):
    import uuid
    workspace=tmp_path/'workspace';workspace.mkdir()
    home=tmp_path/'codex-home';home.mkdir()
    protected_copy((REAL_HOME/'.codex/auth.json'),home/'auth.json')
    role=workspace/'roles/installed/agentroles.verify/current'
    skill=role/'skills/ccb-role-probe';skill.mkdir(parents=True)
    (role/'role.toml').write_text('schema="rolepack/v1"\nid="agentroles.verify"\n'
        '[skills]\ncodex=["skills/ccb-role-probe"]\n',encoding='utf-8')
    agent=dict(provider='codex',driver_kind='codex_app_server',profile_family='protocol_turn',
        role='agentroles.verify',workspace_path=str(workspace),command=[str(CODEX),'app-server','--stdio'],
        env={'CODEX_HOME':str(home)},thread_options={'model':'gpt-6.1-sol','approvalPolicy':'never','sandbox':'read-only'},
        turn_options={'effort':'low'},timeout_s=120)
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        previous=None
        for suffix in ('first','second'):
            token='CCB_ROLE_'+uuid.uuid4().hex
            (skill/f'{suffix}.txt').write_text(token,encoding='utf-8')
            (skill/'SKILL.md').write_text('---\nname: ccb-role-probe\n'
                'description: Read the verification token when asked to check the CCB role token.\n---\n'
                f'Read ./{suffix}.txt relative to this skill directory. Reply only with the file contents.\n',encoding='utf-8')
            receipt=submit(server,body='Use $ccb-role-probe to read the current CCB role verification token. '
                'Read the current skill instructions and referenced file each time. Reply only with the token. '
                'Do not edit files or make network requests.')
            result=wait_result(server,receipt['job_id'])
            assert result['status']=='completed', result['reply']
            assert result['reply'].strip()==token, result['reply']
            metadata=json.loads((server.root/'providers/codex/worker/session.json').read_text())
            assert metadata['state']['role_skills']['verified'] is True
            if previous:
                assert metadata['runtime_instance_id']==previous['runtime_instance_id']
                assert metadata['state']['thread_id']==previous['state']['thread_id']
            previous=metadata
    finally:server.close()


def test_real_pi_role_skill_reads_updated_body_in_same_session(tmp_path):
    import uuid
    workspace=tmp_path/'workspace';workspace.mkdir()
    home=tmp_path/'pi-home';home.mkdir()
    protected_copy((Path.home()/'.pi/agent/auth.json'),home/'auth.json')
    models=(Path.home()/'.pi/agent/models.json')
    if models.exists():protected_copy(models,home/'models.json')
    role=workspace/'roles/installed/agentroles.verify/current'
    skill=role/'skills/ccb-role-probe';skill.mkdir(parents=True)
    (role/'role.toml').write_text('schema="rolepack/v1"\nid="agentroles.verify"\n'
        '[skills]\npi=["skills/ccb-role-probe"]\n',encoding='utf-8')
    agent=dict(provider='pi',driver_kind='pi_rpc',profile_family='protocol_turn',
        role='agentroles.verify',workspace_path=str(workspace),
        command=[str(NODE),str(PI),'--mode','rpc','--provider','openai','--model','gpt-6-astra','--thinking','low',
                 '--no-extensions','--no-skills','--no-prompt-templates','--no-themes','--session-dir',str(home/'sessions'),'--tools','read'],
        env={'PI_CODING_AGENT_DIR':str(home),'PI_SKIP_VERSION_CHECK':'1','PI_TELEMETRY':'0'},timeout_s=120)
    server=Server(tmp_path/'state',{'worker':agent})
    try:
        previous=None
        for suffix in ('first','second'):
            token='CCB_PI_ROLE_'+uuid.uuid4().hex
            (skill/f'{suffix}.txt').write_text(token,encoding='utf-8')
            (skill/'SKILL.md').write_text('---\nname: ccb-role-probe\n'
                'description: Read the verification token when asked to check the CCB role token.\n---\n'
                f'Read ./{suffix}.txt relative to this skill directory. Reply only with the file contents.\n',encoding='utf-8')
            receipt=submit(server,body='Use the ccb-role-probe skill to read the current CCB role verification token. '
                'Read the current SKILL.md and referenced file each time. Reply only with the token. '
                'Use only the read tool. Do not edit files or make network requests.')
            result=wait_result(server,receipt['job_id'])
            assert result['status']=='completed',result['reply']
            assert result['reply'].strip()==token,result['reply']
            metadata=json.loads((server.root/'providers/pi/worker/session.json').read_text())
            assert metadata['state']['role_skills']['verified'] is True
            if previous:
                assert metadata['runtime_instance_id']==previous['runtime_instance_id']
                assert metadata['state']['sessionId']==previous['state']['sessionId']
            previous=metadata
    finally:server.close()
