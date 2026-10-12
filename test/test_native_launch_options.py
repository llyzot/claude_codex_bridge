"""Launch shortcuts must survive compilation and agree with native arguments."""
import json,os
from pathlib import Path
import pytest
from test_native_config import native
from provider_thinking_shortcuts import provider_thinking_levels

def compile_options(root,provider,**options):
    (root/'.ccb').mkdir(exist_ok=True)
    document={'version':2,'agents':{'worker':{'provider':provider,'target':'.','workspace_mode':'inplace','restore':'auto','permission':'manual',**options}}}
    (root/'.ccb/ccb.config').write_text(json.dumps(document),encoding='utf-8')
    manifest=root/'executables.json';manifest.write_text(json.dumps({provider:[provider+'.exe']}),encoding='utf-8')
    return native('config','compile',root,manifest)

@pytest.mark.parametrize('provider',['pi','codex','claude'])
def test_model_and_thinking_are_effective_without_dropping_other_options(tmp_path,provider):
    extra={'turn_options':{'summary':'concise'},'thread_options':{'approvalPolicy':'on-request'}} if provider=='codex' else {}
    result=compile_options(tmp_path,provider,model=' example-model ',thinking=' HIGH ',**extra)
    assert result.returncode==0,result.stderr
    spec=json.loads(result.stdout)['agents']['worker'];assert spec['model']=='example-model' and spec['thinking']=='high'
    if provider=='codex':
        assert spec['thread_options']=={'model':'example-model','approvalPolicy':'on-request'}
        assert spec['turn_options']=={'effort':'high','summary':'concise'}
    else:
        flag='--thinking' if provider=='pi' else '--effort'
        assert spec['command'][spec['command'].index(flag)+1]=='high'
        assert spec['command'][spec['command'].index('--model')+1]=='example-model'

@pytest.mark.parametrize('provider,options',[
    ('pi',{'thinking':'ultra'}),('claude',{'thinking':'off'}),('codex',{'thinking':'not-a-level'}),
    ('claude',{'thinking':True}),('pi',{'model':' '}),('codex',{'model':42}),
    ('claude',{'startup_args':'--effort high'}),('pi',{'startup_args':[False]}),
    ('codex',{'thread_options':[]}),('codex',{'turn_options':'discarded'}),
    ('pi',{'env':{'EXAMPLE':42}}),('claude',{'startup_args':['nul\x00value']}),
    ('codex',{'model':'one','thread_options':{'model':'two'}}),
    ('codex',{'thinking':'low','turn_options':{'effort':'high'}}),
    ('pi',{'model':'one','startup_args':['--model=two']}),
    ('claude',{'thinking':'low','startup_args':['--effort=high']}),
    ('codex',{'thinking':'low','startup_args':['--config=model_reasoning_effort="high"']}),
])
def test_invalid_or_conflicting_launch_settings_fail_at_compilation(tmp_path,provider,options):
    result=compile_options(tmp_path,provider,**options)
    assert result.returncode!=0,'Invalid settings were silently accepted'

@pytest.mark.parametrize('provider,flags',[
    ('pi',['--model','example','--thinking','high']),
    ('claude',['--model','example','--effort','high']),
    ('codex',['-m','example','-c','model_reasoning_effort="high"']),
])
def test_precompiled_legacy_prefix_is_removed_once_and_extra_args_retained(tmp_path,provider,flags):
    result=compile_options(tmp_path,provider,model='example',thinking='high',startup_args=flags+['--example-extra'])
    assert result.returncode==0,result.stderr
    spec=json.loads(result.stdout)['agents']['worker']
    assert spec['startup_args']==['--example-extra']
    assert spec['command'].count('--example-extra')==1
    if provider!='codex':assert spec['command'].count('--model')==1

@pytest.mark.parametrize('provider',['pi','codex','claude'])
def test_thinking_levels_match_existing_provider_contract(tmp_path,provider):
    for level in provider_thinking_levels(provider):
        result=compile_options(tmp_path,provider,thinking=level)
        assert result.returncode==0,(level,result.stderr)

@pytest.mark.skipif(os.name!='nt',reason='Native Herdr composition is Windows-specific')
@pytest.mark.parametrize('provider,options',[
    ('pi',{'thinking':'ultra'}),('claude',{'thinking':True}),
    ('codex',{'model':'one','startup_args':['-mtwo']}),
    ('claude',{'thinking':'low','startup_args':['--effort=high']}),
])
def test_herdr_preflight_uses_same_validation_before_any_runtime_mutation(tmp_path,provider,options):
    result=compile_options(tmp_path,provider);assert result.returncode==0,result.stderr
    config=json.loads(result.stdout);config['agents']['worker'].update(options)
    file=tmp_path/'open-options.json';file.write_text(json.dumps({'config':config}),encoding='utf-8')
    rejected=native('--project',tmp_path,'herdr','open','--options',file,'--dry-run')
    assert rejected.returncode!=0
    assert not (tmp_path/'.ccb/ccbd').exists()
