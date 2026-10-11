"""Project role identities resolve through the native configuration/authority."""
import json
from pathlib import Path
from types import SimpleNamespace
import sys
import os

import pytest
from agents.config_loader_runtime.io_runtime.documents import _parse_compact_config_document
from cli.services.ask_runtime.submission import _resolve_target
from test_native_config import native
from test_native_cli_security import cli
from test_native_daemon import Server, spec
from test_native_claude_stream import STUB

def role(root, identity='agentroles.archi', name='architect', *, adapter=False, current=False):
    directory = root / identity
    if current: directory = directory / 'current'
    directory.mkdir(parents=True)
    naming = '' if adapter else f'\n[identity]\ndefault_name="{name}"\n'
    (directory / 'role.toml').write_text(f'id="{identity}"\n' + naming, encoding='utf-8')
    if adapter:
        (directory / 'adapters/ccb').mkdir(parents=True)
        (directory / 'adapters/ccb/adapter.toml').write_text(f'default_agent_name="{name}"\n', encoding='utf-8')
    return directory

@pytest.mark.parametrize('identity', ['agentroles.archi', 'ccb.archi'])
@pytest.mark.parametrize('adapter', [False, True])
def test_compact_role_expansion_matches_existing_parser(tmp_path, identity, adapter):
    role(tmp_path / 'roles/installed', name='design', adapter=adapter, current=True)
    path = tmp_path / 'ccb.config'
    body = f'cmd; ({identity}:pi, worker:codex@35)'
    path.write_text(body, encoding='utf-8')
    expected = _parse_compact_config_document(body, path=path, project_root=tmp_path)
    result = native('config', 'show', path)
    assert result.returncode == 0, result.stderr
    actual = json.loads(result.stdout)
    for key in ('agents','default_agents','layout','cmd_enabled'):
        assert actual[key] == expected[key]

def test_project_role_store_precedes_user_store_and_missing_roles_fail(tmp_path, monkeypatch):
    user = tmp_path / 'user-roles'
    role(user / 'installed', name='global_design')
    monkeypatch.setenv('AGENT_ROLES_STORE', str(user))
    role(tmp_path / '.ccb/roles/installed', name='local_design')
    path = tmp_path / '.ccb/ccb.config'
    path.write_text('agentroles.archi:codex', encoding='utf-8')
    result = native('config', 'show', path)
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout)['default_agents'] == ['local_design']
    path.write_text('agentroles.missing:codex', encoding='utf-8')
    assert native('config', 'show', path).returncode != 0

def test_hybrid_expanded_name_and_compile_preserve_canonical_role(tmp_path):
    role(tmp_path / 'roles/installed', identity='ccb.archi', name='designer')
    (tmp_path / '.ccb').mkdir()
    (tmp_path / '.ccb/ccb.config').write_text('agentroles.archi:pi\n[agents.designer]\nmodel="fixture-model"\n', encoding='utf-8')
    manifest = tmp_path / 'executables.json'
    manifest.write_text(json.dumps({'pi':['node','pi.js']}))
    result = native('config', 'compile', tmp_path, manifest)
    assert result.returncode == 0, result.stderr
    record = json.loads(result.stdout)['agents']['designer']
    assert record['role'] == 'agentroles.archi'
    assert record['model'] == 'fixture-model'

@pytest.mark.parametrize('bindings,target,expected', [
    ({'designer':'agentroles.archi'}, 'agentroles.archi', 'designer'),
    ({'designer':'agentroles.archi'}, 'ccb.archi', 'designer'),
    ({'helper':'agentroles.ccb_self'}, 'agentrole.ccb_self', 'helper'),
])
def test_role_submission_matches_existing_target_resolution(tmp_path, bindings, target, expected):
    agents = {name:dict(spec(), role=identity) for name,identity in bindings.items()}
    oracle = _resolve_target(target, {name:SimpleNamespace(role=identity) for name,identity in bindings.items()})
    assert oracle == expected
    server = Server(tmp_path / 'state', agents)
    try:
        response = cli('--state-root', server.root, 'ask', target, 'task')
        assert response.returncode == 0, response.stderr
        rows = server.rpc('records.latest', dict(stream='jobs', agent_name=expected, identity='job_id'), version=1)['result']
        assert len(rows) == 1 and rows[0]['request']['to_agent'] == expected
        listed = cli('--state-root', server.root, 'agents')
        assert listed.returncode == 0, listed.stderr
        assert json.loads(listed.stdout)['agents'][0]['role'] == bindings[expected]
    finally: server.close()

def test_ambiguous_missing_and_disabled_role_targets_do_not_admit_work(tmp_path):
    server = Server(tmp_path / 'state', {'a':dict(spec(),role='agentroles.archi'),
                                      'b':dict(spec(),role='agentroles.archi'),
                                      'c':dict(spec(),role='agentroles.hidden',dispatch_disabled=True)})
    try:
        for target, error in [('agentroles.archi','multiple agents'),('agentroles.unknown','not bound'),('agentroles.hidden','dispatch-disabled')]:
            result = cli('--state-root', server.root, 'ask', target, 'task')
            assert result.returncode != 0 and error in result.stderr
        assert server.rpc('records.list', {'stream':'messages'}, version=1)['result'] == []
    finally: server.close()

@pytest.mark.parametrize('kind', ['wrong-id', 'invalid-name', 'duplicate-name', 'path-role', 'bad-manifest'])
def test_invalid_role_identity_or_expansion_fails_without_startup(tmp_path, kind):
    directory = role(tmp_path / 'roles/installed', name='designer')
    body = 'agentroles.archi:pi'
    if kind == 'wrong-id':
        (directory / 'role.toml').write_text('id="agentroles.other"\n')
    elif kind == 'invalid-name':
        (directory / 'role.toml').write_text('id="agentroles.archi"\n[identity]\ndefault_name="../escape"\n')
    elif kind == 'duplicate-name': body += '; designer:codex'
    elif kind == 'path-role': body = '..:pi'
    elif kind == 'bad-manifest': (directory / 'role.toml').write_text('invalid = [')
    path = tmp_path / 'ccb.config'
    path.write_text(body)
    assert native('config', 'show', path).returncode != 0
    assert not (tmp_path / '.ccb/ccbd').exists()

def test_project_store_precedence_and_identity_name_precedes_adapter(tmp_path):
    role(tmp_path / 'roles/installed', name='first')
    role(tmp_path / '.ccb/roles/installed', name='second')
    directory = tmp_path / 'roles/installed/agentroles.archi'
    (directory / 'adapters/ccb').mkdir(parents=True)
    (directory / 'adapters/ccb/adapter.toml').write_text('default_agent_name="adapter_name"')
    path = tmp_path / '.ccb/ccb.config'
    path.write_text('agentroles.archi:pi')
    actual = native('config', 'show', path)
    assert actual.returncode == 0, actual.stderr
    assert json.loads(actual.stdout)['default_agents'] == ['first']

def test_configured_source_home_role_lookup_without_project_copy(tmp_path, monkeypatch):
    source = tmp_path / 'source'
    role(source / '.roles/installed', name='source_agent')
    monkeypatch.delenv('AGENT_ROLES_STORE', raising=False)
    monkeypatch.setenv('CCB_SOURCE_HOME', str(source))
    path = tmp_path / 'ccb.config'
    path.write_text('agentroles.archi:codex')
    output = native('config', 'show', path)
    assert output.returncode == 0, output.stderr
    assert json.loads(output.stdout)['default_agents'] == ['source_agent']
    assert not (tmp_path / 'roles').exists()

@pytest.mark.skipif(os.name != 'nt', reason='Windows account profile fallback')
def test_managed_provider_home_is_not_used_as_the_user_role_store(tmp_path, monkeypatch):
    account = tmp_path / 'account'
    managed = tmp_path / 'agents/caller/provider-state/pi/home'
    role(account / '.roles/installed', name='account_role')
    role(managed / '.roles/installed', name='private_role')
    monkeypatch.delenv('AGENT_ROLES_STORE', raising=False)
    monkeypatch.delenv('CCB_SOURCE_HOME', raising=False)
    monkeypatch.setenv('HOME', str(managed))
    monkeypatch.setenv('USERPROFILE', str(account))
    path = tmp_path / 'ccb.config'
    path.write_text('agentroles.archi:pi')
    output = native('config', 'show', path)
    assert output.returncode == 0, output.stderr
    assert json.loads(output.stdout)['default_agents'] == ['account_role']

def test_named_rich_role_binding_compiles_alias_without_requiring_implicit_name_lookup(tmp_path):
    (tmp_path / '.ccb').mkdir()
    (tmp_path / '.ccb/ccb.config').write_text('version=2\n[agents.worker]\nprovider="codex"\nrole="ccb.archi"\n')
    manifest = tmp_path / 'executables.json'
    manifest.write_text(json.dumps({'codex':['codex.exe']}))
    output = native('config','compile',tmp_path,manifest)
    assert output.returncode == 0, output.stderr
    assert json.loads(output.stdout)['agents']['worker']['role'] == 'agentroles.archi'

def test_native_project_startup_and_role_addressed_submission(tmp_path):
    project = tmp_path / 'role project'
    role(project / 'roles/installed', name='reviewer')
    (project / '.ccb').mkdir()
    config = project / '.ccb/ccb.config'
    config.write_text('cmd; agentroles.archi:claude',encoding='utf-8')
    (project / 'nested').mkdir()
    manifest = tmp_path / 'executables.json'
    manifest.write_text(json.dumps({'claude':[sys.executable,'-I',str(STUB),'worker']}))
    started = cli('start','--executables',manifest,cwd=project / 'nested')
    assert started.returncode == 0, started.stderr
    state = project / '.ccb/ccbd'
    try:
        receipt = cli('ask','ccb.archi','task',cwd=project / 'nested')
        assert receipt.returncode == 0, receipt.stderr
        target = json.loads(receipt.stdout)['job_id']
        response = cli('--state-root',state,'watch',target,'--wait','--timeout-ms','5000')
        assert response.returncode == 0, response.stderr
        row = json.loads(response.stdout)
        assert row['agent_name'] == 'reviewer' and row['reply'] == 'Claude final result'
        assert config.read_text() == 'cmd; agentroles.archi:claude'
    finally:
        stopped = cli('--state-root',state,'stop')
        assert stopped.returncode == 0, stopped.stderr
