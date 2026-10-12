"""The rollout gate checks the filesystem and cleanup, not just turn completion."""
import json
from pathlib import Path
import subprocess
import sys

from native_codex_io_probe import verdict


def evidence(root):
    (root / 'project').mkdir()
    marker = 'CCB_RUST_IO_123456abcdef'
    report = dict(trial_root=str(root), marker=marker, sandbox='workspace-write', approval_policy='on-request',
                  thread_id='fixture-thread', thread_policy_observation={'scope':'thread_binding_modes','thread_id':'fixture-thread','sandbox_type':'workspaceWrite','approval_policy':'on-request'},
                  watch={'status': 'completed', 'terminal': True, 'reply': marker},
                  keeper_status={'alive': False, 'keeper': {'phase': 'stopped'}})
    path = root / 'result.json'
    path.write_text(json.dumps(report), encoding='utf-8')
    return path, report


def test_completed_turn_with_tool_error_is_nonzero(tmp_path):
    path, report = evidence(tmp_path)
    report['watch']['reply'] = 'helper_unknown_error: setup refresh had errors'
    path.write_text(json.dumps(report), encoding='utf-8')
    run = subprocess.run([sys.executable, str(Path(__file__).with_name('native_codex_io_probe.py')),
                          '--verify-result', str(path)], capture_output=True, text=True, encoding='utf-8', timeout=10)
    assert run.returncode == 1
    result = json.loads(run.stdout)
    assert not result['passed'] and result['provider_turn_status'] == 'completed'


def test_success_requires_actual_file_and_confirmed_cleanup(tmp_path):
    path, report = evidence(tmp_path)
    assert not verdict(path)['passed']
    (tmp_path / 'project/probe.txt').write_text(report['marker'] + '\n', encoding='utf-8')
    assert verdict(path)['passed']
    report['keeper_status']['alive'] = True
    path.write_text(json.dumps(report), encoding='utf-8')
    assert not verdict(path)['passed']


def test_relaxed_policy_cannot_pass_the_restricted_gate(tmp_path):
    path, report = evidence(tmp_path)
    (tmp_path / 'project/probe.txt').write_text(report['marker'] + '\n', encoding='utf-8')
    report['sandbox'] = 'danger-full-access'
    path.write_text(json.dumps(report), encoding='utf-8')
    assert not verdict(path)['passed']


def test_provider_response_must_match_requested_restrictions(tmp_path):
    path, report = evidence(tmp_path)
    (tmp_path / 'project/probe.txt').write_text(report['marker'] + '\n', encoding='utf-8')
    report['thread_policy_observation']['sandbox_type'] = 'dangerFullAccess'
    path.write_text(json.dumps(report), encoding='utf-8')
    assert not verdict(path)['passed']
    report['thread_policy_observation']['sandbox_type'] = 'workspaceWrite'
    report['thread_policy_observation']['thread_id'] = 'another-thread'
    path.write_text(json.dumps(report), encoding='utf-8')
    assert not verdict(path)['passed']


def test_account_default_success_is_not_restricted_sandbox_acceptance(tmp_path):
    path, report = evidence(tmp_path)
    (tmp_path / 'project/probe.txt').write_text(report['marker'] + '\n', encoding='utf-8')
    report.update(test_mode='account-default',sandbox=None,approval_policy=None,permission_overrides_supplied=False,
                  account_policy_snapshot={'sandbox_mode':'danger-full-access','approval_policy':'never'})
    report['thread_policy_observation'].update(sandbox_type='dangerFullAccess',approval_policy='never')
    path.write_text(json.dumps(report),encoding='utf-8')
    assert verdict(path,'account-default')['passed']
    assert not verdict(path)['passed']
    report['permission_overrides_supplied']=True
    path.write_text(json.dumps(report),encoding='utf-8')
    assert not verdict(path,'account-default')['passed']
    report['permission_overrides_supplied']=False
    report['thread_policy_observation']['approval_policy']=None
    report['account_policy_snapshot']['approval_policy']=None
    path.write_text(json.dumps(report),encoding='utf-8')
    assert not verdict(path,'account-default')['passed']
