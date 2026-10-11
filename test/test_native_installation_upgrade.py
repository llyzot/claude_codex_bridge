"""Stopped-prefix swaps, retained versions and recovery; never an account install."""
import concurrent.futures,hashlib,json,os,subprocess,uuid
from pathlib import Path
import pytest
from test_native_cli_security import cli
from test_native_installation import install,call

pytestmark=pytest.mark.skipif(os.name!='nt',reason='Windows image exclusion contract')

def previous(prefix):
    install(prefix)
    marker=prefix/'NATIVE_INSTALLATION.json'
    data=json.loads(marker.read_text())
    # A PE overlay produces a distinct executable build without changing its
    # behavior, so tests need no download or build of an obsolete runtime.
    with (prefix/'bin/ccb-native.exe').open('ab') as file:file.write(b'CCB_OLD_BUILD_FIXTURE')
    sha=hashlib.sha256((prefix/'bin/ccb-native.exe').read_bytes()).hexdigest()
    for name in ['ccb.exe','ask.exe','ccb-native.exe']:data['files']['bin/'+name]=sha
    data['binary_sha256']=sha;data['version']='fixture-previous'
    (prefix/'VERSION').write_text('fixture-previous')
    data['files']['VERSION']=hashlib.sha256(b'fixture-previous').hexdigest()
    marker.write_text(json.dumps(data),encoding='utf-8')
    assert cli('installation','verify','--prefix',prefix).returncode==0
    return data

def run(action,prefix,*args,ok=True):
    result=cli('installation',action,'--prefix',prefix,*args)
    if ok:assert result.returncode==0,result.stderr
    return json.loads(result.stdout) if result.returncode==0 else result

def journal(prefix):return prefix.parent/f'.{prefix.name}.ccb-native-upgrade.json'
def read_record(prefix):return json.loads(journal(prefix).read_text())
def record_phase(prefix,phase):
    row=read_record(prefix);row['phase']=phase;journal(prefix).write_text(json.dumps(row))
def move_owned(source,destination,parent):
    normalize=lambda p:str(p.resolve()).removeprefix('\\\\?\\').casefold()
    assert normalize(source.resolve().parent)==normalize(parent)
    assert normalize(destination.parent)==normalize(parent) and not destination.exists()
    source.rename(destination)
def hashes(prefix):return {str(p.relative_to(prefix)):hashlib.sha256(p.read_bytes()).hexdigest() for p in prefix.rglob('*') if p.is_file()}

def test_upgrade_preview_cutover_aliases_and_rollback_preserve_both_builds(tmp_path):
    prefix=tmp_path/'native';old=previous(prefix);before=hashes(prefix)
    operation=str(uuid.uuid4())
    plan=run('upgrade',prefix,'--dry-run','--operation-id',operation)
    assert hashes(prefix)==before and not journal(prefix).exists()
    assert plan['requires_stopped_native_commands'] and plan['active_use_check']=='performed_on_apply'
    result=run('upgrade',prefix,'--operation-id',operation,'--expected-manifest',plan['from']['manifest_sha256'])
    assert result['status']=='upgraded' and result['upgrade']['phase']=='committed'
    assert result['runtime_state_changed'] is False
    again=run('upgrade',prefix,'--operation-id',operation,'--expected-manifest',plan['from']['manifest_sha256'])
    assert again['status']=='already_upgraded' and again['upgrade']['operation_id']==operation
    backup=Path(result['backup']);assert hashes(backup)==before
    assert run('verify',prefix)['binary_sha256']!=old['binary_sha256']
    for name in ['ccb','ask','ccb-native']:
        output=call(prefix/f'bin/{name}.exe','--capabilities')
        assert output.returncode==0,output.stderr
        assert json.loads(output.stdout)['runtime']=='rust'
    assert run('upgrade',prefix)['status']=='already_current'
    assert run('status',prefix)['observed']['target']['state']=='replacement'
    restored=run('rollback',prefix,'--operation-id',operation)
    assert restored['status']=='rolled_back' and hashes(prefix)==before
    assert Path(restored['stage']).is_dir() and not backup.exists()
    assert run('verify',Path(restored['stage']))['binary_sha256']==result['upgrade']['to']['binary_sha256']
    assert run('rollback',prefix,'--operation-id',operation)['status']=='already_rolled_back'
    assert run('upgrade',prefix,'--operation-id',operation,ok=False).returncode!=0
    assert hashes(prefix)==before

@pytest.mark.parametrize('phase,layout',[('prepared','a'),('prepared','b'),('old_retired','b'),('old_retired','c'),
                                        ('rollback_prepared','c'),('rollback_prepared','b'),('new_retired','b'),('new_retired','a')])
def test_recovery_handles_every_directory_rename_boundary(tmp_path,phase,layout):
    prefix=tmp_path/'native';previous(prefix)
    completed=run('upgrade',prefix)
    stage=Path(completed['stage']);backup=Path(completed['backup'])
    if layout in {'a','b'}:move_owned(prefix,stage,tmp_path)
    if layout=='a':move_owned(backup,prefix,tmp_path)
    record_phase(prefix,phase)
    assert run('status',prefix)['upgrade']['phase']==phase
    before={p.name:hashes(p) for p in [prefix,stage,backup] if p.exists()}
    assert run('recover',prefix,'--dry-run')['upgrade']['phase']==phase
    assert before=={p.name:hashes(p) for p in [prefix,stage,backup] if p.exists()}
    # Ordinary install must never fill the temporary gap in a recorded swap.
    blocked=cli('install','--prefix',prefix)
    assert blocked.returncode!=0 and 'unfinished native upgrade' in blocked.stderr
    result=run('recover',prefix)
    forward=phase in {'prepared','old_retired'}
    assert result['status']==('upgraded' if forward else 'rolled_back')
    assert run('verify',prefix)['binary_sha256']==completed['upgrade']['to' if forward else 'from']['binary_sha256']
    assert run('recover',prefix)['status']==('already_upgraded' if forward else 'already_rolled_back')

@pytest.mark.parametrize('change',['extra','tampered','journal','wrong-manifest','wrong-operation'])
def test_changed_inputs_fail_without_replacing_or_deleting_evidence(tmp_path,change):
    prefix=tmp_path/'native';previous(prefix)
    args=[]
    if change=='extra':(prefix/'personal.txt').write_text('preserve')
    elif change=='tampered':(prefix/'README-native.txt').write_text('preserve changed managed file')
    elif change=='journal':journal(prefix).write_text('{invalid')
    elif change=='wrong-manifest':args=['--expected-manifest','0'*64]
    else:args=['--operation-id','not-a-uuid']
    before=hashes(prefix)
    result=run('upgrade',prefix,*args,ok=False)
    assert result.returncode!=0
    assert hashes(prefix)==before
    assert not list(tmp_path.glob('*.stage')) and not list(tmp_path.glob('*.backup'))

def start_installed(prefix,root):
    root.mkdir()
    config=root/'config.json';config.write_text(json.dumps({'schema_version':2,'project_id':'upgrade-test','agents':{'worker':{'provider':'test','command':None}}}))
    state=root/'runtime/ccbd'
    proc=subprocess.Popen([str(prefix/'bin/ccb.exe'),'--state-root',str(state),'daemon','--config',str(config)],
                          stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8',creationflags=0x08000000)
    assert proc.stdout.readline(),proc.stderr.read()
    return proc,state
def stop(proc,state):
    result=cli('--state-root',state,'stop')
    if result.returncode!=0:proc.kill()
    proc.wait(timeout=5)
    assert result.returncode==0,result.stderr

def test_mapped_image_blocks_upgrade_while_an_independent_prefix_can_run(tmp_path):
    prefix=tmp_path/'native';previous(prefix);before=hashes(prefix)
    other=tmp_path/'other';install(other)
    proc,state=start_installed(prefix,tmp_path/'first')
    peer,peer_state=start_installed(other,tmp_path/'peer')
    try:
        assert run('upgrade',prefix,'--dry-run')['status']=='planned'
        result=run('upgrade',prefix,ok=False)
        assert result.returncode!=0 and 'running, locked' in result.stderr
        assert hashes(prefix)==before and not journal(prefix).exists()
        stop(proc,state)
        assert run('upgrade',prefix)['status']=='upgraded'
        assert peer.poll() is None and cli('--state-root',peer_state,'ping').returncode==0
    finally:
        if proc.poll() is None:stop(proc,state)
        stop(peer,peer_state)

def test_running_replacement_blocks_rollback_without_affecting_it(tmp_path):
    prefix=tmp_path/'native';previous(prefix);result=run('upgrade',prefix)
    before=hashes(prefix);proc,state=start_installed(prefix,tmp_path/'runtime')
    try:
        failed=run('rollback',prefix,ok=False)
        assert failed.returncode!=0 and 'running, locked' in failed.stderr
        assert read_record(prefix)['phase']=='committed'
        assert hashes(prefix)==before and proc.poll() is None
    finally:stop(proc,state)
    assert run('rollback',prefix)['upgrade']['operation_id']==result['upgrade']['operation_id']

def test_tampered_backup_is_not_restored_and_pending_gap_is_preserved(tmp_path):
    prefix=tmp_path/'native';previous(prefix);result=run('upgrade',prefix)
    backup=Path(result['backup']);stage=Path(result['stage'])
    move_owned(prefix,stage,tmp_path);record_phase(prefix,'old_retired')
    (backup/'README-native.txt').write_text('unexpected user data')
    before=hashes(backup)
    failed=run('recover',prefix,ok=False)
    assert failed.returncode!=0 and not prefix.exists()
    assert hashes(backup)==before and stage.is_dir()

def test_relocation_keeps_relative_transaction_paths_and_rejects_foreign_journal(tmp_path):
    parent=tmp_path/'parent';parent.mkdir();prefix=parent/'native';previous(prefix)
    result=run('upgrade',prefix);record_phase(prefix,'old_retired')
    moved=tmp_path/'moved';move_owned(parent,moved,tmp_path)
    prefix=moved/'native'
    assert run('recover',prefix)['status']=='upgraded'
    row=read_record(prefix);row['prefix_name']='foreign';journal(prefix).write_text(json.dumps(row))
    assert run('rollback',prefix,ok=False).returncode!=0
    assert run('verify',prefix)['binary_sha256']==result['upgrade']['to']['binary_sha256']


def test_published_but_uncommitted_installation_refuses_runtime_commands(tmp_path):
    prefix=tmp_path/'native';previous(prefix);run('upgrade',prefix)
    record_phase(prefix,'old_retired')
    state=tmp_path/'runtime/ccbd'
    failed=call(prefix/'bin/ccb.exe','--state-root',state,'ping')
    assert failed.returncode!=0 and 'unfinished native upgrade' in failed.stderr
    assert not state.exists()
    assert call(prefix/'bin/ccb.exe','--version').returncode==0
    assert run('recover',prefix)['status']=='upgraded'


def test_concurrent_upgrades_publish_one_replacement_and_keep_one_backup(tmp_path):
    prefix=tmp_path/'native';previous(prefix)
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        results=list(pool.map(lambda _:cli('installation','upgrade','--prefix',prefix),range(2)))
    assert any(r.returncode==0 for r in results)
    assert read_record(prefix)['phase']=='committed'
    assert len(list(tmp_path.glob('*.backup')))==1
    assert not list(tmp_path.glob('*.stage'))
    assert run('verify',prefix)['status']=='verified'


def test_powershell_explicit_upgrade_uses_native_transaction(tmp_path):
    from test_native_installation import ROOT
    pwsh='C:/Users/user/scoop/shims/pwsh.exe'
    prefix=tmp_path/'native';previous(prefix);before=hashes(prefix)
    args=['-NoProfile','-File',ROOT/'install.ps1','install','-Runtime','rust','-InstallPrefix',prefix,'-Upgrade']
    preview=call(pwsh,*args,'-DryRun')
    assert preview.returncode==0,preview.stderr
    assert hashes(prefix)==before
    installed=call(pwsh,*args)
    assert installed.returncode==0,installed.stderr
    assert read_record(prefix)['phase']=='committed'
    rejected=call(pwsh,'-NoProfile','-File',ROOT/'install.ps1','install','-Runtime','python','-Upgrade','-InstallPrefix',tmp_path/'unused')
    assert rejected.returncode!=0 and not (tmp_path/'unused').exists()


def test_recovery_preview_keeps_identity_and_manifest_preconditions(tmp_path):
    prefix=tmp_path/'native';previous(prefix);run('upgrade',prefix)
    before=hashes(prefix)
    assert run('recover',prefix,'--dry-run','--operation-id',str(uuid.uuid4()),ok=False).returncode!=0
    assert run('recover',prefix,'--dry-run','--expected-manifest','0'*64,ok=False).returncode!=0
    assert hashes(prefix)==before and read_record(prefix)['phase']=='committed'


def test_exact_upgrade_retry_recovers_a_lost_final_receipt(tmp_path):
    prefix=tmp_path/'native';previous(prefix);completed=run('upgrade',prefix)
    record_phase(prefix,'old_retired')
    record=completed['upgrade']
    result=run('upgrade',prefix,'--operation-id',record['operation_id'],'--expected-manifest',record['from']['manifest_sha256'])
    assert result['status']=='upgraded' and result['upgrade']['phase']=='committed'
    assert run('verify',prefix)['binary_sha256']==record['to']['binary_sha256']
