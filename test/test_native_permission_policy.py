"""Permission policy tests use disposable protocol fixtures, never unrestricted real agents."""
import copy,json,os
from pathlib import Path
import pytest
from test_native_daemon import Server,submit,await_watch
from test_native_codex_rpc import codex_spec
from test_native_claude_stream import claude_spec
from test_native_reload import config,applied
from test_native_launch_options import compile_options
from test_native_role_memory import install
from test_native_herdr_namespace import setup
from test_native_herdr_agents import call

def records(path):return [json.loads(s) for s in path.read_text(encoding='utf-8').splitlines()] if path.exists() else []

@pytest.mark.parametrize('provider',['codex','claude'])
def test_explicit_auto_is_applied_at_launch_and_removed_on_manual_reload(tmp_path,provider):
    wire=tmp_path/'wire.jsonl';argv=tmp_path/'argv.jsonl'
    spec=(codex_spec if provider=='codex' else claude_spec)()
    spec.update(permission='auto',env={'CCB_FIXTURE_TRACE':str(wire),'CCB_FIXTURE_ARGV_TRACE':str(argv)})
    s=Server(tmp_path/'state',{'worker':spec})
    try:
        first=submit(s);assert await_watch(s,first['job_id'],lambda r:r['terminal'])['status']=='completed'
        if provider=='codex':
            bind=next(r for r in records(wire) if r.get('method')=='thread/start')['params']
            assert bind['approvalPolicy']=='never' and bind['sandbox']=='danger-full-access'
        else:
            args=records(argv)[-1];assert args[args.index('--permission-mode')+1]=='bypassPermissions'
        # Declarative state never stores generated policy arguments/options.
        stored=json.loads((s.root/'configuration.json').read_text(encoding='utf-8'))
        assert 'bypassPermissions' not in json.dumps(stored) and 'danger-full-access' not in json.dumps(stored)
        manual=copy.deepcopy(spec);manual['permission']='manual'
        if provider=='codex':manual['thread_options']={'approvalPolicy':'on-request','sandbox':'read-only'}
        applied(s,config({'worker':manual}))
        second=submit(s);assert await_watch(s,second['job_id'],lambda r:r['terminal'])['status']=='completed'
        if provider=='codex':
            bind=[r for r in records(wire) if r.get('method') in ['thread/start','thread/resume']][-1]['params']
            assert bind['approvalPolicy']=='on-request' and bind['sandbox']=='read-only'
        else:assert '--permission-mode' not in records(argv)[-1]
    finally:s.close()

@pytest.mark.parametrize('provider,options',[
    ('pi',{'permission':'auto'}),('claude',{'permission':True}),('codex',{'permission':'unknown'}),
    ('codex',{'permission':'auto','thread_options':{'sandbox':'read-only'}}),
    ('codex',{'permission':'auto','turn_options':{'approvalPolicy':'on-request'}}),
    ('codex',{'permission':'auto','startup_args':['-c','sandbox_mode="read-only"']}),
    ('codex',{'permission':'auto','startup_args':['--sandbox','read-only']}),
    ('codex',{'permission':'auto','startup_args':['-sread-only']}),
    ('claude',{'permission':'auto','startup_args':['--permission-mode','plan']}),
])
def test_invalid_or_conflicting_permission_is_rejected_during_compile(tmp_path,provider,options):
    result=compile_options(tmp_path,provider,**options);assert result.returncode!=0

@pytest.mark.parametrize('provider',['pi','codex','claude'])
@pytest.mark.parametrize('mode',['manual','auto'])
def test_required_role_surface_never_silently_becomes_unrestricted(tmp_path,provider,mode):
    root=install(tmp_path);surface=root/'adapters/ccb/command-surface.toml';surface.parent.mkdir(parents=True,exist_ok=True)
    surface.write_text('[command_surface]\nmode="deny_all_except"\nenforcement="required"\nif_unsupported="fail_mount"\ngeneric_shell=false\ngeneric_ccb=false\n',encoding='utf-8')
    result=compile_options(tmp_path,provider,permission=mode,role='agentroles.reader')
    assert result.returncode!=0
    assert 'fail_mount' in result.stderr or provider=='pi' and mode=='auto'
    assert not (tmp_path/'.ccb/ccbd').exists()

def test_live_required_role_blocks_new_work_and_reload_before_provider_input(tmp_path):
    root=install(tmp_path);wire=tmp_path/'wire.jsonl'
    spec=claude_spec();spec.update(role='agentroles.reader',workspace_path=str(tmp_path),permission='auto',env={'CCB_FIXTURE_TRACE':str(wire)})
    s=Server(tmp_path/'state',{'worker':spec})
    try:
        first=submit(s);assert await_watch(s,first['job_id'],lambda r:r['terminal'])['status']=='completed'
        before=[r for r in records(wire) if r.get('type')=='user']
        fingerprint=s.rpc('ping')['config_fingerprint']
        surface=root/'adapters/ccb/command-surface.toml';surface.parent.mkdir(parents=True,exist_ok=True)
        surface.write_text('[command_surface]\nmode="deny_all_except"\nenforcement="required"\nif_unsupported="fail_mount"\ngeneric_shell=false\ngeneric_ccb=false\n',encoding='utf-8')
        with pytest.raises(AssertionError,match='fail_mount'):s.rpc('reload-plan',{'config':config({'worker':spec})})
        assert s.rpc('ping')['config_fingerprint']==fingerprint
        second=submit(s);assert await_watch(s,second['job_id'],lambda r:r['terminal'])['status']=='failed'
        assert [r for r in records(wire) if r.get('type')=='user']==before
    finally:s.close()

@pytest.mark.parametrize('missing',[False,True])
def test_auto_requires_readable_role_policy_without_echoing_manifest_contents(tmp_path,missing):
    root=install(tmp_path);(root/'role.toml').write_text('SECRET_ROLE_VALUE = [',encoding='utf-8')
    result=compile_options(tmp_path,'claude',permission='auto',role='agentroles.uninstalled-permission-fixture' if missing else 'agentroles.reader')
    assert result.returncode!=0 and 'cannot establish role policy' in result.stderr
    assert 'SECRET_ROLE_VALUE' not in result.stderr

@pytest.mark.skipif(os.name!='nt',reason='Native Herdr plan is Windows-specific')
@pytest.mark.parametrize('args',[
    [],['--ask-for-approval','never','--sandbox','danger-full-access','--sandbox','read-only'],
    ['--ask-for-approval','never','--sandbox','danger-full-access','-c','sandbox_mode="read-only"'],
])
def test_low_level_herdr_mount_cannot_claim_auto_with_different_flags(setup,args):
    project,backend,request=setup;request['action']='plan';request['config']['agents']['worker']['permission']='auto'
    request['interactive_args']={'worker':args}
    result=call(request);assert result.returncode!=0
    assert not (project/'.ccb/ccbd/herdr/herdr-agents.json').exists()

@pytest.mark.skipif(os.name!='nt',reason='Native Herdr plan is Windows-specific')
def test_low_level_herdr_accepts_matching_auto_declaration_without_mutation(setup):
    project,backend,request=setup;request['action']='plan';request['config']['agents']['worker']['permission']='auto'
    request['interactive_args']={'worker':['--ask-for-approval','never','--sandbox','danger-full-access']}
    result=call(request);assert result.returncode==0,result.stderr
    assert json.loads(result.stdout)['mutations'] is False
