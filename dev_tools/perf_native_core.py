"""Windows core contract benchmark; no model, terminal or existing runtime.

Uses the development parity fixtures, so run with the test Python environment.
Pure Python and Python+Rust measure identical admission and callback-repair
contracts. Standalone Rust is reported separately, including its actual idle
actor loop. Harness startup is not full CCB/Herdr/provider startup.
"""
from __future__ import annotations

import argparse
import ctypes
from ctypes import wintypes
import hashlib
import json
import math
import os
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
KERNEL = ROOT / 'rust/target/release/ccb-kernel.exe'


def resources(pids):
    """Cumulative process CPU and Windows working set, for exact owned PIDs."""
    class Memory(ctypes.Structure):
        _fields_ = [('cb', wintypes.DWORD), ('faults', wintypes.DWORD)] + [
            (name, ctypes.c_size_t) for name in ['peak_rss', 'rss', 'peak_paged', 'paged',
                                               'peak_nonpaged', 'nonpaged', 'pagefile', 'peak_pagefile', 'private']]
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    psapi = ctypes.WinDLL('psapi', use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Memory), wintypes.DWORD]
    rows = []
    for pid in pids:
        handle = kernel.OpenProcess(0x0400 | 0x0010, False, pid)
        if not handle:
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            memory = Memory(); memory.cb = ctypes.sizeof(memory)
            clocks = [wintypes.FILETIME() for _ in range(4)]
            if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(memory), memory.cb):
                raise ctypes.WinError(ctypes.get_last_error())
            if not kernel.GetProcessTimes(handle, *(ctypes.byref(c) for c in clocks)):
                raise ctypes.WinError(ctypes.get_last_error())
            ticks = lambda t: (t.dwHighDateTime << 32) | t.dwLowDateTime
            rows.append({'pid': pid, 'created_ticks': ticks(clocks[0]),
                         'cpu_seconds': (ticks(clocks[2]) + ticks(clocks[3])) / 1e7,
                         'working_set_bytes': memory.rss, 'private_bytes': memory.private})
        finally:
            kernel.CloseHandle(handle)
    return rows


def idle(pids, seconds):
    before = resources(pids); begin = time.perf_counter()
    time.sleep(seconds)
    after = resources(pids); elapsed = time.perf_counter() - begin
    assert [(r['pid'], r['created_ticks']) for r in before] == [(r['pid'], r['created_ticks']) for r in after]
    return {'window_seconds': elapsed,
            'cpu_seconds': sum(a['cpu_seconds'] - b['cpu_seconds'] for a, b in zip(after, before)),
            'working_set_bytes': sum(r['working_set_bytes'] for r in after),
            'private_bytes': sum(r['private_bytes'] for r in after), 'processes': after}


def emit(value):
    print(json.dumps(value), flush=True)


def worker(mode, root, samples, idle_seconds):
    sys.path[:0] = [str(ROOT / 'test'), str(ROOT / 'lib')]
    os.environ['CCB_CORE_RUNTIME'] = 'python' if mode == 'python' else 'rust'
    os.environ['CCB_KERNEL_BIN'] = str(KERNEL)
    os.environ['CCB_TEST_KERNEL_BIN'] = str(KERNEL)
    from native_kernel import close_kernels
    from test_native_admission import request, plan, _submit_plan, NOW, NAMES
    from test_native_finalization import setup, interrupted_child, recovery_runtime, repair_callback_edges, decision
    from message_bureau import CallbackEdgeStore, ReplyStore
    class Environment:
        def setenv(self, key, value): os.environ[key] = str(value)
    server = None
    try:
        setup_begin = time.perf_counter()
        if mode == 'native':
            from test_native_daemon import Server, spec
            server = Server(root / 'admission' / 'ccbd', {name: spec() for name in NAMES})
            pids = [server.proc.pid]
            def submit_one(index):
                row = server.rpc('submit', {'project_id': 'native-test', 'to_agent': NAMES[index % len(NAMES)],
                                           'from_actor': 'user', 'body': 'x' * 120})
                return row['job_id']
        else:
            d = setup(root / 'admission', mode, Environment())
            pids = [os.getpid()]
            if mode == 'rust': pids.append(d._state._native.process.pid)
            def submit_one(index):
                receipt, _ = _submit_plan(d, plan(request(NAMES[index % len(NAMES)], body='x' * 120)), accepted_at=NOW)
                return receipt.jobs[0].job_id
        setup_ms = (time.perf_counter() - setup_begin) * 1000
        native_start_ms = None
        if mode == 'native':
            observed = time.time()
            created = resources(pids)[0]['created_ticks'] / 1e7 - 11644473600
            native_start_ms = (observed - created) * 1000
        emit({'ready': True, 'mode': mode, 'resource_pids': pids})
        fresh = idle(pids, idle_seconds)
        timings = []; ids = []
        for index in range(samples):
            begin = time.perf_counter(); ids.append(submit_one(index))
            timings.append((time.perf_counter() - begin) * 1000)
        assert len(set(ids)) == samples
        populated = idle(pids, idle_seconds)
        if mode == 'native':
            queued = sum(len(q['queued_job_ids']) for q in server.rpc('queue', {'target': 'all'})['queues'])
        else:
            queued = sum(len(d._state.queued_items_for('agent', name)) for name in NAMES)
        assert queued == samples, (queued, samples)
        if server: server.close(); server = None
        close_kernels()

        if mode == 'native':
            from test_native_project_view import finish
            server = Server(root / 'callback' / 'ccbd', {name: spec() for name in NAMES})
            parent = server.rpc('submit', {'project_id': 'native-test', 'to_agent': NAMES[0], 'from_actor': 'user', 'body': 'parent'})
            p = server.rpc('work', {'agent_name': NAMES[0]})['job']
            server.rpc('submit', {'project_id': 'native-test', 'to_agent': NAMES[1], 'from_actor': NAMES[0], 'body': 'child', 'route_options': {'mode': 'chain'}})
            finish(server, p)
            child = server.rpc('work', {'agent_name': NAMES[1]})['job']
            begin = time.perf_counter(); finish(server, child)
            continuation = server.rpc('work', {'agent_name': NAMES[0]})['job']
            callback_ms = (time.perf_counter() - begin) * 1000
            assert continuation['request']['message_type'] == 'chain_continuation'
            finish(server, continuation)
            assert server.rpc('watch', {'target': parent['job_id']})['visible_reply_source'] == 'message_bureau_reply'
            callback_contract = 'child complete RPC through continuation claim RPC (includes loopback transport)'
        else:
            d = setup(root / 'callback', mode, Environment())
            _, child = interrupted_child(d); recovery_runtime(d)
            if mode == 'python':
                d._message_bureau.record_attempt_terminal(child, decision('child-result'), finished_at=NOW)
            d._message_bureau.record_reply(child, decision('child-result'), finished_at=NOW, deliver_to_caller=False)
            begin = time.perf_counter(); repaired = repair_callback_edges(d)
            callback_ms = (time.perf_counter() - begin) * 1000
            edges = CallbackEdgeStore(d._layout).list_latest()
            assert len(repaired) == len(edges) == 1 and edges[0].continuation_job_id
            assert len(ReplyStore(d._layout).list_all()) == 1
            callback_contract = 'persisted terminal child and reply through queued continuation; identical Python caller'
        emit({'mode': mode, 'admission_ms': timings, 'callback_ms': callback_ms, 'callback_contract': callback_contract,
              'setup_ms_after_imports': setup_ms, 'native_process_creation_to_endpoint_ms': native_start_ms,
              'fresh_idle': fresh, 'populated_idle': populated, 'verified_queued_jobs': queued})
    finally:
        if server: server.close()
        close_kernels()


def summary(values):
    ordered = sorted(values)
    return {'count': len(values), 'min': min(values), 'p50': statistics.median(values),
            'p95_nearest_rank': ordered[math.ceil(.95 * len(ordered)) - 1], 'max': max(values)}


def token_benchmark(root, runs):
    """Compare secure token creation including each implementation's helpers."""
    sys.path.insert(0, str(ROOT / 'lib'))
    from ccbd.control_plane_transport.token_auth import create_token_file, load_token_file
    native = KERNEL.with_name('ccb-native.exe')
    values = {'python': [], 'native': []}
    for index in range(runs):
        for mode in (['python', 'native'] if index % 2 == 0 else ['native', 'python']):
            path = root / f'{index}-{mode}.json'
            assert not path.exists()
            begin = time.perf_counter()
            if mode == 'python':
                token = create_token_file(path)
                assert token.acl_status == 'windows-icacls-user-read'
            else:
                result = subprocess.run([str(native), 'token-create', str(path)],
                                        capture_output=True, text=True, encoding='utf-8', creationflags=0x08000000)
                assert result.returncode == 0, result.stderr
                assert json.loads(result.stdout)['acl_status'] == 'windows-native-user-read'
            values[mode].append((time.perf_counter() - begin) * 1000)
            assert load_token_file(path).token
    return {'milliseconds': values, 'summaries': {mode: summary(v) for mode, v in values.items()},
            'contract': 'create a new same-user read-protected token; includes helper subprocesses and ACL verification; excludes module import'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--worker', choices=['python', 'rust', 'native'])
    parser.add_argument('--root', type=Path)
    parser.add_argument('--samples', type=int, default=30)
    parser.add_argument('--runs', type=int, default=10)
    parser.add_argument('--idle-seconds', type=float, default=2)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    if os.name != 'nt': parser.error('this runner uses Windows process counters')
    if not (1 <= args.samples <= 1000 and 1 <= args.runs <= 100 and .1 <= args.idle_seconds <= 10):
        parser.error('invalid benchmark bounds')
    if args.worker:
        assert args.root and not args.root.exists(), 'worker root must be new'
        return worker(args.worker, args.root, args.samples, args.idle_seconds)
    assert KERNEL.is_file(), 'build the release kernel first'
    root = Path(tempfile.mkdtemp(prefix='ccb-core-perf-'))
    result = {'schema_version': 1, 'fixture_root': str(root), 'runs': args.runs, 'samples_per_run': args.samples,
              'kernel_sha256': hashlib.sha256(KERNEL.read_bytes()).hexdigest(), 'python': sys.version,
              'measurements': [], 'limitations': [
                  'Core-contract harness, not complete Python CcbdApp versus native startup',
                  'python=Python core; rust=Python coordinator plus native kernel; native=standalone native daemon',
                  'Python/rust admission timings use identical callers; native API-2 adds authenticated loopback RPC',
                  'Only Python/rust callback repair metrics share the same boundary; native callback latency has a different boundary',
                  'Harness-ready time includes Python test-fixture imports in all lanes; not pure native process startup',
                  'Python/rust idle is a quiescent harness, not their production daemon polling loop',
                  'Working set sums can double-count shared pages; private bytes are also reported',
                  'No cache flush, model, terminal, Herdr, skill materialization or real repository workload']}
    # Rotate order to reduce systematic warm-cache/background-load bias.
    modes = ['python', 'rust', 'native']
    for run in range(args.runs):
        for mode in modes[run % 3:] + modes[:run % 3]:
            command = [sys.executable, str(Path(__file__).resolve()), '--worker', mode, '--root', str(root / f'{run}-{mode}'),
                       '--samples', str(args.samples), '--idle-seconds', str(args.idle_seconds)]
            begin = time.perf_counter()
            proc = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding='utf-8', creationflags=0x08000000)
            ready = proc.stdout.readline()
            ready_ms = (time.perf_counter() - begin) * 1000
            output, error = proc.communicate()
            if proc.returncode or not ready:
                raise RuntimeError(f'benchmark worker failed ({mode}): {error}')
            assert json.loads(ready)['ready']
            row = json.loads(output); row.update(run=run, harness_ready_ms=ready_ms)
            result['measurements'].append(row)
            emit({'run': run, 'mode': mode, 'receipt_p50_ms': statistics.median(row['admission_ms']), 'callback_ms': row['callback_ms']})
    result['summaries'] = {}
    for mode in modes:
        rows = [r for r in result['measurements'] if r['mode'] == mode]
        result['summaries'][mode] = {'harness_ready_ms': summary([r['harness_ready_ms'] for r in rows]),
            'setup_ms_after_imports': summary([r['setup_ms_after_imports'] for r in rows]),
            'admission_ms': summary([v for r in rows for v in r['admission_ms']]),
            'callback_ms': summary([r['callback_ms'] for r in rows]),
            'fresh_working_set_bytes': summary([r['fresh_idle']['working_set_bytes'] for r in rows]),
            'fresh_private_bytes': summary([r['fresh_idle']['private_bytes'] for r in rows]),
            'populated_idle_one_core_cpu_percent': summary([100*r['populated_idle']['cpu_seconds']/r['populated_idle']['window_seconds'] for r in rows])}
        if mode == 'native':
            result['summaries'][mode]['process_creation_to_endpoint_ms'] = summary([r['native_process_creation_to_endpoint_ms'] for r in rows])
    output = args.output or root / 'result.json'
    result['secure_token_creation'] = token_benchmark(root / 'tokens', args.runs)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(result, indent=2) + '\n', encoding='utf-8')
    emit({'result': str(output), 'summaries': result['summaries']})


if __name__ == '__main__': main()
