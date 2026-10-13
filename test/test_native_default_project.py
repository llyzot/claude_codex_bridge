"""No-config startup uses builtin agents without writing a project config."""
import json
import os
from pathlib import Path
import subprocess
import sys
import pytest
from test_native_cli_security import cli
from test_native_workspaces import git, stop
from test_native_pi_rpc import STUB as PI
from test_native_codex_rpc import STUB as CODEX
from test_native_claude_stream import STUB as CLAUDE


def manifest(tmp_path):
    path=tmp_path/'executables.json'
    path.write_text(json.dumps({'pi':[sys.executable,'-I',str(PI),'worker'],
        'claude':[sys.executable,'-I',str(CLAUDE),'normal'],
        'codex':[sys.executable,'-I',str(CODEX),'normal']}))
    return path


def compile_default(root,tmp_path):
    result=cli('config','compile',root,manifest(tmp_path))
    assert result.returncode==0,result.stderr
    return json.loads(result.stdout)


def test_builtin_configuration_has_three_agents_and_does_not_create_files(tmp_path):
    root=tmp_path/'new project';root.mkdir()
    cfg=compile_default(root,tmp_path)
    assert {name:spec['provider'] for name,spec in cfg['agents'].items()}=={'pi':'pi','claude':'claude','codex':'codex'}
    assert cfg['source_config']['windows']['main']=='pi:pi; claude:claude; codex:codex'
    assert not (root/'.ccb').exists()
    assert all('model' not in spec or spec['model'] is None for spec in cfg['agents'].values())


def test_explicit_configuration_overrides_builtin_without_requiring_other_agents(tmp_path):
    root=tmp_path/'configured';(root/'.ccb').mkdir(parents=True)
    (root/'.ccb/ccb.config').write_text('only:claude')
    path=tmp_path/'manifest.json';path.write_text(json.dumps({'claude':['fixture-claude']}))
    result=cli('config','compile',root,path)
    assert result.returncode==0,result.stderr
    assert list(json.loads(result.stdout)['agents'])==['only']


def test_invalid_existing_config_never_falls_back_to_defaults(tmp_path):
    root=tmp_path/'invalid';(root/'.ccb').mkdir(parents=True)
    (root/'.ccb/ccb.config').write_text('version=2\n[agents.\n')
    assert cli('config','compile',root,manifest(tmp_path)).returncode!=0
    assert not (root/'.ccb/ccbd').exists()


@pytest.mark.skipif(os.name!='nt',reason='Windows discovery preview')
@pytest.mark.parametrize('git_project',[False,True])
def test_herdr_open_without_config_previews_defaults_without_launching(tmp_path,git_project):
    root=tmp_path/'project';root.mkdir()
    nested=root/'nested';nested.mkdir()
    if git_project:git(root,'init','-q')
    tools=tmp_path/'tools';tools.mkdir();home=tmp_path/'home';(home/'.local/bin').mkdir(parents=True)
    for name in ('node.exe','herdr.exe'):(tools/name).write_bytes(b'not executable')
    (home/'.local/bin/claude.exe').write_bytes(b'not executable')
    pi=tools/'bin/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js';pi.parent.mkdir(parents=True);pi.write_text('not executed')
    codex=tools/'bin/node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe';codex.parent.mkdir(parents=True);codex.write_bytes(b'not executable')
    env={**os.environ,'PATH':str(tools),'USERPROFILE':str(home),'HOME':str(home)}
    for key in ('CCB_NATIVE_TOKEN_FILE','CCB_NATIVE_STATE_ROOT','CCB_RUNTIME_STATE_HOME'):env.pop(key,None)
    result=cli('herdr','open','--dry-run','--herdr-exe',tools/'herdr.exe',cwd=nested if git_project else root,env=env)
    assert result.returncode==0,result.stderr
    assert 'pi' in result.stdout and 'claude' in result.stdout and 'codex' in result.stdout
    assert not (root/'.ccb').exists() and not (nested/'.ccb').exists()


def test_default_start_reconnect_and_queue_work_from_nested_git_directory(tmp_path):
    root=tmp_path/'repo';root.mkdir();git(root,'init','-q')
    nested=root/'nested';nested.mkdir();path=manifest(tmp_path)
    started=cli('start','--executables',path,cwd=nested,timeout=30)
    assert started.returncode==0,started.stderr
    state=Path(json.loads(started.stdout)['state_root'])
    try:
        again=cli('start','--executables',path,cwd=nested,timeout=30)
        assert again.returncode==0,again.stderr
        assert json.loads(again.stdout)['pid']==json.loads(started.stdout)['pid']
        assert cli('queue',cwd=nested).returncode==0
        asked=cli('ask','pi','fixture task',cwd=nested)
        assert asked.returncode==0,asked.stderr
        assert not (root/'.ccb/ccb.config').exists() and not (nested/'.ccb').exists()
    finally:stop(state)


def test_missing_previous_explicit_config_does_not_silently_change_agent_set(tmp_path):
    root=tmp_path/'configured';(root/'.ccb').mkdir(parents=True)
    (root/'.ccb/ccb.config').write_text('worker:claude')
    started=cli('start','--executables',manifest(tmp_path),cwd=root,timeout=30)
    assert started.returncode==0,started.stderr
    state=Path(json.loads(started.stdout)['state_root']);stop(state)
    (root/'.ccb/ccb.config').unlink()
    rejected=cli('start','--executables',manifest(tmp_path),cwd=root,timeout=30)
    assert rejected.returncode!=0 and 'configuration' in rejected.stderr.lower()
    assert not (root/'.ccb/ccb.config').exists()


def test_nested_git_repository_does_not_inherit_another_projects_config(tmp_path):
    outer=tmp_path/'outer';(outer/'.ccb').mkdir(parents=True)
    (outer/'.ccb/ccb.config').write_text('outer_only:claude')
    inner=outer/'inner';inner.mkdir();git(inner,'init','-q')
    result=cli('--project',inner,'start','--executables',manifest(tmp_path),timeout=30)
    assert result.returncode==0,result.stderr
    state=Path(json.loads(result.stdout)['state_root'])
    try:
        assert state.parent.parent.samefile(inner)
        agents=cli('--state-root',state,'agents');assert agents.returncode==0,agents.stderr
        assert 'outer_only' not in agents.stdout
    finally:stop(state)
