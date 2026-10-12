"""Ordinary Herdr open resolves its production defaults without starting anything."""
import json
import os
from pathlib import Path
import pytest
from test_native_cli_security import cli

pytestmark=pytest.mark.skipif(os.name!='nt',reason='Native Windows Herdr defaults')

@pytest.fixture(params=['compact','windows'])
def layout(tmp_path,request):
    project=tmp_path/'project';(project/'.ccb').mkdir(parents=True)
    config='lead:pi; worker:codex' if request.param=='compact' else 'version=2\n[windows]\nmain="lead:pi; worker:codex"\n'
    (project/'.ccb/ccb.config').write_text(config,encoding='utf-8')
    node=tmp_path/'node';node.mkdir();(node/'node.exe').write_bytes(b'not executable')
    modules=node/'bin/node_modules'
    pi=modules/'@earendil-works/pi-coding-agent/dist/bundle/cli.js';pi.parent.mkdir(parents=True);pi.write_text('not executable')
    codex=modules/'@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe'
    codex.parent.mkdir(parents=True);codex.write_bytes(b'not executable')
    home=tmp_path/'home';current=home/'.herdr/packages/standalone/current/herdr.exe';current.parent.mkdir(parents=True);current.write_bytes(b'not executable')
    stale=node/'herdr.exe';stale.write_bytes(b'not executable')
    env={**os.environ,'PATH':str(node),'USERPROFILE':str(home),'APPDATA':str(home/'appdata')}
    for name in ['CCB_HERDR_EXE','CCB_RUNTIME_STATE_HOME','NODE_PATH']:env.pop(name,None)
    return project,node,codex,current,env

def preview(layout,*args,env=None):
    project,_,_,_,default_env=layout
    result=cli('--project',project,'herdr','open',*args,'--dry-run',env=env or default_env)
    assert not (project/'.ccb/ccbd').exists()
    return result

def test_plain_open_resolves_native_programs_and_authenticated_codex_host(layout):
    project,node,codex,current,_=layout
    result=preview(layout);assert result.returncode==0,result.stderr
    plan=json.loads(result.stdout);agents={a['name']:a for a in plan['agents']}
    assert Path(plan['herdr_executable']).samefile(current)
    assert Path(agents['lead']['program']).samefile(node/'node.exe')
    assert Path(agents['worker']['program']).samefile(codex)
    assert agents['worker']['input_transport']=='authenticated_websocket'
    assert agents['worker']['tui_host_root'].endswith('worker')
    assert plan['mutations'] is False
    compiled=cli('config','compile',project,env=layout[-1])
    assert compiled.returncode==0,compiled.stderr
    assert Path(json.loads(compiled.stdout)['agents']['worker']['command'][0]).samefile(codex)

def test_explicit_herdr_program_wins_over_activated_standalone(layout):
    _,node,_,_,env=layout
    override=node/'override.exe';override.write_bytes(b'not executable')
    result=preview(layout,env={**env,'CCB_HERDR_EXE':str(override)})
    assert result.returncode==0,result.stderr
    assert Path(json.loads(result.stdout)['herdr_executable']).samefile(override)

def test_missing_explicit_herdr_program_does_not_fall_back(layout):
    _,node,_,_,env=layout
    result=preview(layout,env={**env,'CCB_HERDR_EXE':str(node/'missing.exe')})
    assert result.returncode!=0 and 'CCB_HERDR_EXE' in result.stderr

def test_missing_provider_fails_during_read_only_preview(layout):
    _,_,codex,_,_=layout;codex.unlink()
    result=preview(layout)
    assert result.returncode!=0 and 'codex' in result.stderr

def test_default_codex_host_does_not_drop_custom_terminal_arguments(layout):
    project,_,_,_,_=layout
    (project/'.ccb/ccb.config').write_text('version=2\n[agents.worker]\nprovider="codex"\nstartup_args=["--some-custom-flag"]\n')
    result=preview(layout)
    assert result.returncode!=0 and 'startup_args' in result.stderr
