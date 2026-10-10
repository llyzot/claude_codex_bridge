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
import socket
import subprocess
import threading
from uuid import uuid4
from storage.locks import file_lock

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
    def __init__(self, root: Path, *, timeout: float = 10.0, transport=None) -> None:
        self._owner_pid = os.getpid()
        self.root = Path(root).resolve()
        self.timeout = timeout
        self._lock = threading.RLock()
        self._responses = queue.Queue()
        self._sequence = 0
        self._failure = None
        self._owns_process=True
        self.transport=transport or os.environ.get('CCB_KERNEL_TRANSPORT') or 'stdio'
        if self.transport not in {'stdio','tcp'}:
            raise ValueError('CCB_KERNEL_TRANSPORT must be stdio or tcp')
        command=[str(kernel_binary()), '--state-root', str(self.root)]
        self._token_path=None
        if self.transport=='tcp':
            from ccbd.control_plane_transport.token_auth import create_token_file
            self._token_path=self.root/'kernel'/f'transport-token-{uuid4().hex}.json'
            self._token=create_token_file(self._token_path)
            command += ['--token-file',str(self._token_path)]
        self.process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, encoding='utf-8', **background_process_kwargs(),
        )
        self._reader = threading.Thread(target=self._read, name='ccb-kernel-reader', daemon=True)
        self._reader.start()
        try:
            if self.transport=='tcp':
                ready=self._responses.get(timeout=self.timeout)
                if isinstance(ready,BaseException):
                    raise ready
                if ready.get('host')!='127.0.0.1' or ready.get('generation')!=self._token.generation or ready.get('pid')!=self.process.pid:
                    raise KernelUnavailable('Rust control-plane readiness identity mismatch')
                self._tcp_address=(ready['host'],int(ready['port']))
            info = self.call('ping')
            if info.get('service') != 'ccb-kernel' or info.get('api_version') != 1:
                raise KernelUnavailable('incompatible Rust kernel')
            self.capabilities = frozenset(info.get('capabilities') or ())
            required = {'mailbox','scheduler','records','bureau','callback','completion','control_plane','lifecycle','admission'}
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
            envelope={'api_version': 1, 'id': request_id, 'op': op, 'args': args}
            if self.transport=='tcp':
                envelope['generation']=self._token.generation
            request = json.dumps(envelope, ensure_ascii=False, allow_nan=False) + '\n'
            self._sequence = request_id
            try:
                if self.transport=='tcp':
                    response=self._tcp_exchange(request)
                else:
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

    def _tcp_exchange(self, request):
        from ccbd.control_plane_transport.token_auth import client_authenticate
        with socket.create_connection(self._tcp_address,timeout=self.timeout) as stream:
            stream.setsockopt(socket.IPPROTO_TCP,socket.TCP_NODELAY,1)
            client_authenticate(stream,self._token.token)
            stream.sendall(request.encode('utf-8'))
            with stream.makefile('rb') as reader:
                raw=reader.readline(16*1024*1024+1)
            if not raw.endswith(b'\n') or len(raw)>16*1024*1024:
                raise KernelUnavailable('Rust control-plane response is incomplete or too large')
            return json.loads(raw)

    @classmethod
    def attach(cls, root, descriptor, *, timeout=10.0):
        from ccbd.control_plane_transport.token_auth import load_token_file
        client=cls.__new__(cls)
        client.root=Path(root).resolve()
        client._owner_pid=os.getpid()
        client._owns_process=False
        client.timeout=timeout
        client._lock=threading.RLock()
        client._sequence=0
        client._failure=None
        client.transport='tcp'
        client._token_path=None
        token_path=Path(descriptor['token_ref'])
        if token_path.parent.resolve()!=client.root/'kernel' or descriptor.get('host')!='127.0.0.1':
            raise KernelUnavailable('Invalid native control-plane descriptor')
        client._token=load_token_file(token_path)
        if client._token.generation!=descriptor.get('generation'):
            raise KernelUnavailable('Native control-plane token generation changed')
        client._tcp_address=('127.0.0.1',int(descriptor['port']))
        info=client.call('ping')
        if info.get('pid')!=descriptor.get('pid') or info.get('service')!='ccb-kernel':
            raise KernelUnavailable('Native control-plane owner identity mismatch')
        client.capabilities=frozenset(info.get('capabilities') or ())
        if not {'mailbox','scheduler','records','bureau','callback','completion','control_plane','lifecycle','admission'} <= client.capabilities:
            raise KernelUnavailable('Attached Rust kernel lacks required core capabilities')
        return client

    def close(self):
        if os.getpid() != self._owner_pid:
            return  # Never terminate a parent's kernel from a forked child.
        with self._lock:
            self._failure = self._failure or 'Rust kernel client closed'
            if not self._owns_process:
                return
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
            if self._token_path is not None:
                # Only the exact token created by this client is eligible.
                if self._token_path.parent.resolve()!=self.root/'kernel':
                    raise KernelUnavailable('Kernel token cleanup path changed')
                try:
                    self._token_path.unlink()
                except (FileNotFoundError,PermissionError):
                    pass


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
            with file_lock(layout.ccbd_dir/'kernel'/'launch.lock'):
                descriptor_path=layout.ccbd_dir/'kernel'/'control-plane.json'
                if descriptor_path.is_file():
                    descriptor=json.loads(descriptor_path.read_text(encoding='utf-8'))
                    try:
                        _clients[key]=KernelClient.attach(layout.ccbd_dir,descriptor)
                    except Exception:
                        pid=descriptor.get('pid')
                        if not isinstance(pid,int) or pid<=0 or not _owner_stopped(pid):
                            raise
                        current=json.loads(descriptor_path.read_text(encoding='utf-8'))
                        if current.get('generation')!=descriptor.get('generation'):
                            raise KernelUnavailable('Native endpoint generation changed during recovery')
                        descriptor_path.unlink()
                if key not in _clients:
                    _clients[key] = KernelClient(layout.ccbd_dir,transport='tcp')
        return _clients[key]


def close_kernels():
    with _clients_lock:
        clients = list(_clients.values())
        _clients.clear()
    for client in clients:
        client.close()


atexit.register(close_kernels)


def _owner_stopped(pid):
    if os.name!='nt':
        try:
            os.kill(pid,0)
        except ProcessLookupError:
            return True
        except (PermissionError,OSError):
            return False
        return False
    import ctypes
    from ctypes import wintypes
    kernel=ctypes.WinDLL('kernel32',use_last_error=True)
    kernel.OpenProcess.argtypes=[wintypes.DWORD,wintypes.BOOL,wintypes.DWORD]
    kernel.OpenProcess.restype=wintypes.HANDLE
    kernel.WaitForSingleObject.argtypes=[wintypes.HANDLE,wintypes.DWORD]
    kernel.WaitForSingleObject.restype=wintypes.DWORD
    kernel.CloseHandle.argtypes=[wintypes.HANDLE]
    handle=kernel.OpenProcess(0x00100000,False,pid)
    if not handle:
        return ctypes.get_last_error()==87  # Invalid PID, not access denied.
    try:
        return kernel.WaitForSingleObject(handle,0)==0
    finally:
        kernel.CloseHandle(handle)
