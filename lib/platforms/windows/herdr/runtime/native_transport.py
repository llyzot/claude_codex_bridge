"""Transitional DTO bridge to Rust-owned, bounded Herdr command execution."""
from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess

from native_kernel import kernel_binary
from process_background import no_window_process_kwargs
from terminal_runtime.mux_backend_contract import MuxCommandErrorV2


def _invoke(request: dict, *, env: dict[str, str]) -> dict:
    override = os.environ.get('CCB_NATIVE_BIN')
    binary = Path(override).expanduser().resolve() if override else kernel_binary().with_name(
        'ccb-native.exe' if os.name == 'nt' else 'ccb-native')
    if not binary.is_file():
        raise RuntimeError('Rust Herdr transport binary is unavailable; rebuild ccb-native')
    result = subprocess.run(
        [str(binary), 'herdr-transport'], input=json.dumps(request, ensure_ascii=False),
        capture_output=True, text=True, encoding='utf-8', env=env,
        timeout=request['timeout_ms']/1000+5, check=False, **no_window_process_kwargs(),
    )
    if result.returncode:
        raise RuntimeError('Rust Herdr transport rejected its request or could not execute')
    response = json.loads(result.stdout)
    if not isinstance(response, dict) or response.get('schema_version') != 1:
        raise RuntimeError('Rust Herdr transport returned an unsupported contract')
    if response.get('session') != request['session'] or response.get('operation') != request['operation']:
        raise RuntimeError('Rust Herdr transport response identity mismatch')
    if (not isinstance(response.get('ok'), bool) or not isinstance(response.get('read_only'), bool)
            or response.get('automatic_replay') is not False or response.get('task_completion') is not False):
        raise RuntimeError('Rust Herdr transport returned an invalid outcome contract')
    if response['ok']:
        if response.get('outcome') != 'acknowledged' or not isinstance(response.get('stdout'), str):
            raise RuntimeError('Rust Herdr transport success is not a command acknowledgement')
    elif (response.get('outcome') not in {'unknown', 'unobserved'}
          or response.get('failure') not in {'spawn_failed','output_limit','output_read_failed','observation_timeout',
                                            'command_failed','invalid_utf8','server_rejected','invalid_json'}
          or response.get('stdout') is not None):
        raise RuntimeError('Rust Herdr transport failure contract is invalid')
    return response


def run(operation: str, command: list[str], *, expect_json: bool, session: str, env: dict[str, str], ipc_ref: str):
    if command[1:3] != ['--session', session]:
        raise ValueError('Rust Herdr transport requires an explicit matching session')
    timeout = int(os.environ.get('CCB_HERDR_COMMAND_TIMEOUT_MS') or '10000')
    request = dict(schema_version=1, operation=operation, session=session, command=[command[0]],
                   args=command[3:], expect_json=expect_json, timeout_ms=timeout)
    try:
        response = _invoke(request, env=env)
    except Exception as error:
        # A lost helper receipt can follow an accepted server-side mutation.
        # Never include the request body or untrusted CLI stderr in diagnostics.
        raise MuxCommandErrorV2(
            category='command-failed', backend_impl='herdr', operation=operation, ipc_ref=ipc_ref,
            detail='Rust Herdr transport outcome is unknown; inspect the named session before retrying',
            evidence={'runtime':'rust','outcome':'unknown','automatic_replay':False},
        ) from error
    if response.get('ok') is not True:
        raise MuxCommandErrorV2(
            category='command-failed', backend_impl='herdr', operation=operation, ipc_ref=ipc_ref,
            detail=f"Rust Herdr transport failed: {response.get('failure')}; inspect the named session before retrying",
            evidence={key: response.get(key) for key in (
                'session','operation','outcome','read_only','failure','exit_code','elapsed_ms','automatic_replay')},
        )
    return subprocess.CompletedProcess(command, 0, response.get('stdout') or '', '')
