# Idle provider lifetime recovery

New Windows provider faults now retain a `provider_scope` even when no task is
active. It records the original controller/provider PID and process creation
identity, Windows session, named Job scope, runtime identity and project/agent
binding. It does not fabricate a job or execution ID for an idle process.

The shared process-scope validator is also used by task execution proofs. Task
proof validation retains its execution/runtime/schema checks. Provider fault
loading additionally validates the provider-scope schema and its identity
against the fault record; conflicting simultaneous task/provider process
proofs are rejected. An invalid present proof never falls back to PID-only
recovery.

With no currently owned session, `provider-status` exposes
`recovery_observation`. Recovery checks both original process lifetimes and the
named Job scope. A live process reusing a PID with a different creation identity
is not the original provider and is never terminated by this observation. A
surviving scope member, unverified Windows namespace or unavailable observation
keeps recovery blocked. Clearing a fault still requires the exact fault ID and
the normal active-execution guard.

Existing task-level `recovery_scope` takes precedence where present. Legacy
records without either scope retain conservative PID-based handling rather than
being assigned invented ownership. Scope capture failures do not suppress the
original fault publication; such records remain limited by their available
evidence. Cross-Windows-session recovery is not claimed.

Tests cover all three providers after controller restart, simulated PID reuse
without killing a separate owned test process, live members in a controlled
named Job, a wrong Windows session, malformed scope fields, and legacy
PID-only records. The reuse and live-member scenarios deliberately construct
historical-record fixtures; they are not claims that the OS actually reused a
PID during the test. Cooperative journal integrity remains an assumption, not a
defense against arbitrary direct record tampering.

This is explicit recovery of local owned runtimes. It does not grant rights over
external Herdr processes, replay prior tasks, modify production installation or
start model inference. Verification results and release hashes are in the
adjacent evidence file.
