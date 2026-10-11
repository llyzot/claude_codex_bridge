"""Explicit native removal in disposable prefixes; never the account install."""
import json, os, uuid
from pathlib import Path
import pytest
from test_native_cli_security import cli
from test_native_installation import install, call
from test_native_installation_upgrade import hashes, start_installed, stop, move_owned, previous, run

pytestmark=pytest.mark.skipif(os.name!='nt',reason='Windows installation identity and image exclusion')

def remove(prefix,*args,ok=True):
    result=cli('installation','remove','--prefix',prefix,*args)
    if ok: assert result.returncode==0,result.stderr
    return json.loads(result.stdout) if result.returncode==0 else result

def journal(prefix):return prefix.parent/f'.{prefix.name}.ccb-native-remove.json'

def test_preview_remove_and_repeat_preserve_project_and_account_surrogates(tmp_path):
    prefix=tmp_path/'native';install(prefix)
    project=tmp_path/'project';project.mkdir();(project/'work.txt').write_text('preserve')
    before=hashes(prefix);operation=str(uuid.uuid4())
    plan=remove(prefix,'--dry-run','--operation-id',operation)
    assert plan['status']=='planned' and plan['runtime_state_changed'] is False
    assert hashes(prefix)==before and not journal(prefix).exists()
    assert prefix.resolve().parent==tmp_path.resolve()
    result=remove(prefix,'--operation-id',operation,'--expected-manifest',plan['identity']['manifest_sha256'])
    assert result['status']=='removed' and not prefix.exists()
    assert not Path(result['retired']).exists()
    assert (project/'work.txt').read_text()=='preserve'
    assert remove(prefix,'--operation-id',operation)['status']=='already_removed'
    install(prefix)
    assert remove(prefix,'--operation-id',operation)['status']=='already_removed'
    assert hashes(prefix)==before

@pytest.mark.parametrize('change',['extra','tampered','manifest'])
def test_unverified_inputs_are_not_removed(tmp_path,change):
    prefix=tmp_path/'native';install(prefix);args=[]
    if change=='extra':(prefix/'personal.txt').write_text('preserve')
    elif change=='tampered':(prefix/'README-native.txt').write_text('changed')
    else:args=['--expected-manifest','0'*64]
    before=hashes(prefix)
    assert remove(prefix,*args,ok=False).returncode!=0
    assert hashes(prefix)==before and not journal(prefix).exists()

def test_running_installation_is_not_removed(tmp_path):
    prefix=tmp_path/'native';install(prefix);before=hashes(prefix)
    proc,state=start_installed(prefix,tmp_path/'project')
    try:
        failed=remove(prefix,ok=False)
        assert failed.returncode!=0 and 'running, locked' in failed.stderr
        assert proc.poll() is None and hashes(prefix)==before and not journal(prefix).exists()
    finally:stop(proc,state)
    assert remove(prefix)['status']=='removed'

@pytest.mark.parametrize('boundary',['prepared','renamed','retired','partial','deleted'])
def test_removal_resumes_recorded_filesystem_boundaries_without_reinstalling(tmp_path,boundary):
    prefix=tmp_path/'native';install(prefix);operation=str(uuid.uuid4())
    plan=remove(prefix,'--dry-run','--operation-id',operation);row=plan['removal']
    retired=Path(plan['retired'])
    if boundary=='deleted':
        remove(prefix,'--operation-id',operation)
        row=json.loads(journal(prefix).read_text(encoding='utf-8'));row['phase']='deleting'
        receipt=tmp_path/f'.native.ccb-native-remove-{operation}.json'
        assert receipt.resolve().parent==tmp_path.resolve();receipt.unlink()
    else:
        if boundary!='prepared':move_owned(prefix,retired,tmp_path)
        if boundary=='retired':row['phase']='retired'
        if boundary=='partial':
            row['phase']='deleting'
            assert retired.parent.samefile(tmp_path)
            (retired/'bin/ask.exe').unlink()
    journal(prefix).write_text(json.dumps(row),encoding='utf-8')
    assert remove(prefix,ok=False).returncode!=0
    assert cli('install','--prefix',prefix).returncode!=0
    assert run('status',prefix)['removal']['phase']==row['phase']
    if boundary=='prepared':
        failed=call(prefix/'bin/ccb.exe','--state-root',tmp_path/'unused','ping')
        assert failed.returncode!=0 and 'unfinished native removal' in failed.stderr
    result=remove(prefix,'--operation-id',operation)
    assert result['status']=='removed' and not prefix.exists() and not retired.exists()

@pytest.mark.parametrize('change',['extra','file-replaced','root-replaced','wrong-operation','wrong-prefix'])
def test_partial_removal_does_not_delete_changed_evidence(tmp_path,change):
    prefix=tmp_path/'native';install(prefix);operation=str(uuid.uuid4())
    plan=remove(prefix,'--dry-run','--operation-id',operation);row=plan['removal'];row['phase']='deleting'
    retired=Path(plan['retired']);move_owned(prefix,retired,tmp_path);args=['--operation-id',operation]
    if change=='extra':(retired/'personal.txt').write_text('preserve')
    if change=='file-replaced':
        old=retired/'LICENSE';replacement=tmp_path/'replacement';replacement.write_bytes(old.read_bytes());replacement.replace(old)
    if change=='root-replaced':
        move_owned(retired,tmp_path/'original-retired',tmp_path);retired.mkdir();(retired/'personal.txt').write_text('preserve')
    if change=='wrong-operation':args=['--operation-id',str(uuid.uuid4())]
    if change=='wrong-prefix':row['prefix_name']='foreign'
    journal(prefix).write_text(json.dumps(row),encoding='utf-8');before=hashes(retired)
    assert remove(prefix,*args,ok=False).returncode!=0
    assert hashes(retired)==before and not prefix.exists()

def test_removing_one_explicit_retained_version_preserves_current_installation(tmp_path):
    prefix=tmp_path/'native';previous(prefix);upgrade=run('upgrade',prefix)
    backup=Path(upgrade['backup']);before=hashes(prefix)
    assert backup.parent.samefile(tmp_path)
    assert remove(backup)['status']=='removed'
    assert hashes(prefix)==before and run('verify',prefix)['status']=='verified'
    assert run('rollback',prefix,ok=False).returncode!=0

def test_pending_upgrade_is_not_removed(tmp_path):
    from test_native_installation_upgrade import record_phase
    prefix=tmp_path/'native';previous(prefix);run('upgrade',prefix);record_phase(prefix,'old_retired')
    before=hashes(prefix)
    assert remove(prefix,ok=False).returncode!=0
    assert hashes(prefix)==before and not journal(prefix).exists()

def test_linked_prefix_is_not_followed(tmp_path):
    import shutil
    prefix=tmp_path/'native';install(prefix);before=hashes(prefix)
    link=tmp_path/'linked-prefix';pwsh=shutil.which('pwsh');assert pwsh
    quote=lambda p:"'"+str(p).replace("'","''")+"'"
    created=call(pwsh,'-NoProfile','-Command',f'New-Item -ItemType Junction -Path {quote(link)} -Target {quote(prefix)} | Out-Null')
    assert created.returncode==0,created.stderr
    try:
        assert remove(link,ok=False).returncode!=0
        assert hashes(prefix)==before
    finally:
        assert link.parent.resolve()==tmp_path.resolve()
        removed=call(pwsh,'-NoProfile','-Command',f'Remove-Item -LiteralPath {quote(link)} -Force')
        assert removed.returncode==0,removed.stderr

def test_old_completed_operation_never_removes_a_later_reinstallation(tmp_path):
    prefix=tmp_path/'native';old=str(uuid.uuid4());later=str(uuid.uuid4())
    install(prefix);remove(prefix,'--operation-id',old)
    install(prefix);remove(prefix,'--operation-id',later)
    install(prefix);before=hashes(prefix)
    assert remove(prefix,'--operation-id',old)['status']=='already_removed'
    assert remove(prefix,'--operation-id',later)['status']=='already_removed'
    assert hashes(prefix)==before

def test_powershell_uninstall_uses_native_removal_and_preserves_account_path(tmp_path):
    import shutil
    from test_native_installation import ROOT, clean_env
    pwsh=shutil.which('pwsh');assert pwsh
    prefix=tmp_path/'native';install(prefix);before=hashes(prefix);env=clean_env(tmp_path)
    registry='[Environment]::GetEnvironmentVariable("Path","User")'
    account_path=call(pwsh,'-NoProfile','-Command',registry).stdout
    operation=str(uuid.uuid4())
    args=['-NoProfile','-File',ROOT/'install.ps1','uninstall','-Runtime','rust','-InstallPrefix',prefix,'-OperationId',operation]
    plan=call(pwsh,*args,'-DryRun',env=env)
    assert plan.returncode==0,plan.stdout+plan.stderr
    assert json.loads(plan.stdout)['status']=='planned' and hashes(prefix)==before
    manifest=json.loads(plan.stdout)['identity']['manifest_sha256']
    removed=call(pwsh,*args,'-ExpectedManifest',manifest,env=env)
    assert removed.returncode==0,removed.stdout+removed.stderr
    assert json.loads(removed.stdout)['status']=='removed' and not prefix.exists()
    assert json.loads(call(pwsh,*args,env=env).stdout)['status']=='already_removed'
    assert call(pwsh,'-NoProfile','-Command',registry).stdout==account_path
    unsafe=call(pwsh,'-NoProfile','-File',ROOT/'install.ps1','uninstall','-Runtime','rust',env=env)
    assert unsafe.returncode!=0 and 'explicit' in unsafe.stderr
