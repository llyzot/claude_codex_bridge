from __future__ import annotations

from contextlib import contextmanager
from pathlib import Path

from ccbd.control_plane_transport.endpoint import endpoint_from_legacy_socket_path
from ccbd.control_plane_transport.factory import endpoint_connectable
from ccbd.models import CcbdLease, LeaseHealth, LeaseInspection, MountState
from ccbd.system import parse_utc_timestamp, process_exists, utc_now
from storage.locks import file_lock
from storage.paths import PathLayout


class OwnershipConflictError(RuntimeError):
    pass


class OwnershipGuard:
    def __init__(
        self,
        layout: PathLayout,
        mount_manager,
        *,
        clock=utc_now,
        pid_exists=process_exists,
        socket_probe=None,
        heartbeat_grace_seconds: float = 15.0,
    ) -> None:
        self._layout = layout
        self._mount_manager = mount_manager
        self._clock = clock
        self._pid_exists = pid_exists
        self._socket_probe = socket_probe or endpoint_connectable
        self._socket_probe_uses_endpoint = socket_probe is None
        self._heartbeat_grace_seconds = heartbeat_grace_seconds

    @contextmanager
    def startup_lock(self):
        lock_path = self._layout.ccbd_dir / 'startup.lock'
        with file_lock(lock_path):
            yield

    def inspect(
        self,
        lease: CcbdLease | None = None,
        *,
        assume_mounted_socket_connectable: bool = False,
    ) -> LeaseInspection:
        current = lease if lease is not None else self._mount_manager.load_state()
        native=getattr(self._mount_manager,'_native',None)
        if native is not None:
            alive=False if current is None else self._pid_exists(current.ccbd_pid)
            socket=False if current is None else (True if assume_mounted_socket_connectable and current.mount_state is MountState.MOUNTED else self._mounted_socket_connectable(current))
            payload=native.call('daemon_ownership.inspect',lease=None if current is None else current.to_record(),
                                pid_alive=alive,socket_connectable=socket,now=self._clock(),heartbeat_grace_seconds=self._heartbeat_grace_seconds)
            return LeaseInspection(lease=current,health=LeaseHealth(payload['health']),pid_alive=payload['pid_alive'],
                                   socket_connectable=payload['socket_connectable'],heartbeat_fresh=payload['heartbeat_fresh'],
                                   takeover_allowed=payload['takeover_allowed'],reason=payload['reason'])
        if current is None:
            return self._missing_inspection()

        pid_alive, heartbeat_fresh, socket_connectable = self._lease_signals(
            current,
            assume_mounted_socket_connectable=assume_mounted_socket_connectable,
        )
        if current.mount_state is MountState.UNMOUNTED:
            return self._inspection(
                current,
                health=LeaseHealth.UNMOUNTED,
                pid_alive=pid_alive,
                socket_connectable=socket_connectable,
                heartbeat_fresh=heartbeat_fresh,
                takeover_allowed=True,
                reason='lease_unmounted',
            )
        if pid_alive and heartbeat_fresh and socket_connectable:
            return self._inspection(
                current,
                health=LeaseHealth.HEALTHY,
                pid_alive=True,
                socket_connectable=True,
                heartbeat_fresh=True,
                takeover_allowed=False,
                reason='healthy',
            )

        takeover_allowed = self._takeover_allowed(
            pid_alive=pid_alive,
            heartbeat_fresh=heartbeat_fresh,
            socket_connectable=socket_connectable,
        )
        health = LeaseHealth.STALE if takeover_allowed else LeaseHealth.DEGRADED
        return self._inspection(
            current,
            health=health,
            pid_alive=pid_alive,
            socket_connectable=socket_connectable,
            heartbeat_fresh=heartbeat_fresh,
            takeover_allowed=takeover_allowed,
            reason=self._inspection_reason(
                health=health,
                pid_alive=pid_alive,
                heartbeat_fresh=heartbeat_fresh,
                socket_connectable=socket_connectable,
            ),
        )

    def verify_or_takeover(self, *, project_id: str, pid: int, socket_path: str | Path) -> int:
        current = self._mount_manager.load_state()
        native=getattr(self._mount_manager,'_native',None)
        if native is not None:
            return self._native_claim(native,'verify_claim',current,project_id=project_id,pid=pid,socket_path=socket_path)
        if current is None:
            return 1
        if current.project_id != project_id and current.mount_state is MountState.UNMOUNTED:
            return current.generation + 1
        self._assert_project_id(current, project_id=project_id)
        if self._same_holder(current, pid=pid, socket_path=socket_path):
            return current.generation
        inspection = self.inspect(current)
        if inspection.takeover_allowed:
            return current.generation + 1
        raise OwnershipConflictError(
            f'ccbd lease is held by pid={current.ccbd_pid} generation={current.generation}: {inspection.reason}'
        )

    def assert_expected_claim_allowed(
        self,
        *,
        project_id: str,
        pid: int,
        socket_path: str | Path,
        daemon_instance_id: str,
        expected_generation: int,
    ) -> None:
        native=getattr(self._mount_manager,'_native',None)
        if native is not None:
            self._native_claim(native,'expected_claim',self._mount_manager.load_state(),project_id=project_id,pid=pid,
                               socket_path=socket_path,daemon_instance_id=daemon_instance_id,expected_generation=expected_generation)
            return
        generation = int(expected_generation)
        if generation <= 0:
            raise OwnershipConflictError('expected ccbd generation must be positive')
        current = self._mount_manager.load_state()
        if current is None:
            return
        current_generation = int(current.generation)
        same_instance = (
            current.project_id == project_id
            and current_generation == generation
            and self._same_holder(current, pid=pid, socket_path=socket_path)
            and str(current.daemon_instance_id or '') == str(daemon_instance_id or '')
        )
        if same_instance:
            return
        if current.project_id != project_id:
            if current.mount_state is MountState.UNMOUNTED and current_generation < generation:
                return
            raise OwnershipConflictError(
                'ccbd lease project_id mismatch for expected claim: '
                f'expected {project_id}, found {current.project_id}'
            )
        if current_generation >= generation:
            raise OwnershipConflictError(
                'ccbd expected generation is already held or superseded: '
                f'expected {generation}, found {current_generation}'
            )
        inspection = self.inspect(current)
        if inspection.takeover_allowed:
            return
        raise OwnershipConflictError(
            'ccbd predecessor lease does not allow expected claim: '
            f'pid={current.ccbd_pid} generation={current_generation}: {inspection.reason}'
        )

    def _heartbeat_is_fresh(self, lease: CcbdLease) -> bool:
        try:
            current = parse_utc_timestamp(self._clock())
            heartbeat = parse_utc_timestamp(lease.last_heartbeat_at)
        except Exception:
            return False
        delta = (current - heartbeat).total_seconds()
        return delta <= self._heartbeat_grace_seconds

    def _missing_inspection(self) -> LeaseInspection:
        return LeaseInspection(
            lease=None,
            health=LeaseHealth.MISSING,
            pid_alive=False,
            socket_connectable=False,
            heartbeat_fresh=False,
            takeover_allowed=True,
            reason='lease_missing',
        )

    def _lease_signals(
        self,
        lease: CcbdLease,
        *,
        assume_mounted_socket_connectable: bool = False,
    ) -> tuple[bool, bool, bool]:
        pid_alive = self._pid_exists(lease.ccbd_pid)
        heartbeat_fresh = self._heartbeat_is_fresh(lease)
        socket_connectable = (
            True
            if assume_mounted_socket_connectable and lease.mount_state is MountState.MOUNTED
            else self._mounted_socket_connectable(lease)
        )
        return pid_alive, heartbeat_fresh, socket_connectable

    def _mounted_socket_connectable(self, lease: CcbdLease) -> bool:
        if lease.mount_state is not MountState.MOUNTED:
            return False
        if self._socket_probe_uses_endpoint:
            endpoint = lease.control_plane_endpoint or endpoint_from_legacy_socket_path(lease.socket_path)
            return self._socket_probe(endpoint)
        return self._socket_probe(lease.socket_path)

    def _takeover_allowed(
        self,
        *,
        pid_alive: bool,
        heartbeat_fresh: bool,
        socket_connectable: bool,
    ) -> bool:
        return (not pid_alive) or (pid_alive and not heartbeat_fresh and not socket_connectable)

    def _inspection_reason(
        self,
        *,
        health: LeaseHealth,
        pid_alive: bool,
        heartbeat_fresh: bool,
        socket_connectable: bool,
    ) -> str:
        reason_parts: list[str] = []
        if not pid_alive:
            reason_parts.append('pid_missing')
        if not heartbeat_fresh:
            reason_parts.append('heartbeat_stale')
        if not socket_connectable:
            reason_parts.append('socket_unreachable')
        return ','.join(reason_parts) or health.value

    def _inspection(
        self,
        lease: CcbdLease | None,
        *,
        health: LeaseHealth,
        pid_alive: bool,
        socket_connectable: bool,
        heartbeat_fresh: bool,
        takeover_allowed: bool,
        reason: str,
    ) -> LeaseInspection:
        return LeaseInspection(
            lease=lease,
            health=health,
            pid_alive=pid_alive,
            socket_connectable=socket_connectable,
            heartbeat_fresh=heartbeat_fresh,
            takeover_allowed=takeover_allowed,
            reason=reason,
        )

    def _assert_project_id(self, lease: CcbdLease, *, project_id: str) -> None:
        if lease.project_id == project_id:
            return
        raise OwnershipConflictError(
            f'lease project_id mismatch: expected {project_id}, found {lease.project_id}'
        )

    def _same_holder(self, lease: CcbdLease, *, pid: int, socket_path: str | Path) -> bool:
        current_socket = str(Path(lease.socket_path))
        desired_socket = str(Path(socket_path))
        return lease.ccbd_pid == pid and current_socket == desired_socket

    def _native_claim(self,native,method,current,**args):
        inspection=self.inspect(current)
        args['socket_path']=str(Path(args['socket_path']))
        try:
            return native.call('daemon_ownership.'+method,lease=None if current is None else current.to_record(),
                               inspection={'takeover_allowed':inspection.takeover_allowed,'reason':inspection.reason},**args)
        except ValueError as error:
            raise OwnershipConflictError(str(error)) from error


__all__ = ['OwnershipConflictError', 'OwnershipGuard']
