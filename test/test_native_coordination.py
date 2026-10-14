"""Native turns carry exact runtime routing and asynchronous collaboration rules."""
import json
import os
from pathlib import Path
import pytest
from test_native_daemon import Server, spec, submit, await_watch
from test_native_role_memory import FACTORIES, prompts


def binding(text):
    return json.loads(text.split('Current runtime binding (data):\n', 1)[1].split('\n', 1)[0])


@pytest.mark.parametrize('explicit',[True,False])
def test_codex_thread_binding_has_coordination_before_any_turn(tmp_path,explicit):
    from test_native_codex_rpc import codex_spec
    from test_native_herdr_worker import wait_for
    trace=tmp_path/'startup.jsonl'
    source=dict(codex_spec(),env={'CCB_FIXTURE_TRACE':str(trace),'CCB_UNRELATED_SECRET':'DO_NOT_INCLUDE',
                'CCB_FIXTURE_DEVELOPER_INSTRUCTIONS':'USER_GLOBAL_SENTINEL'},
                thread_options={'developerInstructions':'USER_RULE_SENTINEL'} if explicit else {})
    server=Server(tmp_path/'state',{'worker':source,'peer':spec()})
    try:
        def bound():
            if not trace.exists():return None
            rows=[json.loads(line) for line in trace.read_text(encoding='utf-8').splitlines() if line.strip()]
            return next((r for r in rows if r.get('method') in ['thread/start','thread/resume']),None)
        row=wait_for(bound)
        context=row['params']['developerInstructions']
        assert context.startswith('USER_RULE_SENTINEL' if explicit else 'USER_GLOBAL_SENTINEL')
        assert '<ccb_interactive_coordination>' in context and 'peer' in context
        assert 'DO_NOT_INCLUDE' not in context and server.token.token not in context
    finally:server.close()


@pytest.mark.parametrize('provider', FACTORIES)
def test_turn_gets_bound_runtime_and_enabled_peers_without_credentials(tmp_path, provider):
    trace = tmp_path / 'wire.jsonl'
    source = dict(FACTORIES[provider](), env={'CCB_FIXTURE_TRACE': str(trace), 'CCB_UNRELATED_SECRET': 'NEVER_PROMPT_THIS_SECRET'})
    server = Server(tmp_path / "runtime with ' quote/ccbd", {'worker': source, 'peer': spec(), 'disabled': dict(spec(), dispatch_disabled=True)})
    try:
        receipt = submit(server)
        assert await_watch(server, receipt['job_id'], lambda r: r['terminal'])['status'] == 'completed'
        text = prompts(trace, provider)[-1]
        data = binding(text)
        assert data['agent_name'] == 'worker' and data['job_id'] == receipt['job_id']
        assert data['project_id'] == 'native-test' and data['original_caller'] == 'user'
        assert data['available_chain_targets'] == ['peer']
        assert [p['name'] for p in data['configured_peers']] == ['peer']
        argv = data['command_prefix_argv']
        assert Path(argv[0]).name == ('ccb-native.exe' if os.name == 'nt' else 'ccb-native')
        assert argv[1] == '--state-root' and Path(argv[2]).resolve() == server.root.resolve()
        assert 'end the current agent turn immediately' in text
        assert 'uncertain is not permission to repeat' in text
        assert 'NEVER_PROMPT_THIS_SECRET' not in text
        assert server.token.token not in text and server.token.token_ref not in text
        records = server.rpc('records.latest', {'stream': 'jobs', 'agent_name': 'worker', 'identity': 'job_id'}, version=1)['result']
        assert records[0]['request']['body'] == 'task'
        # Execute the generated PowerShell example against the real CLI. Values
        # are replaced only at its literal placeholder arguments, without eval.
        if os.name == 'nt':
            import subprocess
            line = text.split('PowerShell: ', 1)[1].split('\n', 1)[0]
            # A terminal worker cannot chain; use a normal user submission to
            # verify exact quoted root/argument routing, including apostrophes.
            command = line.replace("'ask' '--chain' '--from' 'worker' 'TARGET' '--' 'MESSAGE'", "'ask' '--from' 'user' 'peer' '--' 'literal $() ; & payload'")
            result = subprocess.run(['pwsh', '-NoProfile', '-Command', command], capture_output=True, text=True, encoding='utf-8', timeout=10)
            assert result.returncode == 0, result.stderr
            job = server.rpc('work', {'agent_name': 'peer'})['job']
            assert job['request']['body'] == 'literal $() ; & payload'
    finally:
        server.close()


def test_chain_target_restriction_is_reflected_in_guidance(tmp_path):
    trace = tmp_path / 'wire.jsonl'
    server = Server(tmp_path / 'state', {'worker': dict(FACTORIES['codex'](), env={'CCB_FIXTURE_TRACE': str(trace)}), 'peer': spec(), 'other': spec()})
    try:
        receipt = server.rpc('submit', {'project_id': 'native-test', 'to_agent': 'worker', 'from_actor': 'user', 'body': 'restricted task', 'route_options': {'allowed_chain_targets': ['PEER']}})
        await_watch(server, receipt['job_id'], lambda r: r['terminal'])
        data = binding(prompts(trace, 'codex')[-1])
        assert data['chain_restricted'] and data['available_chain_targets'] == ['peer']
    finally:
        server.close()


def test_reply_delivery_uses_plain_ask_for_the_next_requested_stage(tmp_path):
    from test_native_codex_rpc import codex_spec
    from test_native_project_view import finish
    from test_native_herdr_worker import wait_for
    trace=tmp_path/'reply.jsonl'
    server=Server(tmp_path/'state',{'lead':dict(codex_spec('followup-hold'),env={'CCB_FIXTURE_TRACE':str(trace)}),
                                  'claude':spec(),'codex':spec()})
    try:
        receipt=server.rpc('submit',dict(project_id='native-test',to_agent='claude',from_actor='lead',body='Analyze, then Codex must fix.'))
        first=server.rpc('work',{'agent_name':'claude'})['job'];finish(server,first)
        sent=wait_for(lambda:prompts(trace,'codex'))[-1]
        data=binding(sent)
        assert data['request_kind']=='reply_delivery' and data['required_result_mode']=='plain'
        assert "'ask' '--from' 'lead'" in sent
        next_job=server.rpc('submit',dict(project_id='native-test',to_agent='codex',from_actor='lead',body='Implement the analysis findings.'))
        assert next_job['job_id']!=receipt['job_id']
        assert server.rpc('work',{'agent_name':'codex'})['job']['request']['from_actor']=='lead'
    finally:server.close()
