# Quiescent native backend restart with Herdr retained

Native `herdr open` can now rebuild a stopped Rust daemon before reconnecting its
Codex displays, while keeping the existing Herdr server, namespace and Pi TUI.
Previously, it tried to reattach the dead Codex display before recreating the
host that display needed and failed with `Codex host unavailable`.

The opener reconstructs and verifies the same daemon configuration, checks the
stopped ownership and quiescent job/delivery state, validates the saved native
Codex history, and independently observes each retained terminal binding. It
records a restart intent, starts/observes the native backend through the existing
startup controller, and reattaches a display only after the former host/server
lifetimes ended and the replacement resumes the same thread and workspace.
Observation expiry does not reset the startup permission or replay a task.

Missing history, a changed live Pi session, unfinished jobs, pending deliveries
or provider faults prevent this automatic path. This is a quiescent restart
workflow, not transparent survival of an in-flight model execution. Partially
bound/uncertain startup and broader running-task reconciliation remain open.

## Real Windows acceptance

[The evidence file](native-background-restart-20261012.json) records the existing
independent Windows Terminal/pwsh test session. Only the Rust daemon was stopped
for the restart test; Herdr and Pi remained alive.

- Herdr's full process identity, workspace and Pi's full process binding stayed
  unchanged.
- The daemon changed from PID 20992 to 50936 and the Codex host from 53780 to
  60588, while the native Codex thread UUID stayed unchanged.
- The same Pi process submitted a new Codex task through the replacement backend
  and automatically reviewed its result as `BACKEND_RESTART_REVIEW_OK_6557`.
- Test processes were stopped afterward. Production installation and original
  user workspaces were not changed.

The arithmetic test used no Worker tools; it does not resolve the separately
recorded Codex sandbox helper problem or establish full interactive UI parity.

## Startup identity and diagnostics

Startup intents previously retained only a PID. A controlled regression pointed
an old intent at a live helper with a different creation lifetime: the old code
waited on that unrelated process instead of starting the daemon. New Windows
intents save PID, creation time and Windows session, and both adoption and waiting
check that tuple. The unrelated helper remains untouched. Legacy intents without
this evidence remain conservative while their PID is observable.

A real early recovery attempt encountered a stale startup record and failed
before a new ready endpoint appeared. PID reuse was not captured at that instant;
the controlled regression establishes the underlying defect independently.

One intermediate fixture run reported an unobserved Herdr read after display
naming. The transport now includes its bounded failure category, exit code and
elapsed time in that diagnostic, without exposing raw command output. No mutation
retry was added. A separate Windows locale decoding failure in test teardown was
fixed by explicitly reading the UTF-8 records. Final regression results and
binary hashes are recorded in the progress ledger/evidence.

Final verification: the broad startup/host-recovery/legacy-recovery/open selection
passed 60 cases in 299.67 seconds. After the final backend lifetime-gate change,
six focused restart/PID-reuse cases passed in 49.10 seconds. Rust tests passed
(25 plus the parent-invoked subprocess helper), formatting, Clippy with warnings
denied, diff checks and release/bin hashes passed. This was not a full repository
regression run.
