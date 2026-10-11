"""Paired native-binary comparison using real durable admissions and callbacks.

Run with the development test Python. Creates only fresh temporary controllers;
no providers, models or terminal servers are started. The two binaries use the
same client, token setup, input sizes and authenticated API-2 transport.
"""
from __future__ import annotations
import argparse
import hashlib
import json
from pathlib import Path
import platform
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(ROOT / 'test'), str(ROOT / 'lib'), str(ROOT / 'dev_tools')]
from test_native_daemon import Server, spec, submit
import test_native_daemon
from test_native_project_view import finish
from perf_native_core import summary


def measure(binary, root, count):
    test_native_daemon.BIN = binary
    agents = {'lead': spec(), 'worker': spec()}
    server = Server(root / 'admission' / 'ccbd', agents)
    try:
        before = server.rpc('records.metrics', version=1)['result']
        elapsed = []; identifiers = []
        for index in range(count):
            start = time.perf_counter()
            receipt = submit(server, 'worker' if index % 2 else 'lead', body='x' * 120)
            elapsed.append((time.perf_counter() - start) * 1000)
            identifiers.append(receipt['job_id'])
        after = server.rpc('records.metrics', version=1)['result']
        assert len(set(identifiers)) == count
        assert sum(len(q['queued_job_ids']) for q in server.rpc('queue', {'target': 'all'})['queues']) == count
    finally:
        server.close()
    server = Server(root / 'callback' / 'ccbd', agents)
    try:
        parent = submit(server, 'lead')['job_id']
        p = server.rpc('work', {'agent_name': 'lead'})['job']
        server.rpc('submit', {'project_id': 'native-test', 'to_agent': 'worker', 'from_actor': 'lead',
                              'body': 'child', 'route_options': {'mode': 'chain'}})
        finish(server, p)
        child = server.rpc('work', {'agent_name': 'worker'})['job']
        start = time.perf_counter()
        finish(server, child)
        continuation = server.rpc('work', {'agent_name': 'lead'})['job']
        callback_ms = (time.perf_counter() - start) * 1000
        assert continuation['request']['message_type'] == 'chain_continuation'
        finish(server, continuation)
        assert server.rpc('watch', {'target': parent})['visible_reply_source'] == 'message_bureau_reply'
    finally:
        server.close()
    return {'admission_ms': elapsed, 'callback_ms': callback_ms,
            'records_metrics_before': before, 'records_metrics_after': after,
            'verified_queued_jobs': count, 'verified_callback': True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before', type=Path, required=True)
    parser.add_argument('--after', type=Path, default=ROOT / 'rust/target/release/ccb-kernel.exe')
    parser.add_argument('--runs', type=int, default=10)
    parser.add_argument('--samples', type=int, default=30)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    if not (1 <= args.runs <= 100 and 1 <= args.samples <= 1000): parser.error('invalid sample bounds')
    binaries = {'before': args.before.resolve(), 'after': args.after.resolve()}
    if not all(p.is_file() for p in binaries.values()): parser.error('both binaries must exist')
    root = Path(tempfile.mkdtemp(prefix='ccb-native-commit-paired-'))
    output = args.output or root / 'result.json'
    result = {'schema_version': 1, 'complete': False, 'platform': platform.platform(), 'fixture_root': str(root),
              'binary_sha256': {k: hashlib.sha256(p.read_bytes()).hexdigest() for k, p in binaries.items()},
              'runs': args.runs, 'samples_per_run': args.samples, 'measurements': [],
              'limitations': ['Controlled inert agents, no model or provider execution',
                              'Native callback boundary: child completion RPC through continuation claim RPC',
                              'No OS cache flush; unrelated desktop load may affect small samples',
                              'All journal/file synchronization stays enabled in both binaries']}
    def save():
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(result, indent=2) + '\n', encoding='utf-8')
    save()
    for run in range(args.runs):
        for label in (['before', 'after'] if run % 2 == 0 else ['after', 'before']):
            row = measure(binaries[label], root / f'{run}-{label}', args.samples)
            row.update(run=run, label=label); result['measurements'].append(row); save()
            print(json.dumps({'run': run, 'label': label, 'admission_p50_ms': summary(row['admission_ms'])['p50'],
                              'callback_ms': row['callback_ms']}), flush=True)
    result['summaries'] = {}
    for label in binaries:
        rows = [r for r in result['measurements'] if r['label'] == label]
        result['summaries'][label] = {'admission_ms': summary([v for r in rows for v in r['admission_ms']]),
                                    'callback_ms': summary([r['callback_ms'] for r in rows])}
    result['complete'] = True; save()
    print(json.dumps({'result': str(output), 'summaries': result['summaries']}), flush=True)


if __name__ == '__main__': main()
