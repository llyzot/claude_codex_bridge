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
