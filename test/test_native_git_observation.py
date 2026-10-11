"""Native review identities cover Git content without mutating the real index."""
import hashlib,json,os,sys
from pathlib import Path
import pytest
from test_native_cli_security import cli
from test_native_daemon import Server,submit,await_watch,spec,BIN as KERNEL
from test_native_workspaces import repo,git,commit
from test_native_claude_stream import claude_spec
from cli.services.workgroup_integration.git_ops import GitOperations

def footprint(root):
    index=Path(git(root,'rev-parse','--path-format=absolute','--git-path','index').stdout.strip())
    objects=Path(git(root,'rev-parse','--path-format=absolute','--git-common-dir').stdout.strip())/'objects'
    return (index.read_bytes() if index.exists() else None,git(root,'show-ref','--head').stdout,
            {p.relative_to(objects).as_posix():hashlib.sha256(p.read_bytes()).hexdigest() for p in objects.rglob('*') if p.is_file()})

def observe(root,ignore=True):
    before=footprint(root)
    result=cli('workspace-tree',root,*([] if ignore else ['--include-controller-state']))
    assert result.returncode==0,result.stderr
    assert footprint(root)==before
    return json.loads(result.stdout)

@pytest.mark.parametrize('change',['clean','modified','untracked','staged-edit','staged-add','staged-delete','mixed','ignored-forced','intent-to-add','rename','literal-names'])
def test_digest_matches_git_tree_and_preserves_index_refs_and_objects(tmp_path,change):
    root=repo(tmp_path)
    if change in ('modified','staged-edit','mixed'):(root/'source.txt').write_text('working change\n')
    if change=='staged-edit':git(root,'add','source.txt')
    if change in ('untracked','staged-add','mixed','intent-to-add'):
        (root/'new.txt').write_text('new content')
        if change in ('staged-add','mixed'):git(root,'add','new.txt')
        if change=='intent-to-add':git(root,'add','-N','new.txt')
    if change=='staged-delete':git(root,'rm','source.txt')
    if change=='rename':git(root,'mv','source.txt','renamed file.txt')
    if change=='ignored-forced':
        with (root/'.gitignore').open('a') as f:f.write('ignored.txt\n')
        (root/'ignored.txt').write_text('explicitly staged');git(root,'add','-f','ignored.txt')
    if change=='literal-names':
        for name in ('[literal].txt','-dash.txt','中文 file.txt'):(root/name).write_text(name,encoding='utf-8')
    actual=observe(root,False)
    git(root,'add','-A','--','.')
    expected=git(root,'write-tree').stdout.strip()
    assert actual['digest']=='git-tree:sha1:'+expected
    assert actual['object_format']=='sha1'

def test_operational_changes_are_ignored_even_when_staged(tmp_path):
    root=repo(tmp_path)
    operational=['docs/plantree/state.txt','.ccb-workspace.json','__pycache__/a.pyc','pkg/__pycache__/b.pyc','.ruff_cache/data','pkg/cache.pyo']
    for name in operational:
        p=root/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_text('baseline')
    git(root,'add','-A');commit(root)
    expected=git(root,'rev-parse','HEAD^{tree}').stdout.strip()
    for name in operational:(root/name).write_text('changed')
    (root/'.ccb/runtime.txt').write_text('controller state')
    git(root,'add','-A')
    assert observe(root)['digest']=='git-tree:sha1:'+expected
    assert observe(root,False)['digest']!='git-tree:sha1:'+expected

def test_unstaged_cases_match_python_and_staged_only_blind_spot_is_fixed(tmp_path,monkeypatch):
    monkeypatch.setenv('CCB_CORE_RUNTIME','python')
    root=repo(tmp_path);(root/'source.txt').write_text('edit')
    oracle=GitOperations(root).current_tree_digest(root,ignore_controller_state=True)
    assert observe(root)['digest']==oracle
    git(root,'add','source.txt')
    old=GitOperations(root).current_tree_digest(root,ignore_controller_state=True)
    actual=observe(root)['digest']
    assert actual!=old
    assert actual=='git-tree:sha1:'+git(root,'write-tree').stdout.strip()

def test_native_python_adapter_uses_same_observer(tmp_path,monkeypatch):
    root=repo(tmp_path);(root/'source.txt').write_text('staged');git(root,'add','source.txt')
    monkeypatch.setenv('CCB_CORE_RUNTIME','rust')
    monkeypatch.setenv('CCB_KERNEL_BIN',str(KERNEL))
    assert GitOperations(root).current_tree_digest(root,ignore_controller_state=True)==observe(root)['digest']

def test_subdirectory_observation_leaves_other_working_changes_outside_scope(tmp_path):
    root=repo(tmp_path);folder=root/'nested';folder.mkdir();(folder/'inside.txt').write_text('old')
    git(root,'add','nested');commit(root)
    baseline=observe(folder)['digest']
    (root/'source.txt').write_text('outside change');git(root,'add','source.txt')
    assert observe(folder)['digest']==baseline
    (folder/'inside.txt').write_text('inside change')
    assert observe(folder)['digest']!=baseline

def test_sha256_repository_is_observed_with_its_own_object_format(tmp_path):
    root=tmp_path/'sha256';root.mkdir()
    initialized=git(root,'init','-q','--object-format=sha256',ok=False)
    if initialized.returncode:pytest.skip('Installed Git lacks SHA-256 repositories')
    (root/'source.txt').write_text('first');git(root,'add','.');commit(root)
    (root/'source.txt').write_text('second');git(root,'add','.')
    actual=observe(root)
    assert actual['digest']=='git-tree:sha256:'+git(root,'write-tree').stdout.strip()

@pytest.mark.parametrize('kind',['unborn','skip-worktree','merge-conflict'])
def test_unrepresentable_git_state_fails_without_mutation(tmp_path,kind):
    root=repo(tmp_path)
    if kind=='unborn':git(root,'checkout','--orphan','empty')
    elif kind=='skip-worktree':git(root,'update-index','--skip-worktree','source.txt')
    else:
        git(root,'checkout','-qb','other');(root/'source.txt').write_text('other');git(root,'add','.');commit(root)
        git(root,'checkout','main');(root/'source.txt').write_text('main');git(root,'add','.');commit(root)
        assert git(root,'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','-c','commit.gpgSign=false','merge','other',ok=False).returncode!=0
        assert git(root,'ls-files','--unmerged').stdout
    before=footprint(root)
    result=cli('workspace-tree',root)
    assert result.returncode!=0
    assert footprint(root)==before

def test_restricted_review_binds_parent_tree_and_overrides_forged_claims(tmp_path):
    root=repo(tmp_path);reviewer=tmp_path/'reviewer';reviewer.mkdir()
    (root/'source.txt').write_text('staged review');git(root,'add','source.txt')
    server=Server(tmp_path/'state',{'builder':dict(spec(),workspace_path=str(root)), 'reviewer':dict(spec(),workspace_path=str(reviewer))})
    try:
        parent=server.rpc('submit',dict(project_id='native-test',to_agent='builder',from_actor='user',body='implement',
            route_options={'allowed_chain_targets':['reviewer'],'bind_chain_workspace_tree':True}))
        server.rpc('work',{'agent_name':'builder'})
        receipt=server.rpc('submit',dict(project_id='native-test',to_agent='reviewer',from_actor='builder',body='review',
            route_options={'mode':'chain','review_workspace_path':'forged','review_tree_digest':'forged'}))
        rows=server.rpc('records.latest',dict(stream='jobs',agent_name='reviewer',identity='job_id'),version=1)['result']
        options=rows[0]['request']['route_options']
        assert Path(options['review_workspace_path']).samefile(root)
        assert options['review_tree_digest']==observe(root)['digest']
        edge=server.rpc('records.latest',dict(stream='callbacks',identity='edge_id'),version=1)['result'][0]
        assert edge['parent_job_id']==parent['job_id'] and edge['child_job_id']==receipt['job_id']
        assert edge['diagnostics']['review_tree_digest']==options['review_tree_digest']
    finally:server.close()

def test_failed_tree_binding_admits_no_child_or_callback(tmp_path):
    root=tmp_path/'not-git';root.mkdir()
    server=Server(tmp_path/'state',{'builder':dict(spec(),workspace_path=str(root)),'reviewer':spec()})
    try:
        server.rpc('submit',dict(project_id='native-test',to_agent='builder',from_actor='user',body='implement',route_options={'bind_chain_workspace_tree':True}))
        server.rpc('work',{'agent_name':'builder'})
        with pytest.raises(AssertionError):server.rpc('submit',dict(project_id='native-test',to_agent='reviewer',from_actor='builder',body='review',route_options={'mode':'chain'}))
        assert server.rpc('records.list',{'stream':'callbacks'},version=1)['result']==[]
        assert server.rpc('records.list',{'stream':'jobs','agent_name':'reviewer'},version=1)['result']==[]
    finally:server.close()

def test_observer_metadata_is_visible_to_native_reviewer_prompt(tmp_path):
    root=repo(tmp_path);trace=tmp_path/'wire.jsonl'
    worker=dict(claude_spec(),workspace_path=str(tmp_path),env={'CCB_FIXTURE_TRACE':str(trace)})
    server=Server(tmp_path/'state',{'builder':dict(spec(),workspace_path=str(root)),'reviewer':worker})
    try:
        server.rpc('submit',dict(project_id='native-test',to_agent='builder',from_actor='user',body='implement',route_options={'bind_chain_workspace_tree':True}))
        server.rpc('work',{'agent_name':'builder'})
        receipt=server.rpc('submit',dict(project_id='native-test',to_agent='reviewer',from_actor='builder',body='review',route_options={'mode':'chain'}))
        await_watch(server,receipt['job_id'],lambda r:r['terminal'])
        rows=[json.loads(s) for s in trace.read_text().splitlines()]
        content=[r['message']['content'] for r in rows if r['type']=='user'][0]
        assert 'CCB bound review workspace:' in content and observe(root)['digest'] in content
    finally:server.close()


def test_dirty_submodule_is_not_mistaken_for_unchanged_content(tmp_path):
    child=repo(tmp_path,'child');root=repo(tmp_path,'parent')
    git(root,'-c','protocol.file.allow=always','submodule','add','-q',child,'module')
    commit(root)
    (root/'module/source.txt').write_text('uncommitted submodule content')
    before=footprint(root)
    result=cli('workspace-tree',root)
    assert result.returncode!=0 and 'submodule' in result.stderr
    assert footprint(root)==before


def test_tree_probe_handles_linked_git_worktree(tmp_path):
    root=repo(tmp_path);work=tmp_path/'linked'
    git(root,'worktree','add','-qb','linked',work)
    (work/'source.txt').write_text('linked change');git(work,'add','source.txt')
    actual=observe(work)
    assert actual['digest']=='git-tree:sha1:'+git(work,'write-tree').stdout.strip()
    assert (root/'source.txt').read_text()=='initial\n'


def test_unbound_api_requests_cannot_publish_spoofed_tree_claims(tmp_path):
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        server.rpc('submit',dict(project_id='native-test',to_agent='worker',from_actor='user',body='task',
            route_options={'review_workspace_path':'forged','review_tree_digest':'forged'}))
        row=server.rpc('records.latest',dict(stream='jobs',agent_name='worker',identity='job_id'),version=1)['result'][0]
        assert 'review_workspace_path' not in row['request']['route_options']
        assert 'review_tree_digest' not in row['request']['route_options']
    finally:server.close()


def test_private_kernel_observation_uses_same_implementation(tmp_path):
    root=repo(tmp_path);(root/'source.txt').write_text('private operation')
    server=Server(tmp_path/'state',{'worker':spec()})
    try:
        result=server.rpc('git.current_tree',{'workspace_path':str(root),'ignore_controller_state':True},version=1)['result']
        assert result['digest']==observe(root)['digest']
    finally:server.close()
