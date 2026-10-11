"""Installation lock recovery in disposable prefixes, including live old writers."""
import json
import os
from pathlib import Path
import subprocess
import sys

import pytest
from test_native_cli_security import cli

pytestmark=pytest.mark.skipif(os.name!='nt',reason='native prefix installation targets Windows')


def lock_path(prefix):return prefix.parent/f'.{prefix.name}.ccb-native-install.lock'


def test_closed_legacy_creation_marker_does_not_permanently_block_installation(tmp_path):
    prefix=tmp_path/'native';lock=lock_path(prefix);lock.touch()
    before=lock.stat().st_ino
    result=cli('install','--prefix',prefix)
    assert result.returncode==0,result.stderr
    assert json.loads(result.stdout)['status']=='installed'
    assert lock.exists() and lock.stat().st_size==0 and lock.stat().st_ino==before
    assert cli('installation','verify','--prefix',prefix).returncode==0


@pytest.mark.parametrize('byte_lock',[False,True])
def test_live_writer_is_not_overridden_and_process_exit_releases_the_lock(tmp_path,byte_lock):
    prefix=tmp_path/'native';lock=lock_path(prefix)
    code=("import sys,time,msvcrt\n"
          "file=open(sys.argv[1],'a+b')\n"
          "if sys.argv[2]=='1': msvcrt.locking(file.fileno(),msvcrt.LK_NBLCK,1)\n"
          "print('ready',flush=True)\n"
          "time.sleep(30)\n")
    owner=subprocess.Popen([sys.executable,'-I','-c',code,str(lock),'1' if byte_lock else '0'],
                           stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                           text=True,encoding='utf-8',creationflags=0x08000000)
    try:
        assert owner.stdout.readline().strip()=='ready'
        before=lock.stat().st_ino
        blocked=cli('install','--prefix',prefix)
        assert blocked.returncode!=0 and 'lock' in blocked.stderr
        assert owner.poll() is None and not prefix.exists()
        assert lock.stat().st_size==0 and lock.stat().st_ino==before
        owner.kill();owner.wait(timeout=3)
        recovered=cli('install','--prefix',prefix)
        assert recovered.returncode==0,recovered.stderr
        assert lock.stat().st_ino==before
        assert cli('installation','verify','--prefix',prefix).returncode==0
    finally:
        if owner.poll() is None:owner.kill();owner.wait(timeout=3)


def test_nonempty_lock_is_preserved_instead_of_truncated_or_deleted(tmp_path):
    prefix=tmp_path/'native';lock=lock_path(prefix);lock.write_bytes(b'unrelated user file')
    result=cli('install','--prefix',prefix)
    assert result.returncode!=0 and 'empty native lock' in result.stderr
    assert lock.read_bytes()==b'unrelated user file' and not prefix.exists()


def test_orphan_stage_is_not_adopted_or_deleted_from_a_directory_name_alone(tmp_path):
    prefix=tmp_path/'native';lock_path(prefix).touch()
    stage=tmp_path/'.ccb-native-install-orphan';stage.mkdir();(stage/'keep.txt').write_text('preserve')
    result=cli('install','--prefix',prefix)
    assert result.returncode==0,result.stderr
    assert (stage/'keep.txt').read_text()=='preserve'
    assert cli('installation','verify','--prefix',prefix).returncode==0
