"""Explicit Windows workspace-write acceptance, using existing account auth.

Run with CCB_RUN_REAL_MODELS=1 and optionally CCB_TEST_NATIVE_BIN. Does not
stop shared tools, copy credentials, change account policy or switch defaults.
--verify-result rechecks retained evidence without starting a provider.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
import tomllib
import uuid

from native_keeper_live_probe import cli, CODEX, NATIVE


def verdict(path):
    report = json.loads(path.read_text(encoding='utf-8'))
    marker = report.get('marker')
    project = Path(report['trial_root']) / 'project'
    file = project / 'probe.txt'
    keeper = report.get('keeper_status') or {}
    stopped = keeper.get('alive') is False and keeper.get('keeper', {}).get('phase') == 'stopped'
    terminal = report.get('watch', {})
    observed = report.get('thread_policy_observation') or {}
    passed = (
        isinstance(marker, str) and bool(re.fullmatch(r'CCB_RUST_IO_[a-f0-9]{12}', marker))
        and report.get('sandbox') == 'workspace-write'
        and report.get('approval_policy') == 'on-request'
        and observed.get('sandbox_type') == 'workspaceWrite'
        and observed.get('approval_policy') == 'on-request'
        and observed.get('scope') == 'thread_binding_modes'
        and observed.get('thread_id') == report.get('thread_id') and bool(report.get('thread_id'))
        and terminal.get('terminal') is True and terminal.get('status') == 'completed'
        and terminal.get('reply', '').strip() == marker
        and file.is_file() and file.read_text(encoding='utf-8-sig') == marker + '\n'
        and stopped and not report.get('error') and not report.get('pending_methods')
    )
    return {'passed': bool(passed), 'result': str(path), 'provider_turn_status': terminal.get('status'),
            'keeper_stopped': stopped, 'file_verified': bool(file.is_file() and marker
                and file.read_text(encoding='utf-8-sig') == marker + '\n')}


def run():
    assert os.name == 'nt' and os.environ.get('CCB_RUN_REAL_MODELS') == '1', 'Explicit Windows real-model opt-in required'
    assert NATIVE.is_file() and CODEX.is_file()
    trial = Path(tempfile.mkdtemp(prefix='ccb-native-io-', dir=Path.home() / 'Desktop/work'))
    project = trial / 'project'
    project.mkdir()
    subprocess.run(['git', 'init', '--quiet', str(project)], check=True)
    state = project / '.ccb/ccbd'
    home = Path.home() / '.codex'
    command = [str(CODEX)]
    for name in tomllib.loads((home / 'config.toml').read_text(encoding='utf-8')).get('mcp_servers', {}):
        assert re.fullmatch(r'[A-Za-z0-9_-]+', name)
        command += ['-c', f'mcp_servers.{name}.enabled=false']
    command += ['app-server', '--stdio']
    worker = dict(provider='codex', driver_kind='codex_app_server', profile_family='protocol_turn',
                  workspace_path=str(project), command=command, env={'CODEX_HOME': str(home)},
                  thread_options={'model': 'gpt-6.1-sol', 'approvalPolicy': 'on-request', 'sandbox': 'workspace-write'},
                  turn_options={'effort': 'low'}, timeout_s=120)
    config = dict(schema_version=2, project_id=uuid.uuid4().hex, project_root=str(project), agents={'worker': worker})
    launch = trial / 'launch.json'
    launch.write_text(json.dumps(config, indent=2), encoding='utf-8')
    marker = 'CCB_RUST_IO_' + uuid.uuid4().hex[:12]
    report = dict(trial_root=str(trial), marker=marker, sandbox='workspace-write', approval_policy='on-request',
                  binary_sha256=hashlib.sha256(NATIVE.read_bytes()).hexdigest(),
                  existing_settings_modified=False, credentials_copied=False, shared_processes_stopped=False)
    result = trial / 'result.json'

    def save():
        result.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')

    save()
    print(json.dumps({'trial_root': str(trial), 'phase': 'starting'}), flush=True)
    try:
        report['start'] = cli('--state-root', state, 'start', '--config', launch)
        save()
        # Require provider evidence before any model request. A configuration
        # declaration alone does not prove that the provider applied it.
        metadata = state / 'providers/codex/worker/session.json'
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if metadata.is_file():
                bound = json.loads(metadata.read_text(encoding='utf-8')).get('state', {})
                if bound.get('thread_id'):
                    report['thread_id'] = bound['thread_id']
                    report['thread_policy_observation'] = bound.get('thread_policy_observation')
                    break
            time.sleep(.1)
        observed = report.get('thread_policy_observation') or {}
        if observed.get('sandbox_type') != 'workspaceWrite' or observed.get('approval_policy') != 'on-request':
            raise RuntimeError('Provider did not confirm requested restricted thread policy; no model task submitted')
        prompt = (f'In this disposable test workspace, use your normal file or shell tools to create probe.txt containing exactly {marker} and a newline, '
                  'then read it back using a tool. Do not access files outside this workspace. Do not request escalated execution or change any security settings. '
                  'If a tool fails, stop and report that error. Reply with the file contents only after both operations succeed.')
        report['job_id'] = cli('--state-root', state, 'ask', 'worker', '--', prompt)['job_id']
        save()
        deadline = time.monotonic() + 145
        while time.monotonic() < deadline:
            report['watch'] = cli('--state-root', state, 'watch', report['job_id'])
            if report['watch'].get('terminal'):
                break
            pending = cli('--state-root', state, 'request', 'provider-requests', json.dumps({'agent_name': 'worker'}))
            report['pending_methods'] = [r.get('method') for r in pending['requests']]
            if pending['requests']:
                report['error'] = 'Provider requests user input; no automatic approval supplied'
                break
            time.sleep(1)
        else:
            report['error'] = 'Observation expired; original job retained, no replacement submitted'
    except Exception as exc:
        report['error'] = str(exc)
    finally:
        save()
        # This state root was created by this probe. Cleanup is attempted even
        # if startup acknowledgment was lost; never launch a replacement.
        try:
            report['stop'] = cli('--state-root', state, 'stop', ok=False)
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                status = cli('--state-root', state, 'keeper-status', ok=False)
                report['keeper_status'] = status
                if status and status.get('alive') is False:
                    break
                time.sleep(.5)
        except Exception as exc:
            report['cleanup_error'] = str(exc)
        save()
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--verify-result', type=Path)
    args = parser.parse_args()
    result = verdict(args.verify_result or run())
    print(json.dumps(result, ensure_ascii=False), flush=True)
    raise SystemExit(0 if result['passed'] else 1)
