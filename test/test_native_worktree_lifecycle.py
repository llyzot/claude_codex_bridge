"""Lifecycle operations use disposable Git repos, never the active user project."""
import json
from pathlib import Path
import uuid
import os
import shutil
import subprocess
import time
import pytest
from test_native_workspaces import repo, git, start, stop
from test_native_cli_security import cli, BIN

def invoke(root,action,target,*args,ok=True):
    result=cli('--project',root,'worktree',action,target,*args,timeout=30)
    if ok:assert result.returncode==0,result.stderr
    return json.loads(result.stdout) if ok else result

@pytest.fixture
def managed(tmp_path):
    root=repo(tmp_path,'source with spaces')
    git(root,'config','user.name','Fixture');git(root,'config','user.email','fixture@example.invalid')
    created=start(root,timeout=30);state=Path(created['state_root']);stop(state)
    return root,root/'.ccb/workspaces/worker',state

def commit_file(work,text='worker change\n'):
    (work/'source.txt').write_text(text,encoding='utf-8');git(work,'add','source.txt')
    git(work,'-c','commit.gpgSign=false','commit','-qm','worker change')
    return git(work,'rev-parse','HEAD').stdout.strip()

def test_merge_and_replay_publish_only_one_correlated_result(managed):
    root,work,_=managed;source=commit_file(work);before=git(root,'rev-parse','HEAD').stdout.strip()
    operation=str(uuid.uuid4())
    dry=invoke(root,'merge','worker','--into','main','--operation-id',operation,'--dry-run')
    assert dry['status']=='planned' and git(root,'rev-parse','HEAD').stdout.strip()==before
    merged=invoke(root,'merge','worker','--into','main','--operation-id',operation)
    assert merged['status']=='completed' and (root/'source.txt').read_text()=='worker change\n'
    head=git(root,'rev-parse','HEAD').stdout.strip()
    assert git(root,'rev-list','--parents','-n','1',head).stdout.split()[1:]==[before,source]
    assert invoke(root,'recover',operation)['result_commit']==head
    assert git(root,'rev-parse','HEAD').stdout.strip()==head
    already=invoke(root,'merge','worker','--into','main')
    assert already['result_commit']==head and not Path(already['stage_path']).exists()
    assert git(root,'rev-parse','HEAD').stdout.strip()==head

def test_retirement_moves_dirty_index_untracked_and_ignored_files_and_restores(managed):
    root,work,_=managed
    (work/'source.txt').write_text('staged\n');git(work,'add','source.txt')
    (work/'source.txt').write_text('unstaged\n');(work/'untracked.txt').write_text('keep\n')
    (work/'.gitignore').write_text('.ccb/\nprivate-cache/\n');(work/'private-cache').mkdir()
    (work/'private-cache/private.bin').write_bytes(b'\x00keep ignored\xff')
    index=git(work,'diff','--cached').stdout
    retired=invoke(root,'retire','worker')
    archive=Path(retired['archive_path']);assert retired['status']=='completed' and not work.exists()
    assert (archive/'source.txt').read_text()=='unstaged\n' and (archive/'untracked.txt').read_text()=='keep\n'
    assert (archive/'private-cache/private.bin').read_bytes()==b'\x00keep ignored\xff'
    assert git(archive,'diff','--cached').stdout==index
    refused=invoke(root,'purge',retired['operation_id'],'--into','main',ok=False)
    assert refused.returncode!=0 and archive.exists()
    restored=invoke(root,'restore',retired['operation_id'])
    assert restored['status']=='completed' and work.exists() and not archive.exists()
    assert git(work,'diff','--cached').stdout==index
    assert invoke(root,'recover',restored['operation_id'])['status']=='completed'

def test_conflict_keeps_target_and_recovery_commits_operator_resolution(managed):
    root,work,_=managed;commit_file(work,'worker side\n')
    (root/'source.txt').write_text('target side\n');git(root,'add','source.txt');git(root,'-c','commit.gpgSign=false','commit','-qm','target change')
    before=git(root,'rev-parse','HEAD').stdout.strip()
    result=invoke(root,'merge','worker','--into','main')
    assert result['status']=='conflicted' and git(root,'rev-parse','HEAD').stdout.strip()==before
    stage=Path(result['stage_path']);assert '<<<<<<<' in (stage/'source.txt').read_text()
    (stage/'source.txt').write_text('operator resolution\n');git(stage,'add','source.txt')
    finished=invoke(root,'recover',result['operation_id'])
    assert finished['status']=='completed' and (root/'source.txt').read_text()=='operator resolution\n'

def test_live_daemon_blocks_lifecycle_without_git_changes(managed):
    root,work,_=managed;created=start(root)
    try:
        before=git(root,'worktree','list','--porcelain').stdout
        refused=invoke(root,'retire','worker',ok=False)
        assert refused.returncode!=0 and work.exists()
        assert git(root,'worktree','list','--porcelain').stdout==before
    finally:stop(Path(created['state_root']))

def test_clean_integrated_archive_can_be_purged_without_branch_loss(managed):
    root,work,_=managed;head=commit_file(work)
    invoke(root,'merge','worker','--into','main')
    retired=invoke(root,'retire','worker');archive=Path(retired['archive_path'])
    cleaned=invoke(root,'purge',retired['operation_id'],'--into','main')
    assert cleaned['status']=='completed' and not archive.exists()
    assert git(root,'rev-parse','refs/heads/ccb/worker').stdout.strip()==head
    assert invoke(root,'recover',cleaned['operation_id'])['status']=='completed'

def test_branch_removal_is_explicit_and_archive_commit_remains_pinned(managed):
    root,work,_=managed;head=commit_file(work)
    invoke(root,'merge','worker','--into','main')
    retired=invoke(root,'retire','worker')
    invoke(root,'purge',retired['operation_id'],'--into','main','--delete-branch')
    assert git(root,'show-ref','--verify','refs/heads/ccb/worker',ok=False).returncode!=0
    assert git(root,'rev-parse','refs/ccb/archive/'+retired['operation_id']).stdout.strip()==head

def test_retired_workspace_is_not_recreated_and_old_recovery_preserves_new_pointer(managed):
    root,work,_=managed
    retired=invoke(root,'retire','worker')
    rejected=cli('config','compile',root,root.parent/(root.name+'-executables.json'))
    assert rejected.returncode!=0 and not work.exists()
    restored=invoke(root,'restore',retired['operation_id'])
    assert invoke(root,'recover',retired['operation_id'])['status']=='completed'
    assert invoke(root,'inspect','worker')['lifecycle']['operation_id']==restored['operation_id']
    launched=start(root);stop(Path(launched['state_root']))

def test_dirty_source_and_unmerged_archive_are_refused(managed):
    root,work,_=managed
    (work/'untracked').write_text('keep')
    before=git(root,'rev-parse','HEAD').stdout
    assert invoke(root,'merge','worker','--into','main',ok=False).returncode!=0
    assert git(root,'rev-parse','HEAD').stdout==before
    (work/'untracked').unlink();commit_file(work)
    retired=invoke(root,'retire','worker')
    assert invoke(root,'purge',retired['operation_id'],'--into','main',ok=False).returncode!=0
    assert Path(retired['archive_path']).exists()

def test_recovery_rejects_tampered_archive_path(managed):
    root,work,state=managed;retired=invoke(root,'retire','worker')
    receipt=state.parent/'worktree-operations'/retired['operation_id']/'operation.json'
    row=json.loads(receipt.read_text());row['archive_path']=str(root)
    receipt.write_text(json.dumps(row))
    assert invoke(root,'recover',retired['operation_id'],ok=False).returncode!=0
    assert root.exists() and Path(retired['archive_path']).exists()

def test_completed_receipt_repairs_lost_final_pointer(managed):
    root,_,_=managed;retired=invoke(root,'retire','worker')
    pointer=root/'.ccb/workspaces/worker.ccb-native-lifecycle.json'
    row=json.loads(pointer.read_text());row['state']='pending';pointer.write_text(json.dumps(row))
    invoke(root,'recover',retired['operation_id'])
    assert invoke(root,'inspect','worker')['lifecycle']['state']=='retired'

def test_prepared_receipt_without_pointer_or_pin_can_resume(managed):
    root,work,state=managed;retired=invoke(root,'retire','worker')
    archive=Path(retired['archive_path']);git(root,'worktree','move',archive,work)
    (work/'.ccb-workspace.json').write_text(json.dumps(retired['binding']))
    git(root,'update-ref','-d','refs/ccb/archive/'+retired['operation_id'])
    (root/'.ccb/workspaces/worker.ccb-native-lifecycle.json').unlink()
    receipt=state.parent/'worktree-operations'/retired['operation_id']/'operation.json'
    retired.update(phase='prepared',status='pending');receipt.write_text(json.dumps(retired))
    completed=invoke(root,'recover',retired['operation_id'])
    assert completed['status']=='completed' and archive.exists() and not work.exists()
    assert git(root,'rev-parse','refs/ccb/archive/'+retired['operation_id']).stdout.strip()==retired['source_commit']

def test_recover_does_not_silently_accept_mutation_options(managed):
    root,_,_=managed;retired=invoke(root,'retire','worker')
    assert invoke(root,'recover',retired['operation_id'],'--dry-run',ok=False).returncode!=0

def test_merge_target_movement_during_conflict_blocks_publication(managed):
    root,work,_=managed;commit_file(work,'worker side\n')
    (root/'source.txt').write_text('target side\n');git(root,'add','source.txt')
    git(root,'-c','commit.gpgSign=false','commit','-qm','target change')
    pending=invoke(root,'merge','worker','--into','main');stage=Path(pending['stage_path'])
    (stage/'source.txt').write_text('resolved\n');git(stage,'add','source.txt')
    (root/'other.txt').write_text('target moved\n');git(root,'add','other.txt')
    git(root,'-c','commit.gpgSign=false','commit','-qm','target advanced')
    before=git(root,'rev-parse','HEAD').stdout
    assert invoke(root,'recover',pending['operation_id'],ok=False).returncode!=0
    assert git(root,'rev-parse','HEAD').stdout==before and stage.exists()

def test_operation_id_replay_rejects_changed_retirement_arguments(managed):
    root,_,_=managed
    first=invoke(root,'retire','worker')
    restored=invoke(root,'restore',first['operation_id'])
    second=invoke(root,'retire','worker')
    assert invoke(root,'restore',second['operation_id'],'--operation-id',restored['operation_id'],ok=False).returncode!=0
    assert Path(second['archive_path']).exists()

def test_retained_stage_user_files_block_target_publication(managed):
    root,work,state=managed;commit_file(work)
    merged=invoke(root,'merge','worker','--into','main')
    # Reconstruct the exact post-merge/pre-publication boundary in this fixture.
    git(root,'checkout','--detach',merged['target_before'])
    git(root,'branch','-f','main',merged['target_before']);git(root,'checkout','main')
    stage=Path(merged['stage_path'])
    stage_arg=str(stage).removeprefix('\\\\?\\') if os.name=='nt' else str(stage)
    git(root,'worktree','add','--detach',stage_arg,merged['result_commit'])
    (stage/'operator-notes.txt').write_text('do not delete or publish yet')
    receipt=state.parent/'worktree-operations'/merged['operation_id']/'operation.json'
    merged.update(phase='merge_committed',status='pending');receipt.write_text(json.dumps(merged))
    pointer=root/'.ccb/workspaces/worker.ccb-native-lifecycle.json'
    row=json.loads(pointer.read_text());row['state']='pending';pointer.write_text(json.dumps(row))
    assert invoke(root,'recover',merged['operation_id'],ok=False).returncode!=0
    assert git(root,'rev-parse','HEAD').stdout.strip()==merged['target_before']
    assert (stage/'operator-notes.txt').read_text()=='do not delete or publish yet'

def test_missing_archive_without_removal_intent_is_uncertain(managed):
    root,work,state=managed;commit_file(work);invoke(root,'merge','worker','--into','main')
    retired=invoke(root,'retire','worker');purged=invoke(root,'purge',retired['operation_id'],'--into','main')
    receipt=state.parent/'worktree-operations'/purged['operation_id']/'operation.json'
    purged.update(phase='prepared',status='pending');receipt.write_text(json.dumps(purged))
    assert invoke(root,'recover',purged['operation_id'],ok=False).returncode!=0

@pytest.fixture(scope='module')
def gated_git(tmp_path_factory):
    """Run real Git, then pause its wrapper before CCB receives the outcome."""
    base=tmp_path_factory.mktemp('git-after-effect')
    source=base/'gate.rs';binary=base/('git.exe' if os.name=='nt' else 'git')
    source.write_text(r'''
use std::{env,fs,io::{self,Write},process::Command,thread,time::Duration};
fn main(){
 let args:Vec<_>=env::args_os().skip(1).collect();
 let out=Command::new(env::var_os("CCB_FIXTURE_REAL_GIT").unwrap()).args(&args).output().unwrap();
 let text:Vec<_>=args.iter().map(|a|a.to_string_lossy()).collect();
 let gate=env::var("CCB_FIXTURE_GATE").unwrap_or_default();
 let matched=match gate.as_str(){
  "commit"=>text.iter().any(|a|a=="--no-ff"),
  "publish"=>text.iter().any(|a|a=="--ff-only"),
  "move"=>text.windows(2).any(|a|a==["worktree","move"]),
  "remove"=>text.windows(2).any(|a|a==["worktree","remove"]),
  _=>false};
 if out.status.success() && matched {
  fs::write(env::var_os("CCB_FIXTURE_READY").unwrap(),std::process::id().to_string()).unwrap();
  while !std::path::Path::new(&env::var_os("CCB_FIXTURE_RELEASE").unwrap()).exists(){thread::sleep(Duration::from_millis(10));}
 }
 io::stdout().write_all(&out.stdout).unwrap();io::stderr().write_all(&out.stderr).unwrap();
 std::process::exit(out.status.code().unwrap_or(1));
}
''')
    built=subprocess.run(['rustc',str(source),'-o',str(binary)],capture_output=True,text=True,timeout=60)
    assert built.returncode==0,built.stderr
    return base,shutil.which('git')

@pytest.mark.parametrize('action,gate',[('merge','commit'),('merge','publish'),('merge','remove'),
                                      ('retire','move'),('restore','move'),('purge','remove')])
def test_process_death_after_git_effect_recovers_without_duplicate_or_file_loss(managed,gated_git,tmp_path,action,gate):
    root,work,_=managed;source=commit_file(work)
    if action in ('restore','purge'):
        invoke(root,'merge','worker','--into','main')
        retired=invoke(root,'retire','worker');target=retired['operation_id']
    else:target='worker'
    if action=='retire':(work/'untracked.txt').write_text('preserve crash data')
    operation=str(uuid.uuid4());ready=tmp_path/'gate-ready';release=tmp_path/'gate-release'
    env={**os.environ,'PATH':str(gated_git[0])+os.pathsep+os.environ['PATH'],
         'CCB_FIXTURE_REAL_GIT':gated_git[1],'CCB_FIXTURE_GATE':gate,
         'CCB_FIXTURE_READY':str(ready),'CCB_FIXTURE_RELEASE':str(release)}
    args=[str(BIN),'--project',str(root),'worktree',action,target,'--operation-id',operation]
    if action in ('merge','purge'):args+=['--into','main']
    process=subprocess.Popen(args,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    try:
        deadline=time.monotonic()+20
        while not ready.exists() and process.poll() is None and time.monotonic()<deadline:time.sleep(.02)
        assert ready.exists(),process.communicate(timeout=1) if process.poll() is not None else 'Git gate not reached'
        process.terminate();process.wait(timeout=5)
    finally:
        release.touch()
        if process.poll() is None:process.kill()
        process.communicate(timeout=5)
    recovered=invoke(root,'recover',operation)
    assert recovered['status']=='completed'
    head=git(root,'rev-parse','HEAD').stdout
    assert invoke(root,'recover',operation)['status']=='completed'
    assert git(root,'rev-parse','HEAD').stdout==head
    if action=='merge':
        assert git(root,'rev-list','--parents','-n','1','HEAD').stdout.split()[2]==source
        assert not Path(recovered['stage_path']).exists()
    elif action=='retire':
        assert not work.exists() and (Path(recovered['archive_path'])/'untracked.txt').read_text()=='preserve crash data'
    elif action=='restore':assert work.exists() and not Path(retired['archive_path']).exists()
    else:assert not Path(retired['archive_path']).exists()
