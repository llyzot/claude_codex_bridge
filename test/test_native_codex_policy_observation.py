"""Binding observations come from exact provider responses, not requested defaults."""
import json

from test_native_daemon import Server, submit, await_watch
from test_native_codex_rpc import codex_spec
from test_native_context import provider, control, settled
from test_native_restart import ready


def policy(path, approval='on-request', sandbox='workspaceWrite', **extra):
    path.write_text(json.dumps(dict(approvalPolicy=approval, sandbox={'type':sandbox}, approvalsReviewer='user', **extra)), encoding='utf-8')


def observation(server):
    data = json.loads((server.root/'providers/codex/worker/session.json').read_text(encoding='utf-8'))
    state = data['state']
    observed = state['thread_policy_observation']
    assert observed['thread_id'] == state['thread_id']
    assert observed['scope'] == 'thread_binding_modes'
    assert server.rpc('provider-status', {'agent_name':'worker'})['thread_policy_observation'] == observed
    return observed


def test_binding_observes_reported_policy_and_refreshes_on_resume(tmp_path):
    path=tmp_path/'policy.json'; policy(path, modelProvider='SECRET_PROVIDER_VALUE')
    spec=codex_spec('policy-observation')
    spec.update(env={'CCB_FIXTURE_POLICY':str(path)}, thread_options={'sandbox':'read-only','approvalPolicy':'never'})
    server=Server(tmp_path/'state', {'worker':spec})
    try:
        job=submit(server)['job_id'];await_watch(server,job,lambda r:r['terminal'])
        first=observation(server)
        assert first['sandbox_type']=='workspaceWrite' and first['approval_policy']=='on-request'
        assert first['source']=='thread/start'
        assert 'SECRET_PROVIDER_VALUE' not in json.dumps(first)
    finally:server.close()
    policy(path,approval='untrusted',sandbox='readOnly')
    server=Server(tmp_path/'state',{'worker':spec})
    try:
        job=submit(server)['job_id'];await_watch(server,job,lambda r:r['terminal'])
        resumed=observation(server)
        assert resumed['thread_id']==first['thread_id'] and resumed['source']=='thread/resume'
        assert resumed['sandbox_type']=='readOnly' and resumed['approval_policy']=='untrusted'
        assert resumed['observed_at']!=first['observed_at']
    finally:server.close()


def test_unknown_policy_values_are_not_copied_or_inferred_from_options(tmp_path):
    path=tmp_path/'policy.json';policy(path,approval='SECRET_APPROVAL',sandbox='SECRET_SANDBOX')
    spec=codex_spec('policy-observation');spec.update(env={'CCB_FIXTURE_POLICY':str(path)},thread_options={'sandbox':'workspace-write'})
    server=Server(tmp_path/'state',{'worker':spec})
    try:
        job=submit(server)['job_id'];await_watch(server,job,lambda r:r['terminal'])
        observed=observation(server)
        assert observed['sandbox_type'] is None and observed['approval_policy'] is None
        assert 'SECRET' not in json.dumps(observed)
    finally:server.close()


def test_context_clear_updates_policy_and_compact_keeps_binding_observation(tmp_path):
    path=tmp_path/'policy.json';policy(path)
    spec=provider(tmp_path,kind='codex');spec['env']={'CCB_FIXTURE_POLICY':str(path)}
    server=Server(tmp_path/'state',{'worker':spec})
    try:
        ready(server);before=observation(server)
        policy(path,approval='never',sandbox='readOnly')
        assert settled(server,control(server,'clear'))['status']=='completed'
        ready(server);after=observation(server)
        assert after['thread_id']!=before['thread_id']
        assert after['sandbox_type']=='readOnly' and after['approval_policy']=='never'
        assert settled(server,control(server,'compact'))['status']=='completed'
        ready(server)
        assert observation(server)==after
    finally:server.close()


def test_missing_policy_response_is_explicitly_unobserved(tmp_path):
    server=Server(tmp_path/'state',{'worker':codex_spec()})
    try:
        job=submit(server)['job_id'];await_watch(server,job,lambda r:r['terminal'])
        observed=observation(server)
        assert observed['sandbox_type'] is None and observed['approval_policy'] is None
    finally:server.close()
