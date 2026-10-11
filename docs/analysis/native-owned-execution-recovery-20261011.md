# Windows owned execution recovery

This checkpoint recovers interrupted executions from durable ownership evidence.
It does not infer task completion from elapsed time, a missing heartbeat or a PID
alone. The preceding release reproduced a blocked old running Job after a real
daemon kill for Pi, Codex and Claude, preventing subsequent FIFO work.

## Evidence captured before provider input

Windows provider Jobs now have a fresh, collision-checked local name derived
from their scope UUID. Creation still atomically associates the provider with
the Job and restricts inherited handles to stdio. Before sending a prompt, the
actor commits a versioned `native_scope_proof` on the Job. It binds project,
Agent, provider, Job, execution, daemon and provider-runtime identities to the
named scope, Windows session and both process creation times. Creation times
are decimal strings so JSON clients cannot lose integer precision.

Microsoft documents named-object inspection and destruction after process/handle
closure in [Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects).
The recovery probe uses [OpenJobObjectW](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-openjobobjectw)
for scope accounting and [GetProcessTimes](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getprocesstimes)
to distinguish process identities when Windows reuses a PID.

## Recovery behavior

On startup and maintenance ticks, prior running slots are inspected before new
sessions are spawned. The daemon and provider identities must be confirmed gone;
the named Job must be absent or have zero live processes in the same Windows
session. Access failures, live processes, a different namespace, invalid bindings
and missing older proofs retain the slot. The API reports the blocking reason
and does not start a replacement for that Agent while the execution is fenced.

After closure is proven, a durable terminal snapshot is reused only when its
Job, Agent, runtime and execution bindings match. Otherwise the task becomes
`incomplete`, or `cancelled` when a cancellation intent was already committed.
The outcome explicitly records that work may have taken effect before the crash.
Recovery does not automatically retry that execution. Distinct queued tasks can
proceed, and the normal result broker can create a Lead continuation.

Cancellation policy survives this path, including silent replies. Job, snapshot,
attempt, mailbox, result and reconciliation audit updates use the existing
transaction journal. Repeating reconciliation of a terminal Job is a no-op.

A reply-delivery turn that lacks confirmed success retains its original mailbox
head and creates a recovery fault instead of automatically repeating the Lead's
work. Explicit `provider-recover` rechecks the saved scope before unblocking that
delivery. A confirmed successful delivery can consume its head directly.

```powershell
ccb-native --state-root <runtime-root> job-recovery <job-id>
ccb-native --state-root <runtime-root> job-reconcile <job-id>
```

The first command observes recovery eligibility. The second attempts the same
evidence-based reconciliation used by maintenance. Neither bypasses a live,
unknown or current owner.

## Verification and remaining scope

Fixtures kill actual owned daemons in temporary roots and read the sealed record
from the provider before it receives its first prompt. Tests cover three-provider
FIFO recovery, cancellation, missing and invalid proofs, a live owner, PID reuse,
live/empty named scopes, exact versus foreign terminal snapshots, uncertain reply
delivery across another restart, and a saved Pi Lead session automatically
reviewing the recovered Codex Worker outcome. These are protocol fixtures; no
new real-model inference is claimed. Release results are in the progress JSON.

Final validation passed 285 selected Rust contracts, including 21 recovery cases.
Rust unit tests (7), formatting and Clippy with warnings denied passed. The initial
three-provider crash loop failed against the preceding release because old
running slots remained occupied. New cancellation contracts verify terminal
cancelled outcomes without silent-policy loss or prompt replay; explicit legacy
fixtures without proof retain the original fence.

Older executions without the versioned proof remain fenced. Cross-Windows-session
inspection, Unix ownership/recovery and authoritative reconstruction from legacy
provider histories remain incomplete. Full Herdr/Keeper/config/CLI parity,
successful real Claude collaboration, performance/rollback checks, native default
installation and Python retirement are still migration requirements.
