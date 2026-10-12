"""A stopped legacy daemon does not imply its supervisor relinquished ownership."""
import json
import os
import subprocess
import sys

import pytest
from test_native_cli_security import cli
from test_native_startup import configuration
from test_native_keeper import stop

pytestmark=pytest.mark.skipif(os.name!='nt',reason='Windows process observation')


def attempt(root, config):
    result=cli('--state-root',root,'start','--config',config)
    if result.returncode==0:
        stop(root)
    return result


@pytest.mark.parametrize('lease_present',[False,True])
@pytest.mark.parametrize('state',['running','failed','stopped'])
def test_live_legacy_keeper_fences_start_even_without_live_daemon(tmp_path,lease_present,state):
    root=tmp_path/'state';root.mkdir()
    keeper=dict(schema_version=2,record_type='ccbd_keeper',project_id='startup-test',keeper_pid=os.getpid(),state=state)
    marker=root/'keeper.json';marker.write_text(json.dumps(keeper),encoding='utf-8');before=marker.read_bytes()
    if lease_present:
        (root/'lease.json').write_text(json.dumps(dict(mount_state='unmounted',ccbd_pid=0)))
    result=attempt(root,configuration(tmp_path))
    assert result.returncode!=0 and 'legacy Keeper ownership is live or cannot be proven stopped' in result.stderr
    assert marker.read_bytes()==before
    assert not (root/'kernel/native-start.json').exists()
    assert not (root/'kernel/pending.json').exists()


@pytest.mark.parametrize('pid',[0,2**40])
def test_invalid_legacy_keeper_identity_does_not_authorize_handoff(tmp_path,pid):
    root=tmp_path/'state';root.mkdir()
    (root/'keeper.json').write_text(json.dumps(dict(schema_version=2,record_type='ccbd_keeper',keeper_pid=pid)))
    result=attempt(root,configuration(tmp_path))
    assert result.returncode!=0 and 'invalid legacy Keeper identity' in result.stderr
    assert not (root/'kernel/native-start.json').exists()


def test_proven_exited_legacy_keeper_does_not_block_native_start(tmp_path):
    root=tmp_path/'state';root.mkdir()
    child=subprocess.Popen([sys.executable,'-c','pass']);child.wait(timeout=10)
    marker=root/'keeper.json';marker.write_text(json.dumps(dict(schema_version=2,record_type='ccbd_keeper',keeper_pid=child.pid,state='running')))
    before=marker.read_bytes()
    result=attempt(root,configuration(tmp_path))
    assert result.returncode==0,result.stderr
    assert marker.read_bytes()==before
