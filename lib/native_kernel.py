"""Long-lived Rust kernel transport. Failed writes never fall back or replay."""
from __future__ import annotations

import atexit
from contextlib import contextmanager
from functools import wraps
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import threading

from process_background import background_process_kwargs


class KernelUnavailable(RuntimeError):
    pass


def rust_core_enabled() -> bool:
    mode = str(os.environ.get('CCB_CORE_RUNTIME') or 'python').strip().lower()
    if mode not in {'python', 'rust'}:
        raise ValueError('CCB_CORE_RUNTIME must be python or rust')
    return mode == 'rust'


def kernel_binary() -> Path:
    explicit = os.environ.get('CCB_KERNEL_BIN')
    if explicit:
        path = Path(explicit).expanduser().resolve()
        if not path.is_file():
            raise KernelUnavailable('CCB_KERNEL_BIN does not identify a kernel binary')
        return path
    root = Path(__file__).resolve().parents[1]
    name = 'ccb-kernel.exe' if os.name == 'nt' else 'ccb-kernel'
    for path in (root / 'bin' / name, root / 'rust' / 'target' / 'release' / name):
        if path.is_file():
            return path
    found = shutil.which(name)
    if found:
        return Path(found)
    raise KernelUnavailable('Rust core selected but ccb-kernel is not built; build rust/crates/ccb-kernel')


class KernelClient:
    def __init__(self, root: Path, *, timeout: float = 10.0) -> None:
        self._owner_pid = os.getpid()
        self.root = Path(root).resolve()
        self.timeout = timeout
        self._lock = threading.RLock()
        self._responses = queue.Queue()
        self._sequence = 0
        self._failure = None
        self.process = subprocess.Popen(
            [str(kernel_binary()), '--state-root', str(self.root)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, encoding='utf-8', **background_process_kwargs(),
        )
        self._reader = threading.Thread(target=self._read, name='ccb-kernel-reader', daemon=True)
        self._reader.start()
        try:
            info = self.call('ping')
            if info.get('service') != 'ccb-kernel' or info.get('api_version') != 1:
                raise KernelUnavailable('incompatible Rust kernel')
            self.capabilities = frozenset(info.get('capabilities') or ())
            required = {'mailbox','scheduler','records','bureau','callback','completion'}
            if not required <= self.capabilities:
                raise KernelUnavailable('Rust kernel lacks required core capabilities; rebuild ccb-kernel')
        except Exception:
            self.close()
            raise

    def _read(self):
        try:
            for line in self.process.stdout:
                self._responses.put(json.loads(line))
        except Exception as error:
            self._responses.put(error)
        finally:
            self._responses.put(KernelUnavailable('Rust kernel disconnected; outcome may be unknown'))

    def call(self, op: str, **args):
        if os.getpid() != self._owner_pid:
            raise KernelUnavailable('Rust kernel connection belongs to another process')
        with self._lock:
            if self._failure:
                raise KernelUnavailable(self._failure)
            request_id = self._sequence + 1
            # Reject invalid local data before a write can have an unknown
            # outcome. Python adapter handles never belong on this wire.
            request = json.dumps({'api_version': 1, 'id': request_id, 'op': op, 'args': args}, ensure_ascii=False, allow_nan=False) + '\n'
            self._sequence = request_id
            try:
                self.process.stdin.write(request)
                self.process.stdin.flush()
                response = self._responses.get(timeout=self.timeout)
                if isinstance(response, BaseException):
                    raise response
                if response.get('id') != request_id or response.get('api_version') != 1:
                    raise KernelUnavailable('Rust kernel response identity mismatch')
            except Exception as error:
                self._failure = f'Rust kernel unavailable; request {request_id} was not replayed: {type(error).__name__}'
                self.close()
                raise KernelUnavailable(self._failure) from error
            if not response.get('ok'):
                raise ValueError(response.get('error') or 'Rust kernel rejected request')
            return response.get('result')

    def close(self):
        if os.getpid() != self._owner_pid:
            return  # Never terminate a parent's kernel from a forked child.
        with self._lock:
            self._failure = self._failure or 'Rust kernel client closed'
            try:
                self.process.stdin.close()
            except (OSError, ValueError):
                pass
            try:
                self.process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                try:
                    self.process.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=1)
            self._reader.join(timeout=1)
            self.process.stdout.close()


_clients = {}
_clients_lock = threading.RLock()
_legacy_local = threading.local()
_legacy_lock = threading.RLock()


@contextmanager
def legacy_writer(layout):
    """Use the same OS lock as Rust for transitional Python mailbox writes."""
    path = layout.ccbd_dir / 'kernel' / 'writer.lock'
    key = str(path.resolve())
    with _legacy_lock:
        held = getattr(_legacy_local, 'held', set())
        if key in held:
            yield
            return
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open('a+b') as handle:
            handle.seek(0)
            if os.name == 'nt':
                import msvcrt
                acquire = lambda: msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
                release = lambda: msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                import fcntl
                acquire = lambda: fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                release = lambda: fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
            try:
                acquire()
            except OSError as error:
                raise KernelUnavailable('Mailbox state has an active writer; Python write rejected') from error
            held.add(key)
            _legacy_local.held = held
            try:
                yield
            finally:
                held.remove(key)
                release()


def guard_legacy_writes(cls, *names):
    for name in names:
        original = getattr(cls, name)
        def wrap(method):
            @wraps(method)
            def call(self, *args, **kwargs):
                if self._native is not None:
                    return method(self, *args, **kwargs)
                with legacy_writer(self._layout):
                    return method(self, *args, **kwargs)
            return call
        setattr(cls, name, wrap(original))


def kernel_for_layout(layout):
    if not rust_core_enabled():
        return None
    key = (os.getpid(), str(layout.ccbd_dir.resolve()))
    with _clients_lock:
        if key not in _clients:
            _clients[key] = KernelClient(layout.ccbd_dir)
        return _clients[key]


def close_kernels():
    with _clients_lock:
        clients = list(_clients.values())
        _clients.clear()
    for client in clients:
        client.close()


atexit.register(close_kernels)
