from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path


def background_process_kwargs() -> dict[str, object]:
    kwargs: dict[str, object] = {'start_new_session': True}
    if os.name == 'nt':
        flags = (
            _subprocess_flag('CREATE_NEW_PROCESS_GROUP', 0x00000200)
            | _subprocess_flag('DETACHED_PROCESS', 0x00000008)
            | _subprocess_flag('CREATE_NO_WINDOW', 0x08000000)
        )
        kwargs['creationflags'] = flags
    return kwargs


def no_window_process_kwargs() -> dict[str, object]:
    if os.name != 'nt':
        return {}
    return {'creationflags': _subprocess_flag('CREATE_NO_WINDOW', 0x08000000)}


def venv_base_interpreter() -> str | None:
    """Resolve the real base interpreter of the active Windows venv.

    On Windows a venv's ``Scripts\\python.exe`` is the venvlauncher redirector:
    it re-executes the base interpreter (e.g. ``Python314\\python.exe``) as a
    child process.  Spawning the redirector (a) breaks ``CREATE_NO_WINDOW`` /
    ``DETACHED_PROCESS`` propagation to the real interpreter, so a visible
    console window appears, and (b) makes the ``Popen`` pid differ from the
    daemon's own ``os.getpid()``, breaking pid-based readiness identity checks.
    Returns ``None`` when not on Windows, not inside a venv, or the base
    interpreter is missing.
    """
    if os.name != 'nt':
        return None
    executable = Path(sys.executable)
    if executable.name.lower() not in {'python.exe', 'pythonw.exe'}:
        return None
    return _base_interpreter_for_venv(executable.parent.parent, executable.name)


def _base_interpreter_for_venv(venv_root: Path, executable_name: str) -> str | None:
    cfg = venv_root / 'pyvenv.cfg'
    if not cfg.is_file():
        return None
    home = ''
    try:
        for line in cfg.read_text(encoding='utf-8').splitlines():
            key, _, value = line.partition('=')
            if key.strip() == 'home':
                home = value.strip()
                break
    except OSError:
        return None
    if not home:
        return None
    candidate = Path(home) / executable_name
    return str(candidate) if candidate.is_file() else None


def background_spawn() -> tuple[str, dict[str, str]]:
    """Return ``(interpreter, extra_env)`` for spawning a CCB background daemon.

    On Windows this prefers the venv's real base interpreter (bypassing the
    venvlauncher redirector) so that ``CREATE_NO_WINDOW`` applies directly to
    the spawned process and ``Popen.pid`` equals the daemon's own pid.  The
    venv site-packages are carried into ``PYTHONPATH`` (the base interpreter
    does not auto-add them without the redirector) and ``VIRTUAL_ENV`` is set.
    A keeper already using the base executable reconstructs that dependency
    path from its inherited venv, after matching the configured interpreter.
    Off Windows this is ``(sys.executable, {})``.
    """
    if os.name != 'nt':
        return sys.executable, {}
    interpreter = venv_base_interpreter() or sys.executable
    executable = Path(sys.executable)
    venv_root = executable.parent.parent
    if not (venv_root / 'pyvenv.cfg').is_file():
        inherited = str(os.environ.get('VIRTUAL_ENV') or '').strip()
        if not inherited or executable.name.lower() not in {'python.exe', 'pythonw.exe'}:
            return interpreter, {}
        venv_root = Path(inherited).expanduser()
        base = _base_interpreter_for_venv(venv_root, executable.name)
        try:
            if base is None or Path(base).resolve() != executable.resolve():
                return interpreter, {}
        except (OSError, RuntimeError):
            return interpreter, {}
    extra: dict[str, str] = {}
    site_packages = venv_root / 'Lib' / 'site-packages'
    if site_packages.is_dir():
        extra['PYTHONPATH'] = str(site_packages)
    if (venv_root / 'pyvenv.cfg').is_file():
        extra['VIRTUAL_ENV'] = str(venv_root)
    return interpreter, extra


def _subprocess_flag(name: str, fallback: int) -> int:
    return int(getattr(subprocess, name, fallback) or fallback)


__all__ = [
    'background_process_kwargs',
    'no_window_process_kwargs',
    'venv_base_interpreter',
    'background_spawn',
]
