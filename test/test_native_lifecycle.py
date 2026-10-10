"""Real Rust lease fencing; all records belong to temporary projects."""
import os
from pathlib import Path

import pytest

from ccbd.services.mount import MountManager
from ccbd.services.lifecycle import CcbdLifecycleStore,build_lifecycle
from ccbd.services.ownership import OwnershipGuard,OwnershipConflictError
from native_kernel import close_kernels
from storage.paths import PathLayout

ROOT=Path(__file__).resolve().parents[1]
BIN=ROOT/'rust'/'target'/'debug'/('ccb-kernel.exe' if os.name=='nt' else 'ccb-kernel')
NOW='2026-10-11T10:00:00Z'

@pytest.fixture
def manager(tmp_path,monkeypatch):
    monkeypatch.setenv('CCB_CORE_RUNTIME','rust')
    monkeypatch.setenv('CCB_KERNEL_BIN',str(BIN))
    close_kernels()
    layout=PathLayout(tmp_path/'repo')
    manager=MountManager(layout,clock=lambda:NOW,uid_getter=lambda:1000,boot_id_getter=lambda:'boot')
    yield layout,manager
    close_kernels()

def test_old_holder_cannot_release_or_change_new_lease(manager):
    layout,mount=manager
    lease=mount.mark_mounted(project_id='project',pid=202,socket_path=layout.ccbd_socket_path,generation=2,daemon_instance_id='new')
    for method,extra in [('heartbeat',{'interval_s':0}),('signature',{'expected_generation':2,'config_signature':'old'}),('release',{})]:
        with pytest.raises(ValueError,match='holder changed'):
            mount._native.call('daemon_lease.'+method,expected_pid=101,expected_daemon_instance_id='old',now=NOW,**extra)
    assert mount.load_state().to_record()==lease.to_record()

def test_ownership_probe_snapshot_cannot_override_changed_generation(manager):
    layout,mount=manager
    old=mount.mark_mounted(project_id='project',pid=101,socket_path=layout.ccbd_socket_path,generation=1,daemon_instance_id='old')
    mount.mark_mounted(project_id='project',pid=202,socket_path=layout.ccbd_socket_path,generation=2,daemon_instance_id='new')
    with pytest.raises(ValueError,match='changed during ownership inspection'):
        mount._native.call('daemon_ownership.expected_claim',lease=old.to_record(),project_id='project',pid=303,
                           socket_path=str(layout.ccbd_socket_path),daemon_instance_id='future',expected_generation=3,
                           inspection={'takeover_allowed':True})

def test_lifecycle_retains_schema_and_recovers_after_kernel_restart(manager):
    layout,mount=manager
    store=CcbdLifecycleStore(layout)
    state=build_lifecycle(project_id='project',occurred_at=NOW,desired_state='running',phase='starting',generation=3,
                          startup_id='startup',owner_pid=101,owner_daemon_instance_id='instance')
    store.save(state)
    assert store.load()==state
    close_kernels()
    assert CcbdLifecycleStore(layout).load()==state

def test_live_owner_rejects_expected_same_generation_claim(manager):
    layout,mount=manager
    mount.mark_mounted(project_id='project',pid=101,socket_path=layout.ccbd_socket_path,generation=3,daemon_instance_id='owner')
    guard=OwnershipGuard(layout,mount,clock=lambda:NOW,pid_exists=lambda _:True,socket_probe=lambda _:True)
    with pytest.raises(OwnershipConflictError,match='already held'):
        guard.assert_expected_claim_allowed(project_id='project',pid=202,socket_path=layout.ccbd_socket_path,
                                           daemon_instance_id='other',expected_generation=3)
