"""Native compact/TOML/hybrid config checked against the existing reader."""
import json
from pathlib import Path
import subprocess
import pytest
from test_native_cli_security import BIN
from agents.config_loader_runtime.io_runtime.documents import _parse_compact_config_document,_parse_hybrid_config_document,_parse_toml_config_document
from project.ids import compute_legacy_project_id

def native(*args):
    result=subprocess.run([str(BIN),*map(str,args)],capture_output=True,text=True,encoding='utf-8',timeout=5)
    return result

@pytest.mark.parametrize('text',[
    'cmd; lead:pi, worker:codex',
    '(cmd; lead:pi), worker:codex@30',
    'lead:pi(worktree); worker:codex',
])
def test_compact_reader_keeps_agent_semantics(tmp_path,text):
    path=tmp_path/'ccb.config';path.write_text(text,encoding='utf-8')
    oracle=_parse_compact_config_document(text,path=path,project_root=tmp_path)
    result=native('config','show',path);assert result.returncode==0,result.stderr
    actual=json.loads(result.stdout)
    for key in ['agents','default_agents','cmd_enabled','version']:assert actual[key]==oracle[key]

def test_hybrid_and_rich_options_match_existing_reader(tmp_path):
    header='cmd; lead:pi; worker:codex'
    overlay='[agents.lead]\nmodel="gpt-6-astra"\nthinking="low"\n[agents.worker.env]\nEXAMPLE="value"\n'
    path=tmp_path/'ccb.config';path.write_text(header+'\n'+overlay)
    expected=_parse_hybrid_config_document(header,overlay,path=path,project_root=tmp_path)
    result=native('config','show',path);assert result.returncode==0,result.stderr
    actual=json.loads(result.stdout)
    assert actual['agents']==expected['agents']
    rich='version=2\ndefault_agents=["lead"]\n[agents.lead]\nprovider="pi"\ntarget="."\nworkspace_mode="inplace"\nrestore="auto"\npermission="manual"\n'
    path.write_text(rich)
    actual=json.loads(native('config','show',path).stdout)
    assert actual==_parse_toml_config_document(rich,path=path)

@pytest.mark.parametrize('text',['lead:pi;lead:codex','cmd;cmd;lead:pi','lead:pi@0','cmd;(lead:pi,worker:codex)@40','lead:pi\n[agents.lead]\nprovider="codex"','lead:pi\n[unknown]\nflag=true'])
def test_invalid_or_owned_overlay_fields_fail_closed(tmp_path,text):
    path=tmp_path/'ccb.config';path.write_text(text)
    assert native('config','show',path).returncode!=0

def test_project_compilation_preserves_identity_and_provider_argv(tmp_path):
    (tmp_path/'.ccb').mkdir();(tmp_path/'.ccb/ccb.config').write_text('cmd; lead:pi; worker:codex; reviewer:claude')
    manifest=tmp_path/'executables.json';manifest.write_text(json.dumps({'pi':['node','pi.js'],'codex':['codex.exe'],'claude':['claude.exe']}))
    result=native('config','compile',tmp_path,manifest);assert result.returncode==0,result.stderr
    actual=json.loads(result.stdout)
    assert actual['project_id']==compute_legacy_project_id(tmp_path)
    assert actual['agents']['lead']['driver_kind']=='pi_rpc'
    assert actual['agents']['worker']['command']==['codex.exe','app-server','--stdio']
    assert '--replay-user-messages' in actual['agents']['reviewer']['command']
    identity={'schema_version':1,'record_type':'ccb_project_identity','project_id':'a'*64}
    (tmp_path/'.ccb/project.identity.json').write_text(json.dumps(identity))
    assert json.loads(native('config','compile',tmp_path,manifest).stdout)['project_id']=='a'*64
