from io import StringIO
import os
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace

import pytest
from platforms.windows.herdr import entrypoint as native


@pytest.mark.parametrize('kind', ['herdr-open', 'start', 'ping'])
def test_native_dispatch_only_overrides_owned_commands(monkeypatch, kind):
    calls = []
    services = object()
    monkeypatch.setattr(native.phase2, '_dispatch_services', lambda: services)
    def custom(*args, **kwargs):
        calls.append((args, kwargs))
        return 7
    monkeypatch.setattr(native, 'handle_herdr_open', custom)
    monkeypatch.setattr(native, 'handle_start', custom)
    monkeypatch.setattr(native.phase2, '_dispatch_impl', custom)
    command = SimpleNamespace(kind=kind)
    assert native._native_dispatch('context', command, 'out') == 7
    assert calls[0][0] == ('context', command, 'out', services)
    assert calls[0][1] == {}


def test_native_foreground_reuses_terminal_without_spawning_ui(monkeypatch):
    fg = native.foreground
    payload = {'namespace_backend_family': 'herdr-native', 'namespace_backend_impl': 'herdr',
               'namespace_id': 'w1', 'namespace_session_name': 'ccb-test',
               'namespace_ipc_kind': 'herdr_socket', 'namespace_ipc_ref': 'herdr://test',
               'namespace_ui_attachable': True, 'namespace_workspace_window_name': 'main'}
    calls = []
    monkeypatch.setattr(fg, '_foreground_attach_client', lambda context: object())
    monkeypatch.setattr(fg, '_wait_for_attach_target', lambda *a, **kw: payload)
    monkeypatch.setattr(fg, '_launch_herdr_ui', lambda *a: pytest.fail('must not open a second UI'))
    monkeypatch.setattr(fg, '_build_herdr_attach_backend', lambda **kw: SimpleNamespace(
        attach_namespace=lambda ref, **opts: calls.append((ref, opts))))
    result = native.attach_started_project_namespace(SimpleNamespace(project=SimpleNamespace(project_id='test')))
    assert result.backend_impl == 'herdr'
    assert len(calls) == 1 and calls[0][1]['window_name'] == 'main'


def test_native_wrapper_keeps_shared_source_guard(tmp_path):
    root = Path(__file__).resolve().parents[1]
    env = dict(os.environ, CCB_SKIP_HERDR_CHECK='1', PYTHONUTF8='1')
    for key in ('PYTEST_CURRENT_TEST', 'CCB_SOURCE_RUNTIME_OK', 'CCB_SOURCE_ALLOWED_ROOTS'):
        env.pop(key, None)
    result = subprocess.run([sys.executable, str(root/'platforms/windows/ccb.py'), 'doctor'],
                             cwd=tmp_path, env=env, capture_output=True, encoding='utf-8', timeout=30)
    assert result.returncode == 1
    assert 'Refusing to run the CCB source checkout' in result.stderr
    version = subprocess.run([sys.executable, str(root/'platforms/windows/ccb.py'), '--print-version'],
                              cwd=tmp_path, env=env, capture_output=True, encoding='utf-8', timeout=30)
    assert version.returncode == 0
    assert version.stdout.strip() == 'v' + (root/'VERSION').read_text().strip()


@pytest.mark.parametrize('explicit,expected', [(None, 600.0), ('120', 120.0)])
def test_native_script_sets_cold_start_budget_before_policy_import(tmp_path, explicit, expected):
    root = Path(__file__).resolve().parents[1]
    env = dict(os.environ, CCB_SKIP_HERDR_CHECK='1', PYTHONUTF8='1')
    env.pop('CCB_STARTUP_TRANSACTION_TIMEOUT_S', None)
    if explicit is not None:
        env['CCB_STARTUP_TRANSACTION_TIMEOUT_S'] = explicit
    script = str(root / 'platforms/windows/ccb.py')
    code = (
        f'import runpy; runpy.run_path({script!r}, run_name="native_budget_probe"); '
        'from ccbd.startup_policy import STARTUP_TRANSACTION_TIMEOUT_S; '
        'print(STARTUP_TRANSACTION_TIMEOUT_S)'
    )
    result = subprocess.run([sys.executable, '-c', code], cwd=tmp_path, env=env,
                            capture_output=True, encoding='utf-8', timeout=30)
    assert result.returncode == 0, result.stderr
    assert float(result.stdout.strip().splitlines()[-1]) == expected
