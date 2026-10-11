# Native commit metadata optimization

This removes known redundant metadata work. It does **not** establish a large
end-to-end latency improvement: the initial speed difference was confounded by
executable placement, as the controls below demonstrate.

The Windows baseline did not establish an unconditional native latency win.
Inspection found a smaller, avoidable cost in `Store::commit`: every staged
write queried its file signature both before and after applying it, even when
there was no cached index for that path. Each signature query also traversed
the guarded state path.

The new commit path queries signatures only for an append that has a cached
latest-row index to update. An uncached write simply applies the existing write
operation. Replacements and removals invalidate their cached indices directly.
The native `records.metrics` response adds `signature_reads` so this work can be
observed without logging record contents or filesystem paths.

The redo journal, write ordering, append offsets, per-file synchronization,
atomic replacement, recovery replay and ownership/path checks in `apply` remain
the existing implementations. No writes are batched or removed. This change
does not weaken durability to match the legacy append-only Python path.

Before the change, a regression test with an uncached append, save and removal
observed six unnecessary signature queries. After the change it observes zero,
while checking the persisted content. Another test exercises two cached index
identities, incremental append without a history rescan, detection of externally
changed history, whole-file replacement and removal. Existing cross-stream,
admission and completion tests exercise every recorded crash boundary.

The paired runner `dev_tools/perf_native_commit.py` compares a saved pre-change
release binary with the rebuilt binary. Each round alternates their order and
uses the same inputs and authenticated native API transport. It validates every
queue admission and the completion of each callback continuation. Raw results
include binary hashes and per-run samples. A low-sample desktop experiment is
not a general performance guarantee; the known reduction in redundant metadata
work is separate from any observed latency difference.

```powershell
& '<test Python>' dev_tools/perf_native_commit.py --before '<saved old ccb-kernel.exe>' --runs 10 --samples 30 --output docs/analysis/native-commit-metadata-20261012.json
```

The experiment uses fresh temporary controllers with inert agents. It does not
launch providers, models, Herdr or existing workspaces. Production installation
and default-runtime promotion remain outside this checkpoint.

## Measurements and controls

| Experiment | Admission p50 before / after (ms) | Callback p50 before / after (ms) |
| --- | ---: | ---: |
| Initial 10 pairs, old image in Temp and new image in repository | 46.30 / 29.58 | 137.52 / 65.64 |
| 3 pairs, both images in sibling directories under the same Temp parent | 45.30 / 44.93 | 150.96 / 137.41 |
| 3 pairs, identical optimized binary hash, Temp versus repository | 46.04 / 30.09 | 144.74 / 59.87 |

The identical-binary control reproduces most of the initial difference without
any code change. Consequently the first row is not evidence of a 36% submission
speedup or a 52% callback speedup from this patch. The same-location submission
result is effectively similar at this sample size. The smaller callback
difference has only three paired observations and is not a reliable general
performance claim. The Windows cause of the location sensitivity was not
independently diagnosed; it must not be attributed to antivirus or a specific
OS component without additional evidence.

Raw data is retained for all three experiments:
[initial](native-commit-metadata-20261012.json),
[matched location](native-commit-metadata-location-control-20261012.json), and
[identical binary](native-commit-identical-binary-control-20261012.json).
Each native admission is verified in its queue and every callback continuation
is executed to completion. The combined experiments validate 960 admissions and
32 callback cases, but they do not represent 960 independent environment trials.

Validation: 81 selected kernel/records/admission/finalization/daemon/CLI security/
agent-journal cases passed in 125.80 seconds. Rust unit tests passed 27 cases,
including the parent that invokes the process-death helper. Formatting, Clippy
with warnings denied, diff checks and release/bin hash equality passed. No full
repository regression or production rollout is claimed.
