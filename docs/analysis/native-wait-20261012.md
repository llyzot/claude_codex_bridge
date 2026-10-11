# Native reply waiting commands

The Rust CLI now supports the existing group-observation syntax:

```powershell
ccb wait-any <id> --timeout 30
ccb wait-all <id> --timeout 30
ccb wait-quorum 2 <id> --timeout 30
```

Targets are submission, message, attempt, reply or job IDs. The daemon exposes
`wait-summary`, which returns one immediate snapshot. The CLI performs polling;
waiting never blocks the authority actor or changes a job, mailbox or callback.
Normal agent result chains still use asynchronous `ask --chain`, not these waits.

Selection follows the legacy reply policy: group by message/agent route, choose
the latest retry attempt, then its latest reply. Failed, cancelled and incomplete
terminal replies count as received replies. `satisfied` means the requested reply
threshold was met; inspect `terminal_status` for each task's outcome. Notices can
meet the observation threshold but return `wait_status: notice`, with separate
terminal/notice counts. They are not treated as proof of task completion.

A delegated parent Job follows its logical message's continuation attempts,
rather than returning when the parent's first physical turn ends. Other direct
attempt/reply/job selectors retain their explicit route identity. Message and
submission selectors observe their latest routes, including retries.

The CLI retains the initial project and target identity across observations.
Transport failures end observation without starting a daemon or replaying work.
Timeout emits the last snapshot with `observation_timed_out: true`, exits nonzero,
and leaves the original task IDs usable. `--timeout` uses seconds and defaults to
30; `CCB_WAIT_TIMEOUT_S` and `CCB_WAIT_POLL_INTERVAL_S` retain their legacy roles.
The additive `--timeout-ms 0` performs one observation. Native waits are bounded
to one day and reject non-finite explicit deadlines.

Reply bodies inherit native diagnostic text bounds and retain artifact metadata;
artifacts are not read. Unlike using a paginated trace as input, the reply-route
snapshot computes counts before output selection, so a trace page does not hide
newer retry attempts. Large-history scan performance remains to be benchmarked.

## Verification

The initial integration test failed because the daemon lacked the operation.
Final wait/trace/daemon/CLI-security regression passed 38 cases in 51.19 seconds.
Three additional cancellation/invalid-client-deadline cases passed in 3.59 seconds.
Coverage includes broadcast any/all/quorum behavior, read-only timeout,
parent-turn versus callback completion, latest retry selection compared against
the Python implementation, notices, cancelled replies, and invalid quorums.
Rust unit tests passed (25 plus the parent-invoked subprocess helper), formatting,
Clippy with warnings denied and diff checks passed.

These checks used isolated native daemons and controlled provider fixtures. No
model inference, production installation/default switch or existing user agent
control occurred. Full CLI/API parity and the overall migration remain open.
