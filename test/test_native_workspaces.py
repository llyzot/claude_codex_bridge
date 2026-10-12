"""Native worktree startup uses Git in disposable fixture repositories only."""
import json,os,subprocess,sys,time
import shlex
from pathlib import Path
from types import SimpleNamespace
import pytest
from test_native_cli_security import cli
from test_native_claude_stream import STUB
from workspace.planner import WorkspacePlanner
from agents.models import WorkspaceMode
from project.ids import compute_project_id
from workspace.binding import WorkspaceBindingStore

def git(root,*args,ok=True):
    result=subprocess.run(['git','-C',str(root),*map(str,args)],capture_output=True,text=True,encoding='utf-8',timeout=15)
    if ok:assert result.returncode==0,result.stderr
    return result
def repo(base,name='project'):
    root=base/name;root.mkdir()
    git(root,'init','-q','--initial-branch=main')
    (root/'source.txt').write_text('initial\n')
    (root/'.gitignore').write_text('.ccb/\n')
    git(root,'add','source.txt','.gitignore')
    commit(root)
    (root/'.ccb').mkdir()
    (root/'.ccb/ccb.config').write_text('worker:claude(worktree)')
    return root
def commit(root):git(root,'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','-c','commit.gpgSign=false','commit','-qm','fixture')
def manifest(root):
    path=root.parent/(root.name+'-executables.json')
    path.write_text(json.dumps({'claude':[sys.executable,'-I',str(STUB),'workspace']}))
    return path
def compiled(root):
    result=cli('config','compile',root,manifest(root))
    assert result.returncode==0,result.stderr
    return json.loads(result.stdout)
def start(root,**kwargs):
    result=cli('start','--executables',manifest(root),cwd=root,**kwargs)
    assert result.returncode==0,result.stderr
    return json.loads(result.stdout)
def stop(state):
    before=cli('--state-root',state,'ping');assert before.returncode==0,before.stderr
    pid=json.loads(before.stdout)['pid']
    result=cli('--state-root',state,'stop');assert result.returncode==0,result.stderr
    # stop acknowledges the request before unmount drains the provider processes.
    deadline=time.monotonic()+10
    while time.monotonic()<deadline:
        lifecycle=state/'lifecycle.json'
        # Read the final journal only after its writer exits; Windows can deny
        # opens while the live writer atomically replaces this file.
        if cli('process-identity',pid).returncode!=0 and lifecycle.exists() and record(lifecycle).get('phase')=='unmounted':
            return
        time.sleep(.025)
    pytest.fail('owned workspace daemon did not finish unmounting')
def rich(root,extra='',agents=('worker',)):
    text='version=2\n'+''.join(f'[agents.{name}]\nprovider="claude"\ntarget="."\nworkspace_mode="git-worktree"\n'+extra+'\n' for name in agents)
    (root/'.ccb/ccb.config').write_text(text,encoding='utf-8')
def record(path):return json.loads(path.read_text(encoding='utf-8'))
def normalized(path):return str(Path(path).resolve()).removeprefix('\\\\?\\').casefold() if os.name=='nt' else str(Path(path).resolve())

@pytest.mark.parametrize('kind',['default','root','group','external'])
def test_read_only_plans_match_existing_workspace_planner(tmp_path,kind):
    root=repo(tmp_path,'source repo')
    options={}
    if kind=='root':options['workspace_root']=str(tmp_path/'external workspaces')
    if kind=='group':options['workspace_group']='team'
    if kind=='external':
        external=tmp_path/'existing';git(root,'worktree','add','-qb','external',external)
        options['workspace_path']=str(external)
    extra=''.join(f'{k}={json.dumps(v)}\n' for k,v in options.items())
    rich(root,extra)
    before=git(root,'worktree','list','--porcelain').stdout
    data=compiled(root)['agents']['worker']
    spec=SimpleNamespace(name='worker',workspace_mode=WorkspaceMode.GIT_WORKTREE,workspace_root=None,workspace_group=None,workspace_path=None,branch_template=None)
    for key,value in options.items():setattr(spec,key,value)
    expected=WorkspacePlanner().plan(spec,SimpleNamespace(project_root=root.resolve(),project_id=compute_project_id(root)))
    assert normalized(data['workspace_path'])==normalized(expected.workspace_path)
    assert data['workspace_plan']['branch']==expected.branch_name
    assert git(root,'worktree','list','--porcelain').stdout==before
    if kind!='external':assert not Path(data['workspace_path']).exists()
    assert not (root/'.ccb/ccbd').exists()

def test_native_start_creates_and_binds_worktree_and_runs_provider_there(tmp_path):
    root=repo(tmp_path,'project with spaces')
    created=start(root);state=Path(created['state_root']);work=root/'.ccb/workspaces/worker'
    try:
        assert created['status']=='running' and (work/'.git').is_file()
        binding=record(work/'.ccb-workspace.json')
        legacy=WorkspaceBindingStore().load(work/'.ccb-workspace.json')
        assert Path(legacy.target_project).resolve()==root.resolve()
        assert binding['record_type']=='workspace_binding' and binding['project_id']==compute_project_id(root)
        assert binding['branch_name']=='ccb/worker' and Path(binding['target_project']).samefile(root)
        nested=work/'nested';nested.mkdir()
        result=cli('ask','worker','where are you',cwd=nested);assert result.returncode==0,result.stderr
        watched=cli('--state-root',state,'watch',json.loads(result.stdout)['job_id'],'--wait','--timeout-ms','5000')
        assert watched.returncode==0,watched.stderr
        assert Path(json.loads(json.loads(watched.stdout)['reply'])['cwd']).samefile(work)
        assert start(root)['pid']==created['pid']
    finally:stop(state)
    assert work.exists() and git(root,'branch','--show-current').stdout.strip()=='main'

def test_reuse_preserves_dirty_worktree_and_does_not_follow_new_source_head(tmp_path):
    root=repo(tmp_path);first=start(root);state=Path(first['state_root']);stop(state)
    work=root/'.ccb/workspaces/worker';head=git(work,'rev-parse','HEAD').stdout
    (work/'source.txt').write_text('worker change');git(work,'add','source.txt')
    (work/'untracked.txt').write_text('preserve')
    (root/'source.txt').write_text('new source');git(root,'add','source.txt');commit(root)
    second=start(root)
    try:
        assert (work/'source.txt').read_text()=='worker change' and (work/'untracked.txt').read_text()=='preserve'
        assert git(work,'rev-parse','HEAD').stdout==head
        assert git(work,'diff','--cached','--name-only').stdout.strip()=='source.txt'
    finally:stop(Path(second['state_root']))

def test_group_agents_share_one_registered_checkout(tmp_path):
    root=repo(tmp_path);rich(root,'workspace_group="shared"',('one','two'))
    result=start(root)
    try:
        data=compiled(root)['agents']
        assert Path(data['one']['workspace_path']).samefile(data['two']['workspace_path'])
        assert git(root,'worktree','list','--porcelain').stdout.count('worktree ')==2
        assert record(Path(data['one']['workspace_path'])/'.ccb-workspace.json')['branch_name']=='ccb/group/shared'
    finally:stop(Path(result['state_root']))

def test_external_checkout_is_used_without_binding_mutation(tmp_path):
    root=repo(tmp_path);external=tmp_path/'external'
    git(root,'worktree','add','-qb','external',external)
    (external/'user.txt').write_text('unchanged')
    rich(root,'workspace_path='+json.dumps(str(external)))
    result=start(root)
    try:
        assert not (external/'.ccb-workspace.json').exists()
        assert (external/'user.txt').read_text()=='unchanged'
        assert git(external,'branch','--show-current').stdout.strip()=='external'
    finally:stop(Path(result['state_root']))

@pytest.mark.parametrize('kind',['unborn','occupied-branch','nonempty-path','foreign-repo','wrong-branch','tracked-binding','unbound'])
def test_conflicting_or_unconfirmed_workspaces_are_not_replaced(tmp_path,kind):
    root=repo(tmp_path);work=root/'.ccb/workspaces/worker'
    if kind=='unborn':git(root,'checkout','--orphan','empty');git(root,'rm','-rf','--cached','.')
    elif kind=='occupied-branch':git(root,'worktree','add','-qb','ccb/worker',tmp_path/'occupied')
    elif kind=='nonempty-path':work.mkdir(parents=True);(work/'user.txt').write_text('preserve')
    elif kind=='foreign-repo':
        other=repo(tmp_path,'foreign');rich(root,'workspace_path='+json.dumps(str(other)))
    elif kind=='tracked-binding':
        (root/'.ccb-workspace.json').write_text('user content');git(root,'add','.ccb-workspace.json');commit(root)
    elif kind=='unbound':git(root,'worktree','add','-qb','ccb/worker',work)
    elif kind=='wrong-branch':
        started=start(root);stop(Path(started['state_root']));git(work,'checkout','-qb','different')
    before=git(root,'worktree','list','--porcelain').stdout
    result=cli('config','compile',root,manifest(root))
    assert result.returncode!=0
    assert git(root,'worktree','list','--porcelain').stdout==before
    if kind=='nonempty-path':assert (work/'user.txt').read_text()=='preserve'
    if kind=='tracked-binding':assert (root/'.ccb-workspace.json').read_text()=='user content'
    if kind=='wrong-branch':assert git(work,'branch','--show-current').stdout.strip()=='different'

@pytest.mark.parametrize('text',['workspace_path="x"\nworkspace_group="g"','workspace_root="x"\nworkspace_group="g"',
    'workspace_group="g"\nbranch_template="ccb/custom"','branch_template="ccb/{unknown}"','branch_template="--bad"'])
def test_invalid_workspace_configuration_has_no_filesystem_effects(tmp_path,text):
    root=repo(tmp_path);rich(root,text)
    result=cli('config','compile',root,manifest(root));assert result.returncode!=0
    assert not (root/'.ccb/workspaces').exists() and not (root/'.ccb/ccbd').exists()

@pytest.mark.parametrize('dirty',[False,True])
def test_prepared_creation_recovery_requires_clean_matching_checkout(tmp_path,dirty):
    root=repo(tmp_path);started=start(root);state=Path(started['state_root']);stop(state)
    work=root/'.ccb/workspaces/worker';marker=work/'.ccb-workspace.json'
    intent=work.with_name('worker.ccb-native-worktree.json');data=record(intent);data['phase']='prepared';intent.write_text(json.dumps(data))
    marker.unlink()
    if dirty:(work/'source.txt').write_text('ambiguous change')
    previous_start=(state/'kernel/native-start.json').read_bytes()
    # Keeper observes bounded startup retries; the harness must outlive the
    # command's 15-second observation window to inspect its actual result.
    result=cli('start','--executables',manifest(root),cwd=root,timeout=25)
    if dirty:
        assert result.returncode!=0
        assert 'unconfirmed or changed worktree creation' in result.stderr
        assert (state/'kernel/native-start.json').read_bytes()==previous_start
        assert (work/'source.txt').read_text()=='ambiguous change' and not marker.exists()
    else:
        assert result.returncode==0,result.stderr
        try:assert marker.exists() and record(intent)['phase']=='bound'
        finally:stop(state)

def test_explicit_base_and_branch_template_are_applied(tmp_path):
    root=repo(tmp_path);base=git(root,'rev-parse','HEAD').stdout.strip()
    (root/'source.txt').write_text('later');git(root,'add','source.txt');commit(root)
    rich(root,f'branch_template="ccb/{{agent_name}}/{{project_slug}}/{{date}}"\nbase_commit="{base}"')
    plan=compiled(root)['agents']['worker']['workspace_plan']
    result=start(root)
    try:
        work=Path(plan['workspace_path'])
        assert git(work,'rev-parse','HEAD').stdout.strip()==base
        assert git(work,'branch','--show-current').stdout.strip()==plan['branch']
        assert (work/'source.txt').read_text()=='initial\n'
    finally:stop(Path(result['state_root']))


def test_missing_registered_worktree_is_not_pruned_or_recreated(tmp_path):
    root=repo(tmp_path);result=start(root);state=Path(result['state_root']);stop(state)
    work=root/'.ccb/workspaces/worker';parked=tmp_path/'parked-worktree'
    assert work.resolve().is_relative_to(tmp_path.resolve())
    assert parked.parent.resolve().is_relative_to(tmp_path.resolve())
    work.rename(parked)
    before=git(root,'worktree','list','--porcelain').stdout
    previous_start=(state/'kernel/native-start.json').read_bytes()
    result=cli('start','--executables',manifest(root),cwd=root,timeout=25)
    assert result.returncode!=0
    assert 'registered workspace is missing' in result.stderr
    assert (state/'kernel/native-start.json').read_bytes()==previous_start
    assert not work.exists() and (parked/'source.txt').read_text()=='initial\n'
    assert git(root,'worktree','list','--porcelain').stdout==before


def test_materialization_does_not_execute_checkout_hook(tmp_path):
    root=repo(tmp_path);marker=tmp_path/'hook-ran'
    hook=root/'.git/hooks/post-checkout'
    hook.write_text('#!/bin/sh\nprintf ran > '+shlex.quote(marker.as_posix())+'\n',encoding='utf-8')
    hook.chmod(0o755)
    git(root,'worktree','add','-qb','hook-control',tmp_path/'hook-control')
    assert marker.read_text()=='ran'
    marker.unlink()
    started=start(root)
    try:assert not marker.exists()
    finally:stop(Path(started['state_root']))


def test_startup_observation_expiry_reuses_one_creator_and_checkout(tmp_path):
    root=repo(tmp_path)
    first=cli('start','--executables',manifest(root),'--timeout-ms','0',cwd=root)
    assert first.returncode==0,first.stderr
    record1=json.loads(first.stdout)
    second=start(root)
    try:
        assert second['pid']==record1['pid']
        assert git(root,'worktree','list','--porcelain').stdout.count('worktree ')==2
    finally:stop(Path(second['state_root']))


def test_custom_runtime_state_root_places_managed_workspaces_consistently(tmp_path,monkeypatch):
    root=repo(tmp_path);home=tmp_path/'runtime-home'
    monkeypatch.setenv('CCB_RUNTIME_STATE_HOME',str(home))
    data=compiled(root)['agents']['worker']
    expected=home/compute_project_id(root)/'workspaces/worker'
    assert normalized(data['workspace_path'])==normalized(expected)
    started=start(root)
    try:assert expected.is_dir() and (expected/'.ccb-workspace.json').is_file()
    finally:stop(Path(started['state_root']))
