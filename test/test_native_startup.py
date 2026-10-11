"""Native project startup and owner fences in independent temporary projects."""
import json
import os
from pathlib import Path
import subprocess
import time

import pytest
from test_native_cli_security import BIN, cli
from test_native_daemon import spec
from test_native_claude_stream import STUB


def configuration(tmp_path, project='startup-test'):
    path = tmp_path / 'daemon.json'
    path.write_text(json.dumps(dict(schema_version=2, project_id=project, agents={'worker': spec()})), encoding='utf-8')
    return path


def start(root, config, **kwargs):
    result = cli('--state-root', root, 'start', '--config', config, **kwargs)
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


def stop(root, **kwargs):
    result = cli('--state-root', root, 'stop', **kwargs)
    assert result.returncode == 0, result.stderr


def test_background_start_survives_parent_and_ordinary_ask_infers_project_id(tmp_path):
    root = tmp_path / 'state'
    created = start(root, configuration(tmp_path))
    try:
        assert created['status'] == 'running'
        ping = cli('--state-root', root, 'ping')
        assert json.loads(ping.stdout)['pid'] == created['pid']
        submitted = cli('--state-root', root, 'ask', 'worker', 'hello')
        assert submitted.returncode == 0, submitted.stderr
        watched = cli('--state-root', root, 'watch', json.loads(submitted.stdout)['job_id'])
        assert json.loads(watched.stdout)['status'] == 'accepted'
        assert not (root / 'daemon.json').exists()
    finally:
        stop(root)


def test_repeated_start_attaches_same_daemon_and_changed_config_does_not_replace_it(tmp_path):
    root = tmp_path / 'state'
    config = configuration(tmp_path)
    first = start(root, config)
    try:
        second = start(root, config)
        assert second['already_running'] is True and second['pid'] == first['pid']
        value = json.loads(config.read_text())
        value['agents']['worker']['model'] = 'changed'
        config.write_text(json.dumps(value))
        refused = cli('--state-root', root, 'start', '--config', config)
        assert refused.returncode != 0 and 'configuration changed' in refused.stderr
        assert json.loads(cli('--state-root', root, 'ping').stdout)['pid'] == first['pid']
    finally:
        stop(root)


def test_readiness_observation_expiry_reuses_live_startup_owner(tmp_path):
    root = tmp_path / 'state'
    config = configuration(tmp_path)
    result = cli('--state-root', root, 'start', '--config', config, '--timeout-ms', '0')
    assert result.returncode == 0, result.stderr
    first = json.loads(result.stdout)
    assert first['status'] in {'starting', 'running'}
    try:
        second = start(root, config)
        assert second['status'] == 'running'
        assert second['pid'] == first['pid']
        assert second['startup_id'] == first['startup_id']
    finally:
        stop(root)

@pytest.mark.skipif(os.name!='nt',reason='Windows process lifetime evidence')
def test_stale_start_intent_does_not_adopt_recycled_pid(tmp_path):
    import sys
    root=tmp_path/'state';config=configuration(tmp_path);created=start(root,config)
    old=json.loads(cli('process-identity',created['pid']).stdout)
    stop(root)
    until=time.monotonic()+5
    while cli('process-identity',created['pid']).returncode==0 and time.monotonic()<until:time.sleep(.03)
    helper=subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(30)'],creationflags=0x08000000)
    try:
        path=root/'kernel/native-start.json';intent=json.loads(path.read_text(encoding='utf-8'))
        intent['pid']=helper.pid;intent['process']={**old,'pid':helper.pid};path.write_text(json.dumps(intent),encoding='utf-8')
        result=start(root,config)
        assert result['status']=='running' and result['pid']!=helper.pid
        assert helper.poll() is None
        stop(root)
    finally:
        helper.kill();helper.wait(timeout=5)


def test_foreign_project_cannot_adopt_a_live_state_root(tmp_path):
    root = tmp_path / 'state'
    first = start(root, configuration(tmp_path))
    try:
        other = configuration(tmp_path, 'foreign-project')
        rejected = cli('--state-root', root, 'start', '--config', other)
        assert rejected.returncode != 0 and 'another service or project' in rejected.stderr
        assert json.loads(cli('--state-root', root, 'ping').stdout)['pid'] == first['pid']
    finally:
        stop(root)


def project(tmp_path):
    root = tmp_path / 'project with spaces'
    (root / '.ccb').mkdir(parents=True)
    (root / '.ccb/ccb.config').write_text('cmd; worker:claude', encoding='utf-8')
    (root / 'nested').mkdir()
    return root


def test_project_compact_config_starts_native_provider_from_nested_directory(tmp_path):
    import sys
    root = project(tmp_path)
    manifest = tmp_path / 'executables.json'
    manifest.write_text(json.dumps({'claude': [sys.executable, '-I', str(STUB), 'worker']}))
    result = cli('start', '--executables', manifest, cwd=root / 'nested')
    assert result.returncode == 0, result.stderr
    started = json.loads(result.stdout)
    state = root / '.ccb/ccbd'
    try:
        assert os.path.samefile(started['state_root'], state)
        receipt = cli('ask', 'worker', 'task', cwd=root / 'nested')
        assert receipt.returncode == 0, receipt.stderr
        job = json.loads(receipt.stdout)['job_id']
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            row = json.loads(cli('watch', job, cwd=root / 'nested').stdout)
            if row['terminal']:
                break
            time.sleep(.05)
        assert row['reply'] == 'Claude final result'
        assert (root / '.ccb/ccb.config').read_text() == 'cmd; worker:claude'
    finally:
        stop(state)


@pytest.mark.parametrize('relocation', ['env', 'reference'])
def test_project_state_placement_matches_existing_rules(tmp_path, relocation):
    import sys
    from project.ids import compute_legacy_project_id
    root = project(tmp_path)
    project_id = compute_legacy_project_id(root)
    manifest = tmp_path / 'executables.json'
    manifest.write_text(json.dumps({'claude': [sys.executable, '-I', str(STUB), 'worker']}))
    env = os.environ.copy()
    env.pop('CCB_RUNTIME_STATE_HOME', None)
    destination = tmp_path / 'relocated'
    if relocation == 'env':
        env['CCB_RUNTIME_STATE_HOME'] = str(destination)
        destination = destination / project_id
    else:
        (root / '.ccb/runtime-root-ref.json').write_text(json.dumps({'record_type': 'ccb_runtime_root_ref', 'project_id': project_id, 'runtime_state_root': str(destination)}))
    result = cli('--project', root, 'start', '--executables', manifest, env=env)
    assert result.returncode == 0, result.stderr
    try:
        assert os.path.samefile(json.loads(result.stdout)['state_root'], destination / 'ccbd')
        assert not (root / '.ccb/ccbd').exists()
    finally:
        stop(destination / 'ccbd', env=env)


def test_missing_provider_is_preflight_failure_without_runtime_mutation(tmp_path):
    root = project(tmp_path)
    env = os.environ.copy()
    env['PATH'] = ''
    env['USERPROFILE'] = str(tmp_path / 'empty-home')
    env['HOME'] = str(tmp_path / 'empty-home')
    env.pop('CCB_NATIVE_TOKEN_FILE', None)
    env.pop('CCB_RUNTIME_STATE_HOME', None)
    result = cli('--project', root, 'start', env=env)
    assert result.returncode != 0 and 'executable not found' in result.stderr
    assert not (root / '.ccb/ccbd').exists()


@pytest.mark.skipif(os.name != 'nt', reason='Windows package layout discovery')
def test_discovery_reads_existing_node_packages_and_native_claude_without_execution(tmp_path):
    root = project(tmp_path)
    (root / '.ccb/ccb.config').write_text('lead:pi; worker:codex; reviewer:claude')
    node = tmp_path / 'node'
    node.mkdir()
    (node / 'node.exe').write_bytes(b'not executable')
    modules = node / 'bin/node_modules'
    pi = modules / '@earendil-works/pi-coding-agent/dist/bundle/cli.js'
    pi.parent.mkdir(parents=True)
    pi.write_text('not executed')
    codex = modules / '@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe'
    codex.parent.mkdir(parents=True)
    codex.write_bytes(b'not executable')
    home = tmp_path / 'home'
    (home / '.local/bin').mkdir(parents=True)
    (home / '.local/bin/claude.exe').write_bytes(b'not executable')
    env = {**os.environ, 'PATH': str(node), 'USERPROFILE': str(home)}
    output = cli('config', 'compile', root, env=env)
    assert output.returncode == 0, output.stderr
    agents = json.loads(output.stdout)['agents']
    assert Path(agents['lead']['command'][0]) == node / 'node.exe'
    assert Path(agents['worker']['command'][0]) == codex
    assert Path(agents['reviewer']['command'][0]) == home / '.local/bin/claude.exe'


def test_invalid_configuration_is_rejected_before_runtime_files_are_created(tmp_path):
    root = tmp_path / 'state'
    config = configuration(tmp_path)
    value = json.loads(config.read_text())
    value['agents']['worker']['env'] = {'INVALID': 7}
    config.write_text(json.dumps(value))
    result = cli('--state-root', root, 'start', '--config', config)
    assert result.returncode != 0 and 'environment' in result.stderr
    assert not root.exists()


def test_two_start_clients_publish_only_one_live_daemon(tmp_path):
    root = tmp_path / 'state'
    config = configuration(tmp_path)
    args = [str(BIN), '--state-root', str(root), 'start', '--config', str(config)]
    processes = [subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, creationflags=0x08000000 if os.name == 'nt' else 0) for _ in range(2)]
    responses = []
    try:
        for process in processes:
            stdout, stderr = process.communicate(timeout=8)
            assert process.returncode == 0, stderr
            responses.append(json.loads(stdout))
        running = start(root, config)
        published = [row['pid'] for row in responses if row.get('pid')]
        assert published and set(published) == {running['pid']}
        assert all(row['status'] in {'running', 'startup_in_progress'} for row in responses)
    finally:
        stop(root)
        for process in processes:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=3)


def test_agent_environment_can_route_ask_without_project_directory(tmp_path):
    root = tmp_path / 'state'
    config = configuration(tmp_path)
    document = json.loads(config.read_text())
    document['agents']['lead'] = spec()
    config.write_text(json.dumps(document))
    start(root, config)
    try:
        endpoint = json.loads((root / 'kernel/control-plane.json').read_text())
        env = {**os.environ, 'CCB_NATIVE_TOKEN_FILE': endpoint['token_ref'], 'CCB_AGENT_NAME': 'lead'}
        result = cli('ask', 'worker', 'delegated task', env=env, cwd=tmp_path)
        assert result.returncode == 0, result.stderr
        receipt = json.loads(result.stdout)
        row = cli('--state-root', root, 'watch', receipt['job_id'])
        assert json.loads(row.stdout)['status'] == 'accepted'
    finally:
        stop(root)
