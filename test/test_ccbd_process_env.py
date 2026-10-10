from __future__ import annotations

import os
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace

import pytest

import ccbd.daemon_process as daemon_process
import process_background
from ccbd.daemon_process import _ccbd_env, _prepend_tool_paths, _ready_payload_matches_expected
from ccbd.startup_fence import (
    EXPECTED_GENERATION_ENV,
    EXPECTED_STARTUP_ID_ENV,
    KEEPER_STARTUP_ACCEPTED_PERF_COUNTER_NS_ENV,
)


def _mock_process_background_os(monkeypatch, *, name: str) -> None:
    """Change the module-under-test platform without mutating global ``os``."""
    os_proxy = SimpleNamespace(**vars(process_background.os))
    os_proxy.name = name
    monkeypatch.setattr(process_background, 'os', os_proxy)


def test_ccbd_env_prefers_current_worktree_tools(monkeypatch) -> None:
    monkeypatch.setenv('PATH', os.pathsep.join(['/usr/bin', '/bin']))
    monkeypatch.setenv('PYTHONPATH', '/stable/ccb/lib:/other')

    env = _ccbd_env(keeper_pid=123)
    script_root = Path(__file__).resolve().parents[1]
    lib_root = script_root / 'lib'
    parts = env['PATH'].split(os.pathsep)

    assert parts[:2] == [str(script_root / 'bin'), str(script_root)]
    assert env['PYTHONPATH'] == str(lib_root)
    assert env['CCB_KEEPER_PID'] == '123'


def test_ccbd_env_injects_exact_startup_fence_and_drops_inherited_values(monkeypatch) -> None:
    monkeypatch.setenv(EXPECTED_STARTUP_ID_ENV, 'b' * 32)
    monkeypatch.setenv(EXPECTED_GENERATION_ENV, '99')

    env = _ccbd_env(
        keeper_pid=123,
        expected_startup_id='a' * 32,
        expected_generation=7,
        keeper_startup_accepted_perf_counter_ns=1234567,
    )

    assert env[EXPECTED_STARTUP_ID_ENV] == 'a' * 32
    assert env[EXPECTED_GENERATION_ENV] == '7'
    assert env[KEEPER_STARTUP_ACCEPTED_PERF_COUNTER_NS_ENV] == '1234567'


def test_ccbd_env_drops_inherited_or_malformed_diagnostics_checkpoint(monkeypatch) -> None:
    monkeypatch.setenv(KEEPER_STARTUP_ACCEPTED_PERF_COUNTER_NS_ENV, '999')

    legacy_env = _ccbd_env(keeper_pid=123)
    malformed_env = _ccbd_env(
        keeper_pid=123,
        expected_startup_id='a' * 32,
        expected_generation=7,
        keeper_startup_accepted_perf_counter_ns=-1,
    )

    assert KEEPER_STARTUP_ACCEPTED_PERF_COUNTER_NS_ENV not in legacy_env
    assert KEEPER_STARTUP_ACCEPTED_PERF_COUNTER_NS_ENV not in malformed_env


def test_ccbd_env_rejects_partial_startup_fence() -> None:
    try:
        _ccbd_env(keeper_pid=123, expected_startup_id='a' * 32)
    except ValueError as exc:
        assert 'requires both' in str(exc)
    else:
        raise AssertionError('partial startup fence should fail')


def test_ready_payload_identity_does_not_require_serving_pid_equals_popen_pid() -> None:
    payload = {
        'generation': 7,
        'mount_state': 'mounted',
        'desired_state': 'running',
        'serving_pid': 4321,
        'serving_daemon_instance_id': 'daemon-1',
        'serving_lease_generation': 7,
        'accepted_startup_id': 'a' * 32,
        'diagnostics': {
            'startup_id': 'a' * 32,
            'startup_stage': 'mounted',
        },
    }

    # On Windows the venvlauncher redirector makes Popen.pid differ from the
    # daemon's own pid; the fence must accept any positive serving pid as long
    # as startup_id/generation identity matches.
    assert _ready_payload_matches_expected(
        payload,
        expected_startup_id='a' * 32,
        expected_generation=7,
    )
    assert _ready_payload_matches_expected(
        {**payload, 'serving_pid': 9999},
        expected_startup_id='a' * 32,
        expected_generation=7,
    )
    assert not _ready_payload_matches_expected(
        {**payload, 'serving_pid': 0},
        expected_startup_id='a' * 32,
        expected_generation=7,
    )
    assert not _ready_payload_matches_expected(
        {**payload, 'serving_daemon_instance_id': ''},
        expected_startup_id='a' * 32,
        expected_generation=7,
    )
    assert not _ready_payload_matches_expected(
        {**payload, 'serving_lease_generation': 8},
        expected_startup_id='a' * 32,
        expected_generation=7,
    )
    assert not _ready_payload_matches_expected(
        {**payload, 'accepted_startup_id': 'b' * 32},
        expected_startup_id='a' * 32,
        expected_generation=7,
    )
    assert not _ready_payload_matches_expected(
        {
            **payload,
            'generation': 8,
            'mount_state': 'starting',
            'diagnostics': {
                'startup_id': 'b' * 32,
                'startup_stage': 'spawn_requested',
            },
        },
        expected_startup_id='a' * 32,
        expected_generation=7,
    )


def test_prepend_tool_paths_deduplicates_existing_entries(tmp_path: Path) -> None:
    root = tmp_path / 'repo'
    (root / 'bin').mkdir(parents=True)
    env = {'PATH': os.pathsep.join([str(root), '/usr/bin', str(root / 'bin')])}

    _prepend_tool_paths(env, root)

    assert env['PATH'].split(os.pathsep) == [str(root / 'bin'), str(root), '/usr/bin']


def test_background_process_kwargs_detaches_windows_console(monkeypatch) -> None:
    _mock_process_background_os(monkeypatch, name='nt')
    monkeypatch.setattr(process_background.subprocess, 'CREATE_NEW_PROCESS_GROUP', 0x00000200, raising=False)
    monkeypatch.setattr(process_background.subprocess, 'DETACHED_PROCESS', 0x00000008, raising=False)
    monkeypatch.setattr(process_background.subprocess, 'CREATE_NO_WINDOW', 0x08000000, raising=False)

    kwargs = process_background.background_process_kwargs()

    assert kwargs['start_new_session'] is True
    assert kwargs['creationflags'] & 0x00000200
    assert kwargs['creationflags'] & 0x00000008
    assert kwargs['creationflags'] & 0x08000000


def test_background_spawn_off_windows_returns_sys_executable(monkeypatch) -> None:
    _mock_process_background_os(monkeypatch, name='posix')

    interpreter, extra = process_background.background_spawn()

    assert interpreter == sys.executable
    assert extra == {}


def test_background_spawn_resolves_venv_base_interpreter_and_site_packages(
    tmp_path: Path, monkeypatch
) -> None:
    _mock_process_background_os(monkeypatch, name='nt')
    venv = tmp_path / 'venv'
    (venv / 'Scripts').mkdir(parents=True)
    site_packages = venv / 'Lib' / 'site-packages'
    site_packages.mkdir(parents=True)
    base_exe = tmp_path / 'base' / 'python.exe'
    base_exe.parent.mkdir(parents=True)
    base_exe.write_bytes(b'')
    (venv / 'pyvenv.cfg').write_text(
        f'home = {base_exe.parent}\nversion = 3.14\n', encoding='utf-8'
    )
    monkeypatch.setattr(
        process_background.sys, 'executable', str(venv / 'Scripts' / 'python.exe')
    )

    interpreter, extra = process_background.background_spawn()

    assert interpreter == str(base_exe)
    assert extra['PYTHONPATH'] == str(site_packages)
    assert extra['VIRTUAL_ENV'] == str(venv)


def test_venv_base_interpreter_none_without_pyvenv_cfg(tmp_path: Path, monkeypatch) -> None:
    _mock_process_background_os(monkeypatch, name='nt')
    fake_exe = tmp_path / 'Scripts' / 'python.exe'
    fake_exe.parent.mkdir(parents=True)
    monkeypatch.setattr(process_background.sys, 'executable', str(fake_exe))

    assert process_background.venv_base_interpreter() is None


def test_windows_keeper_preserves_venv_dependencies_for_daemon(tmp_path: Path, monkeypatch) -> None:
    _mock_process_background_os(monkeypatch, name='nt')
    venv = tmp_path / 'venv'
    (venv / 'Scripts').mkdir(parents=True)
    site_packages = venv / 'Lib' / 'site-packages'
    site_packages.mkdir(parents=True)
    base_exe = tmp_path / 'base' / 'python.exe'
    base_exe.parent.mkdir()
    base_exe.write_bytes(b'')
    (venv / 'pyvenv.cfg').write_text(f'home = {base_exe.parent}\n', encoding='utf-8')
    monkeypatch.setattr(process_background.sys, 'executable', str(venv / 'Scripts' / 'python.exe'))
    keeper_interpreter, keeper_extra = process_background.background_spawn()

    # The keeper runs the base executable, with the venv identity retained.
    monkeypatch.setattr(process_background.sys, 'executable', keeper_interpreter)
    monkeypatch.setenv('VIRTUAL_ENV', keeper_extra['VIRTUAL_ENV'])
    monkeypatch.setenv('PYTHONPATH', os.pathsep.join([str(site_packages), '/unrelated/inherited']))
    daemon_interpreter, daemon_extra = process_background.background_spawn()
    env = _ccbd_env(keeper_pid=123, venv_env=daemon_extra)

    assert daemon_interpreter == str(base_exe)
    assert env['PYTHONPATH'].split(os.pathsep) == [
        str(Path(__file__).resolve().parents[1] / 'lib'), str(site_packages),
    ]
    assert daemon_extra['VIRTUAL_ENV'] == str(venv)


@pytest.mark.parametrize('valid_config', [False, True])
def test_windows_background_spawn_ignores_unrelated_inherited_venv(
    tmp_path: Path, monkeypatch, valid_config: bool,
) -> None:
    _mock_process_background_os(monkeypatch, name='nt')
    inherited = tmp_path / 'unrelated'
    (inherited / 'Lib' / 'site-packages').mkdir(parents=True)
    other_base = tmp_path / 'other-base'
    other_base.mkdir()
    (other_base / 'python.exe').write_bytes(b'')
    if valid_config:
        (inherited / 'pyvenv.cfg').write_text(f'home = {other_base}\n', encoding='utf-8')
    current = tmp_path / 'current' / 'python.exe'
    monkeypatch.setattr(process_background.sys, 'executable', str(current))
    monkeypatch.setenv('VIRTUAL_ENV', str(inherited))

    interpreter, extra = process_background.background_spawn()

    assert interpreter == str(current)
    assert extra == {}


@pytest.mark.skipif(os.name != 'nt', reason='Windows base-interpreter venv launch chain')
def test_windows_two_background_hops_import_venv_only_dependency(tmp_path: Path) -> None:
    venv = tmp_path / 'venv'
    subprocess.run(
        [sys.executable, '-m', 'venv', '--without-pip', str(venv)],
        check=True, capture_output=True, text=True,
    )
    (venv / 'Lib' / 'site-packages' / 'ccb_venv_probe.py').write_text(
        "VALUE = 'venv_dependency_preserved'\n", encoding='utf-8',
    )
    lib_root = str(Path(__file__).resolve().parents[1] / 'lib')
    script = tmp_path / 'spawn_probe.py'
    script.write_text(
        'import sys, subprocess\n'
        f'sys.path.insert(0, {lib_root!r})\n'
        'from process_background import background_spawn, no_window_process_kwargs\n'
        'from ccbd.daemon_process import _ccbd_env\n'
        "if '--daemon' in sys.argv:\n"
        '    import ccb_venv_probe\n'
        '    print(ccb_venv_probe.VALUE)\n'
        'else:\n'
        '    interpreter, extra = background_spawn()\n'
        "    mode = '--daemon' if '--keeper' in sys.argv else '--keeper'\n"
        '    child = subprocess.run([interpreter, __file__, mode],\n'
        '        env=_ccbd_env(keeper_pid=None, venv_env=extra),\n'
        '        capture_output=True, text=True, timeout=20,\n'
        '        **no_window_process_kwargs())\n'
        '    print(child.stdout, end="")\n'
        '    print(child.stderr, end="", file=sys.stderr)\n'
        '    sys.exit(child.returncode)\n',
        encoding='utf-8',
    )

    result = subprocess.run(
        [str(venv / 'Scripts' / 'python.exe'), str(script)],
        capture_output=True, text=True, timeout=40,
        **process_background.no_window_process_kwargs(),
    )

    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == 'venv_dependency_preserved'


def test_spawn_failure_reclaims_only_spawned_child_and_closes_parent_logs(
    tmp_path: Path,
    monkeypatch,
) -> None:
    captured: dict[str, object] = {}
    process = type('Process', (), {'pid': 4321})()

    def fake_popen(*args, **kwargs):
        captured['stdout'] = kwargs['stdout']
        captured['stderr'] = kwargs['stderr']
        captured['start_new_session'] = kwargs['start_new_session']
        return process

    def fail_wait(**kwargs) -> None:
        assert kwargs['process'] is process
        raise daemon_process.CcbdProcessError('readiness mismatch')

    reclaimed: list[object] = []
    monkeypatch.setattr(daemon_process.subprocess, 'Popen', fake_popen)
    monkeypatch.setattr(daemon_process, '_wait_for_ccbd_ready', fail_wait)
    monkeypatch.setattr(daemon_process, '_terminate_spawned_process', reclaimed.append)

    with pytest.raises(daemon_process.CcbdProcessError, match='readiness mismatch'):
        daemon_process.spawn_ccbd_process(
            project_root=tmp_path,
            socket_path=tmp_path / 'ccbd.sock',
            ccbd_dir=tmp_path / '.ccb' / 'ccbd',
            timeout_s=0.01,
            expected_startup_id='a' * 32,
            expected_generation=7,
        )

    assert reclaimed == [process]
    assert captured['start_new_session'] is True
    assert captured['stdout'].closed
    assert captured['stderr'].closed


def test_spawned_process_cleanup_escalates_and_reaps(monkeypatch) -> None:
    class FakeProcess:
        pid = 4321

        def __init__(self) -> None:
            self.wait_calls = 0

        def poll(self):
            return None

        def wait(self, *, timeout):
            self.wait_calls += 1
            if self.wait_calls == 1:
                raise subprocess.TimeoutExpired('ccbd', timeout)
            return -9

        def terminate(self) -> None:
            raise AssertionError('process-group termination should be used')

        def kill(self) -> None:
            raise AssertionError('process-group kill should be used')

    signals: list[tuple[int, int]] = []
    monkeypatch.setattr(daemon_process.os, 'killpg', lambda pid, sig: signals.append((pid, sig)), raising=False)
    monkeypatch.setattr(daemon_process.signal, 'SIGKILL', 9, raising=False)
    process = FakeProcess()

    daemon_process._terminate_spawned_process(process, timeout_s=0.01)

    assert signals == [
        (4321, daemon_process.signal.SIGTERM),
        (4321, daemon_process.signal.SIGKILL),
    ]
    assert process.wait_calls == 2
