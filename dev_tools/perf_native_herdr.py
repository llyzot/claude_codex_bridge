"""Measure native startup/reconnect against the offline Herdr protocol fixture.

No real Herdr, provider account, model task or global session is created.
Run with the test Python; --native selects an unchanged baseline or a new build.
"""
import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(ROOT / 'test'), str(ROOT / 'lib')]
from test_native_herdr_open import project, setup, read
from test_native_herdr_worker import wait_for


def test_measure_native_herdr(project):
    p = project
    calls_path = p['backend'] / 'calls.jsonl'

    def calls():
        return [json.loads(line) for line in calls_path.read_text(encoding='utf-8').splitlines()] if calls_path.exists() else []

    rows = []
    daemon_pid = None
    for index in range(int(os.environ['CCB_PERF_HERDR_SAMPLES']) + 1):
        before = len(calls())
        started = time.perf_counter()
        result = p['open']()
        assert result.returncode == 0, result.stderr
        wait_for(lambda: read(p['native'] / 'herdr/herdr-open.json')['phase'] == 'running', timeout=60)
        elapsed = (time.perf_counter() - started) * 1000
        rows.append({'kind': 'cold' if index == 0 else 'reconnect', 'milliseconds': elapsed,
                     'herdr_calls': dict(Counter(' '.join(c[:2]) for c in calls()[before:]))})
        current_pid = p['ready']()['pid']
        if daemon_pid is not None:
            assert current_pid == daemon_pid
        daemon_pid = current_pid
    state = p['ready']()
    assert state and p['pi'].poll() is None and p['codex'].poll() is None
    all_calls = calls()
    assert sum(c[:2] == ['agent', 'start'] for c in all_calls) == 2
    assert not p['prompts']()
    report = {'schema_version': 1, 'native': os.environ['CCB_TEST_NATIVE_BIN'], 'rows': rows,
              'native_sha256': hashlib.sha256(Path(os.environ['CCB_TEST_NATIVE_BIN']).read_bytes()).hexdigest(),
              'reconnect_median_ms': statistics.median(r['milliseconds'] for r in rows[1:]),
              'limits': 'Offline Herdr subprocess fixture; no real provider initialization or network model latency.',
              'verified': {'agent_launches': 2, 'model_prompts': 0, 'same_daemon_after_reconnect': True}}
    Path(os.environ['CCB_PERF_HERDR_OUTPUT']).write_text(json.dumps(report, indent=2), encoding='utf-8')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--native', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=3)
    args = parser.parse_args()
    assert args.native.is_file() and 1 <= args.samples <= 20
    env = os.environ.copy()
    env.update(CCB_TEST_NATIVE_BIN=str(args.native.resolve()), CCB_PERF_HERDR_OUTPUT=str(args.output.resolve()),
               CCB_PERF_HERDR_SAMPLES=str(args.samples))
    raise SystemExit(subprocess.call([sys.executable, '-m', 'pytest', str(Path(__file__).resolve()), '-q', '-x', '--tb=short'],
                                    cwd=ROOT, env=env))
