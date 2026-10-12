"""Existing single-window project documents feed the native launch/topology path."""
import json
from pathlib import Path
import pytest
from test_native_config import native
from agents.config_loader_runtime.parsing import validate_project_config
from test_native_herdr_namespace import call as namespace_call

def compile_doc(root,document):
    (root/'.ccb').mkdir(exist_ok=True);(root/'.ccb/ccb.config').write_text(json.dumps(document),encoding='utf-8')
    manifest=root/'executables.json';manifest.write_text(json.dumps({p:[p+'.exe'] for p in ['pi','codex','claude']}),encoding='utf-8')
    return native('config','compile',root,manifest)

@pytest.mark.parametrize('layout',['pi_lead:pi, codex_worker:codex','(lead:pi; worker:codex), reviewer:claude@30'])
def test_windows_document_matches_existing_agent_defaults_and_topology(tmp_path,layout):
    document={'version':2,'windows':{'main':layout},'runtime':{'mux':{'backend':'herdr'}},
              'agents':{'lead':{'model':'example','thinking':'low'},'stale':{'provider':'unsupported-old-provider'}}}
    expected=validate_project_config(document,project_root=tmp_path)
    result=compile_doc(tmp_path,document);assert result.returncode==0,result.stderr
    config=json.loads(result.stdout);assert config['source_config']==document
    assert set(config['agents'])==set(expected.agents)
    for name,spec in expected.agents.items():
        actual=config['agents'][name]
        assert actual['provider']==spec.provider and actual['permission']==spec.permission_default.value
        assert actual['workspace_mode']==spec.workspace_mode.value
    options=tmp_path/'open.json';options.write_text(json.dumps({'config':config}),encoding='utf-8')
    result=native('--project',tmp_path,'herdr','open','--options',options,'--dry-run')
    assert result.returncode==0,result.stderr
    assert json.loads(result.stdout)['status']=='planned' and not (tmp_path/'.ccb/ccbd').exists()
    planned=namespace_call(dict(schema_version=1,action='plan',session='native-config-test',command=['unused-herdr'],config=config,timeout_ms=1000))
    assert planned.returncode==0,planned.stderr
    topology=json.loads(planned.stdout)['plan']
    assert topology['visible_slots']==list(expected.windows[0].agent_names)
    assert not (tmp_path/'.ccb/ccbd').exists()

@pytest.mark.parametrize('change',[
    {'windows':{}},{'windows':{'one':'a:pi','two':'b:codex'}},{'windows':{'main':42}},
    {'entry_window':'missing'},{'layout':'a:pi'},{'cmd_enabled':False},{'default_agents':['a']},
    {'windows':{'main':'cmd; a:pi'}},{'agents':{'a':{'provider':'codex'}}},
    {'tool_windows':{'shell':'pwsh'}},{'ui':{'sidebar':{'mode':'every_window'}}},
    {'version':3},
])
def test_unsupported_or_conflicting_topology_is_not_silently_flattened(tmp_path,change):
    result=compile_doc(tmp_path,{'version':2,'windows':{'main':'a:pi'},**change})
    assert result.returncode!=0 and not (tmp_path/'.ccb/ccbd').exists()
