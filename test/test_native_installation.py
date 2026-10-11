"""Real Rust installs/commands in private prefixes; no account installation."""
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time

import pytest
from test_native_cli_security import BIN,cli
from test_native_daemon import spec
from test_native_pi_rpc import pi_spec
from test_native_codex_rpc import codex_spec
from test_native_claude_stream import claude_spec
from native_kernel import _owner_stopped

pytestmark=pytest.mark.skipif(os.name!='nt',reason='native installation currently targets Windows x64')
ROOT=Path(__file__).resolve().parents[1]
STUB=ROOT/'test/stubs/native_installed_command.py'


def call(executable,*args,**kwargs):
    return subprocess.run([str(executable),*map(str,args)],capture_output=True,text=True,encoding='utf-8',timeout=15,**kwargs)


def install(prefix,**kwargs):
    result=cli('install','--prefix',prefix,**kwargs)
    assert result.returncode==0,result.stderr
    assert json.loads(result.stdout)['runtime']=='rust'
    return prefix/'bin/ccb.exe'


def clean_env(tmp_path):
    shadow=tmp_path/'shadow-bin';shadow.mkdir(exist_ok=True)
    for name in ['python.exe','py.exe','ask.exe','ccb.exe']:(shadow/name).write_bytes(b'MZnot-an-executable')
    env=os.environ.copy()
    for name in ['CCB_NATIVE_CLI','CCB_CORE_RUNTIME','CCB_NATIVE_TOKEN_FILE','CCB_NATIVE_ENDPOINT','CCB_AGENT_NAME']:
        env.pop(name,None)
    env['PATH']=str(shadow)
    env['CCB_PYTHON']=str(shadow/'python.exe')
    return env


def test_dry_run_and_native_prefix_install_require_no_python_or_environment_activation(tmp_path):
    env=clean_env(tmp_path);prefix=tmp_path/'native 中文 & space'
    dry=cli('install','--prefix',prefix,'--dry-run',env=env)
    assert dry.returncode==0,dry.stderr
    assert json.loads(dry.stdout)['status']=='planned' and not prefix.exists()
    ccb=install(prefix,env=env)
    assert not (prefix/'.venv').exists() and not list(prefix.rglob('*.py'))
    for executable in [ccb,prefix/'bin/ask.exe',prefix/'bin/ccb-native.exe']:
        version=call(executable,'--print-version',env=env)
        assert version.returncode==0,version.stderr
        assert (ROOT/'VERSION').read_text().strip() in version.stdout
        capabilities=call(executable,'--capabilities',env=env)
        assert capabilities.returncode==0,capabilities.stderr
        contract=json.loads(capabilities.stdout)
        assert contract['native_prefix_installation']['supported'] and contract['runtime']=='rust'
    verified=call(ccb,'installation','verify','--prefix',prefix,env=env)
    assert verified.returncode==0,verified.stderr
    assert json.loads(verified.stdout)['status']=='verified'
    again=cli('install','--prefix',prefix,env=env)
    assert again.returncode==0 and json.loads(again.stdout)['status']=='already_installed'
    assert (prefix/'LICENSE').read_bytes()==(ROOT/'LICENSE').read_bytes()
    assert not list(tmp_path.glob('.ccb-native-install-*'))


@pytest.mark.parametrize('kind',['file','empty-directory','populated-directory','relative','missing-parent'])
def test_install_rejects_unowned_or_invalid_destination_without_replacing_files(tmp_path,kind):
    prefix=tmp_path/'destination'
    if kind=='file':prefix.write_bytes(b'preserve')
    elif kind in {'empty-directory','populated-directory'}:
        prefix.mkdir()
        if kind=='populated-directory':(prefix/'user.txt').write_text('preserve')
    elif kind=='relative':prefix=Path('relative-native-prefix')
    else:prefix=tmp_path/'missing'/'prefix'
    before={str(p.relative_to(tmp_path)):p.read_bytes() for p in tmp_path.rglob('*') if p.is_file()}
    result=cli('install','--prefix',prefix,cwd=tmp_path)
    assert result.returncode!=0
    after={str(p.relative_to(tmp_path)):p.read_bytes() for p in tmp_path.rglob('*') if p.is_file()}
    assert after==before and not (tmp_path/'relative-native-prefix').exists()


def test_verification_detects_binary_damage_and_install_does_not_repair_over_user_changes(tmp_path):
    prefix=tmp_path/'native';install(prefix)
    altered=prefix/'bin/ask.exe'
    with altered.open('ab') as stream:stream.write(b'tampered')
    before=altered.stat().st_size
    assert cli('installation','verify','--prefix',prefix).returncode!=0
    assert cli('install','--prefix',prefix).returncode!=0
    assert altered.stat().st_size==before


def test_existing_different_release_requires_a_new_prefix(tmp_path):
    prefix=tmp_path/'native';install(prefix)
    marker=prefix/'NATIVE_INSTALLATION.json';record=json.loads(marker.read_text())
    (prefix/'VERSION').write_text('previous-release')
    record['version']='previous-release'
    record['files']['VERSION']=hashlib.sha256(b'previous-release').hexdigest()
    marker.write_text(json.dumps(record),encoding='utf-8')
    assert cli('installation','verify','--prefix',prefix).returncode==0
    result=cli('install','--prefix',prefix)
    assert result.returncode!=0 and 'another native build' in result.stderr
    assert (prefix/'VERSION').read_text()=='previous-release'


def test_competing_installers_publish_one_complete_prefix(tmp_path):
    prefix=tmp_path/'native'
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        results=list(pool.map(lambda _:cli('install','--prefix',prefix),range(2)))
    assert any(result.returncode==0 for result in results)
    assert cli('installation','verify','--prefix',prefix).returncode==0
    assert not list(tmp_path.glob('.ccb-native-install-*'))


def test_installed_ask_alias_and_codex_completion_wake_lead_without_python_core(tmp_path):
    env=clean_env(tmp_path);prefix=tmp_path/'native';ccb=install(prefix,env=env)
    state=tmp_path/'state';report=tmp_path/'routing.jsonl'
    lead=dict(provider='test',command=[sys.executable,'-I',str(STUB),'lead',str(report)],profile_family='structured_result',env={'PATH':env['PATH']})
    data=dict(schema_version=2,project_id='native-install',agents={'lead':lead,'worker':codex_spec()})
    configuration=tmp_path/'config.json';configuration.write_text(json.dumps(data),encoding='utf-8')
    started=call(ccb,'--state-root',state,'start','--config',configuration,env=env)
    assert started.returncode==0,started.stderr
    pid=json.loads(started.stdout)['pid']
    try:
        asked=call(prefix/'bin/ask.exe','--state-root',state,'lead','Unicode 测试',env=env)
        assert asked.returncode==0,asked.stderr
        watched=call(ccb,'--state-root',state,'watch',json.loads(asked.stdout)['job_id'],'--wait','--timeout-ms','8000',env=env)
        assert watched.returncode==0,watched.stderr
        assert json.loads(watched.stdout)['reply']=='installed native Lead reviewed Codex result'
        rows=[json.loads(line) for line in report.read_text().splitlines()]
        assert len(rows)==2 and rows[-1]['kind']=='chain_continuation'
        for row in rows:
            assert Path(row['ask']).samefile(prefix/'bin/ask.exe')
            assert Path(row['ccb']).samefile(ccb)
            assert Path(row['prefix']).samefile(prefix)
        stdin=call(prefix/'bin/ask.exe','--state-root',state,'worker',input='Hello from redirected 中文 stdin',env=env)
        assert stdin.returncode==0,stdin.stderr
        bad=call(prefix/'bin/ask.exe','--state-root',env=env)
        assert bad.returncode!=0 and 'requires a value' in bad.stderr
    finally:
        stopped=call(ccb,'--state-root',state,'stop',env=env)
        assert stopped.returncode==0,stopped.stderr
        deadline=time.monotonic()+5
        while not _owner_stopped(pid) and time.monotonic()<deadline:time.sleep(.02)
        assert _owner_stopped(pid)


@pytest.mark.parametrize('provider',['pi','codex','claude'])
def test_installed_provider_commands_prefer_own_native_prefix(tmp_path,provider):
    env=clean_env(tmp_path);prefix=tmp_path/'native';ccb=install(prefix,env=env)
    worker={'pi':lambda:pi_spec('worker'),'codex':codex_spec,'claude':claude_spec}[provider]()
    report=tmp_path/'provider-env.json'
    worker['command']=[sys.executable,'-I',str(STUB),'environment',str(report),*worker['command']]
    worker['env']={'Path':env['PATH']}
    configuration=tmp_path/'config.json'
    configuration.write_text(json.dumps(dict(schema_version=2,project_id='env-test',agents={'worker':worker})))
    state=tmp_path/'state';started=call(ccb,'--state-root',state,'start','--config',configuration,env=env)
    assert started.returncode==0,started.stderr
    try:
        asked=call(prefix/'bin/ask.exe','--state-root',state,'worker','task',env=env)
        assert asked.returncode==0,asked.stderr
        watched=call(ccb,'--state-root',state,'watch',json.loads(asked.stdout)['job_id'],'--wait','--timeout-ms','8000',env=env)
        assert watched.returncode==0 and json.loads(watched.stdout)['status']=='completed',watched.stdout+watched.stderr
        evidence=json.loads(report.read_text())
        assert Path(evidence['PATH'].split(os.pathsep)[0]).samefile(prefix/'bin')
        assert evidence['CCB_NATIVE_CLI']=='1'
        assert Path(evidence['CCB_INSTALL_PREFIX']).samefile(prefix)
    finally:
        assert call(ccb,'--state-root',state,'stop',env=env).returncode==0


def test_powershell_rust_install_entry_bypasses_legacy_dependencies_and_account_writes(tmp_path):
    import shutil
    pwsh=shutil.which('pwsh')
    assert pwsh,'pwsh required for Windows installation contract'
    env=clean_env(tmp_path);prefix=tmp_path/'native'
    registry='[Environment]::GetEnvironmentVariable("Path","User")'
    before=call(pwsh,'-NoProfile','-Command',registry).stdout
    args=['-NoProfile','-File',ROOT/'install.ps1','install','-Runtime','rust','-InstallPrefix',prefix]
    dry=call(pwsh,*args,'-DryRun',env=env)
    assert dry.returncode==0 and not prefix.exists(),dry.stdout+dry.stderr
    result=call(pwsh,*args,env=env)
    assert result.returncode==0,result.stdout+result.stderr
    assert json.loads(result.stdout)['runtime']=='rust'
    assert not (prefix/'.venv').exists()
    assert call(pwsh,'-NoProfile','-Command',registry).stdout==before
    old=call(pwsh,'-NoProfile','-File',ROOT/'install.ps1','install','-InstallPrefix',prefix,env=env)
    assert old.returncode!=0 and 'Rust runtime' in old.stderr
    unsafe=call(pwsh,'-NoProfile','-File',ROOT/'install.ps1','install','-Runtime','rust',env=env)
    assert unsafe.returncode!=0 and 'explicit' in unsafe.stderr


@pytest.mark.parametrize('valid_digest',[True,False])
def test_powershell_archive_install_uses_verified_local_payload(tmp_path,valid_digest):
    import shutil,zipfile
    pwsh=shutil.which('pwsh');assert pwsh
    archive=tmp_path/'native.zip'
    with zipfile.ZipFile(archive,'w',compression=zipfile.ZIP_DEFLATED) as output:
        output.write(BIN,'payload/bin/ccb-native.exe')
        output.writestr('payload/bin/ccb.exe',b'MZfixture-launcher-not-used-in-rust-mode')
        output.writestr('payload/WINDOWS_MANIFEST.json','{}')
        output.writestr('payload/VERSION',(ROOT/'VERSION').read_bytes())
    digest=hashlib.sha256(archive.read_bytes()).hexdigest() if valid_digest else '0'*64
    prefix=tmp_path/'native'
    result=call(pwsh,'-NoProfile','-File',ROOT/'install.ps1','install','-Runtime','rust','-InstallPrefix',prefix,
                '-SourceArchive',archive,'-ExpectedSha256',digest,env=clean_env(tmp_path))
    if valid_digest:
        assert result.returncode==0,result.stdout+result.stderr
        assert cli('installation','verify','--prefix',prefix).returncode==0
    else:
        assert result.returncode!=0 and 'SHA256 mismatch' in result.stderr
        assert not prefix.exists()


def test_manifest_paths_cannot_escape_the_owned_inventory(tmp_path):
    prefix=tmp_path/'native';install(prefix)
    marker=prefix/'NATIVE_INSTALLATION.json';record=json.loads(marker.read_text())
    record['files']['../outside']=record['files'].pop('README-native.txt')
    marker.write_text(json.dumps(record),encoding='utf-8')
    result=cli('installation','verify','--prefix',prefix)
    assert result.returncode!=0 and 'inventory' in result.stderr


def test_native_prefix_junction_is_not_followed_or_replaced(tmp_path):
    import shutil
    prefix=tmp_path/'native';install(prefix)
    junction=tmp_path/'linked-prefix';pwsh=shutil.which('pwsh');assert pwsh
    quote=lambda p:"'"+str(p).replace("'","''")+"'"
    created=call(pwsh,'-NoProfile','-Command',f'New-Item -ItemType Junction -Path {quote(junction)} -Target {quote(prefix)} | Out-Null')
    assert created.returncode==0,created.stderr
    try:
        result=cli('install','--prefix',junction)
        assert result.returncode!=0 and any(word in result.stderr for word in ['reparse','linked native installation'])
        assert cli('installation','verify','--prefix',prefix).returncode==0
    finally:
        removed=call(pwsh,'-NoProfile','-Command',f'Remove-Item -LiteralPath {quote(junction)} -Force')
        assert removed.returncode==0,removed.stderr
