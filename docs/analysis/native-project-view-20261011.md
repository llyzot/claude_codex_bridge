# Native ProjectView and Windows sidebar checkpoint

The Rust daemon serves public API-2 `project_view` with schema version 1. Its
`view`/`cache` envelope is consumed by the existing Rust sidebar model. Native
commands can inspect the same projection:

```powershell
ccb-native --state-root C:\runtime\project\ccbd view
ccb-native --state-root C:\runtime\project\ccbd view --limit 32

# One-shot output without opening a terminal UI:
ccb-agent-sidebar --state-root C:\runtime\project\ccbd --native-bin C:\tools\ccb-rust\bin\ccb-native.exe --dump-json

# Interactive native view; helper defaults to ccb-native beside the sidebar:
ccb-agent-sidebar --state-root C:\runtime\project\ccbd --project-root C:\project --pane-window native
```

Agent status comes from native queues, runtime readiness, provider faults,
interactive-request counts, reload state and callback lineage. It does not
scrape or control an external pane. An unobserved external/manual worker is
not presented as an observed idle provider. The `native` window is a logical
group, with no fabricated physical pane IDs or focus state.

Communication rows distinguish executing a task, waiting for a child,
continuation execution, returning a reply, a blocked receiver and resolved
terminal outcomes. A completed Worker is not automatically a completed exchange.
Continuation replies are matched to their original logical message and producer,
including when the continuation has a different attempt ID. Recent retries are
grouped by message/Agent lineage. Preview text is bounded and terminal control
and bidirectional-formatting characters are removed. Approval contents, role
memory, environment values, endpoint tokens and provider protocol frames are
not included in the projection; task previews remain user-visible task text.

Projection does not acknowledge an inbox head, reconcile an execution, answer an
approval, finish a Job, or write a view snapshot. Cached committed-record indices
are borrowed without cloning the complete history on every lookup. Their first
load can still scan history. The response uses a stable content sequence and a
one-second TTL. It is a bounded status view, not an unlimited history browser:
candidate communications include recent business jobs, active/initial queued
work and waiting callback parents; output defaults to eight rows, up to 64.

`project_view_dismiss_comms {id}` records only a display preference in
`project-view.json`. It requires a terminal logical result with no unresolved
delivery. Jobs, messages, attempts, replies and mailbox events are preserved,
and dismissal survives restart. Native row cancellation capability is explicit.

On Windows, `--state-root` selects a native sidebar transport. It invokes the
trusted native command helper directly, so token ACL, generation and loopback
checks stay in one implementation. No Python or shell is used for these calls.
The helper has bounded output and wait time, and a failed mutation is never
automatically replayed. The current transport launches one helper per request;
its production CPU/latency cost still needs measurement.

Native sidebar cancellation and terminal dismissal use the selected service.
The quit-project action also targets that native service, instead of running a
legacy `ccb kill` from the account PATH. Focus, restart, recovery and config-UI
actions that are not integrated yet fail explicitly rather than invoking the
legacy implementation. Existing Unix-socket mode remains available. The fresh
native-prefix installer still installs only core/ask commands; sidebar installer
integration and an actual terminal/Herdr UI run remain separate work.

The runtime layout has Agent journals at `runtime-parent/agents` and controller
state at `runtime-parent/ccbd`. Independent services must use distinct runtime
parents. A test initially used a controller directory directly below the shared
OS temp root and saw old test journals; the fixture now isolates the entire
runtime parent. Startup and view projection reject foreign-project Job records.
The subsequent [Agent journal authority](native-agent-journal-authority-20261011.md)
checkpoint adds a shared-parent lock and persistent controller binding. Current
writers reject alternate controllers against the same Agent journals rather
than treating separate controller-local locks as sufficient isolation.

Validation uses disposable native services and fixture providers. It covers
stable/read-only projections, queue/running/callback/delivery/fault/approval
states, retries, dismissal and restart, bounded previews, committed-index reuse,
foreign records and native CLI/sidebar dumps. The sidebar's explicit opt-in
integration test exercises authenticated view/cancel/dismiss/stop and rejection
of legacy controls. Existing TestBackend rendering tests cover the terminal
layout, but no real Herdr session or model inference is claimed here.
