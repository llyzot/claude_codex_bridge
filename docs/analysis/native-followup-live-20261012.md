# Real native followup acceptance

Commit `1fcafc79` was exercised against real Pi (`gpt-6-astra`) and Codex
(`gpt-6.1-sol`) in a fresh local project. Pi used native RPC; Codex used the
native authenticated WebSocket host. This run did not open Herdr or either
original terminal UI and did not change the production installation.

Two scenarios succeeded:

1. A direct Codex task initially requested two sum-of-squares derivations. A
   same-turn followup replaced its requested final result with `67 * 71` and a
   fresh marker. The task completed with `4,757` and
   `FOLLOWUP_DIRECT_4757_a0c27bc4`.
2. Real Pi submitted a worker task through native `ask --chain`, then ended its
   initial turn. A followup changed that worker's requested result and marker.
   Codex returned `FOLLOWUP_CHAIN_4757_a0c27bc4`; CCB automatically admitted one
   continuation into the same Pi process and native session. Pi checked the
   arithmetic and returned `REVIEWED: FOLLOWUP_CHAIN_4757_a0c27bc4`.

Pi released its initial turn **2.100246 seconds before** the worker completed.
No manual prompt or polling command was sent to Pi to resume review. The final
callback edge was `done`, with exactly one continuation job.

The native Codex history contains exactly two started/completed turns, one for
each scenario, and exactly one supplemental user input for each fresh marker.
Every acknowledgement binding matches the completed job's execution instance
and provider turn ID. No Codex tool calls occurred. This demonstrates same-turn
input rather than a replacement task or duplicated model request.

The review also exposed an expected semantic boundary: Pi correctly noted that
the altered worker answer no longer fulfilled the original sum-of-squares
request. `followup` changes input to the worker; it does not silently rewrite the
parent's requirements or force the Lead to approve the result. This run proves
delivery and review resumption, not approval of the original business task.

The four owned test processes (daemon, Pi, host and hosted Codex server) were
confirmed absent after cleanup. Native session histories and the disposable
project were retained as evidence. Exact IDs, timestamps, history/binary hashes,
results and the local trial path are in [the evidence](native-followup-live-20261012.json).

This is one successful real-model run per scenario. Broader failure/recovery
coverage remains fixture-based; it is not a reliability or performance benchmark.
Other-provider steering, legacy socket adoption, retention, full default-runtime
compatibility and production promotion remain unfinished.
