# Windows core performance baseline

The benchmark runs only fresh disposable state and controlled completions. It
does not launch models, Herdr, terminal windows or existing user agents. The
reusable runner is `dev_tools/perf_native_core.py` and requires the development
Python environment used for the contract tests.

```powershell
& '<test Python>' dev_tools/perf_native_core.py --runs 10 --samples 30 --idle-seconds 2 --output docs/analysis/native-core-performance-20261012.json
```

## Boundaries

Three lanes are deliberately kept separate:

- `python`: the legacy Python core behind the existing parity harness.
- `rust`: the same Python caller using the Rust kernel over its existing IPC
  adapter. This measures the transitional architecture, not a native-only app.
- `native`: the standalone Rust actor, driven through authenticated API-2
  loopback requests. Its Python measurement process is excluded from the native
  resource count.

Each lane has three inert agents and receives the same 120-byte request bodies.
Every run asserts that all receipts have unique IDs and every admitted job is
still present in its queue. Each callback case asserts one valid continuation.
Native callback cases additionally execute that continuation and verify that
the original parent exposes its final message-bureau reply.

Python and transitional Rust callback timings cover the same boundary: an
already persisted terminal child and reply becoming a queued continuation.
Native timings cover a different, larger boundary: the child-completion RPC
through the continuation-claim RPC. They must not be presented as a direct
callback speed ratio. The Python repair path requires the existing reply, so
both comparison inputs explicitly include it; Rust's extra ability to recover
a missing reply is not used to bias the benchmark.

`harness_ready_ms` includes test-fixture imports in all lanes. It is not full
application startup, nor pure native process startup. The separate native
creation-to-endpoint measurement uses Windows process creation time and the
moment its endpoint becomes visible; token/config preparation precedes it.

Resource counters use Windows process creation identity, cumulative CPU,
working set and private bytes. Transitional Rust includes both the Python
caller and its kernel. Python measurements include the parity/test harness
imports, so the memory comparison is not an exact production-application
reduction. Working-set sums can double-count shared pages. Python/sidecar idle
measurements are quiescent harnesses; only the native lane exercises an actual
daemon polling loop. CPU percentage uses one logical core as 100%.

Ten runs rotate execution order to reduce systematic cache/load bias. No OS
cache flush is performed. Admission has 300 samples per lane; callback and
startup have only ten, so their nearest-rank p95 is merely the largest observed
sample, not a confident tail estimate. This desktop may have unrelated load.

## Interpretation

Measured on the local Windows machine with release binaries from `f474a38d`:

| Lane | Admission p50 / p95 (ms) | Callback p50 (ms) | Fresh working set p50 (MiB) |
| --- | ---: | ---: | ---: |
| Python core harness | 21.63 / 32.51 | 35.73 | 52.20 |
| Python + Rust kernel | 30.23 / 36.86 | 82.04 | 58.99 |
| Standalone Rust actor | 28.98 / 36.25 | 71.99* | 6.57 |

`*` The standalone callback boundary includes terminal persistence and two
authenticated RPCs; the other two measure callback repair only. Likewise the
standalone admission boundary includes API validation/transport that the two
internal admission callers do not. These columns expose observed costs; they
do not establish a like-for-like standalone language speedup or slowdown.

The comparable transitional path was slower than Python in these small-state
samples. Keeping a Python coordinator beside Rust did not reduce its measured
memory footprint either. The standalone actor had a small absolute footprint,
but the Python column includes test-harness imports and cannot establish the
exact memory saving of a fully deployed application.

Native process creation to endpoint had p50 **44.67 ms** (observed max/p95
46.51 ms). The fixture's setup p50 was **877.32 ms**, because this fixture still
uses Python's secure-token creator before starting the native process. It would
be incorrect to call that setup number the native binary startup time.

A separate ten-sample secure-token comparison confirmed the difference:
Python's current-user ACL creation/verification path had p50 **816.32 ms**;
the public native `token-create` command had p50 **40.90 ms**, including its own
process startup. Both implementations protect the newly created token and the
benchmark verifies their successful ACL status and token readability. The
native path already exists in production code; this run measured its benefit
rather than introducing a weaker permission shortcut. No token contents are
included in the report.

Standalone native idle CPU after 30 queued inert jobs had p50 **0.39% of one
logical core** over two-second windows; the largest observed window was 3.12%.
Small samples and Windows CPU-accounting granularity preclude a stronger idle
CPU claim. The Python harnesses sleep and are not production idle baselines.

Raw samples, process identities, parameters and binary hashes are in
[the measured results](native-core-performance-20261012.json). The final runner
also passed a separate one-run/three-admission smoke test through all three
lanes and token creation. No Rust runtime source changed in this checkpoint.

This is a core-contract baseline, not a completed production performance gate.
It must not support a blanket claim that rewriting in Rust makes CCB faster.
In particular, `Store::commit` writes a redo journal and synchronizes native
target files; the legacy `JsonlStore.append` closes an ordinary append without
the same per-append `fsync`. Observed differences therefore include durability,
filesystem and architecture costs as well as language/runtime effects.

Full Python CcbdApp versus native startup/idle comparison, large-history and real
repository workloads, terminal/provider startup and production rollout remain
separate requirements. The existing Linux `/proc` startup harness cannot prove
those Windows results. Optimizations must retain the native recovery guarantees.
