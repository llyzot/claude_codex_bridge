# Rust provider fault isolation and fenced recovery

This checkpoint changes the standalone Rust runtime. The production installation,
default Python routing, existing Herdr Workspace and `codex_impl` remain unchanged.

## Failure boundary

The preceding release let an owned Pi/Codex/Claude spawn error or a rejected
initialization propagate through the authority tick and stop the whole daemon.
Twelve fixture cases reproduced those exits, active protocol failures, missing
fault visibility and malformed-frame handling gaps before the implementation.

The actor now records a provider-local `fault.json` and audits the active Job
before quarantining its session. The record binds project, Agent, provider,
fault UUID, daemon instance, runtime instance, PID and active execution where
available. It contains a phase, reason code and optional OS error kind; raw
provider frames, error strings and answers are excluded. A poll failure restores
the previous validated metadata view, preventing a partially rejected frame from
overwriting the session binding. Malformed JSON/non-object/oversized/read-error
frames report a transport fault instead of disappearing silently.

Faults block new prompts, submissions and retries for that Agent. Other Agents,
the authenticated control plane and daemon heartbeat continue. Preaccepted task
requests that have not reached a provider are failed with
`provider_prompt_sent=false`; they are not replayed into a new process. Ordinary
result-delivery heads remain in the receiver mailbox until recovery and verified
delivery. A failed Worker result still passes through the result broker and can
automatically resume the same persistent Pi Lead.

An active quarantined execution retains its scheduler slot. No failure result
is synthesized merely from the protocol error, a live PID, an observation timeout
or a disconnected pipe. Exit fallback requires observed owned-process exit and
drained output. A previously verified result retains precedence. A failure after
that boundary retains cancellation/silent-reply semantics. Interactive Codex
requests are retired and responses are rejected while quarantined.

Owned child cleanup now begins at process creation, including failures during
subsequent pipe/initialization setup. It remains limited to owned processes.
Quarantine itself does not terminate or restart a live process.

## API and native CLI

`ping` retains service status and adds `degraded` when a provider fault is blocked.
`provider-status` reports the safe fault identity and current owned process view.
`provider-recover` requires that exact fault ID, rechecks the durable record,
rejects active/uncertain executions and requires confirmed closure of an owned
session. For a restored record without an owned handle, a saved PID must be
proven stopped. PID reuse is handled conservatively. The recovery record commits
before the in-memory fault and closed handle are retired. A remaining bad
configuration can produce a new fault ID on the next authorized spawn attempt;
there is no automatic retry loop.

These commands target a private native runtime root; they do not operate a
focused Herdr session:

```powershell
ccb-native --state-root <private-root> provider-status <agent>
ccb-native --state-root <private-root> provider-recover <agent> <fault-id>
```

Blocked faults survive clean restart. A prior uncertain running Job after daemon
death remains fenced, even if its faulted provider PID is now stopped. Clearing
the fault does not substitute for reconciliation of that execution.

## Validation and remaining work

The protocol fixtures use temporary roots and no model calls. They cover spawn
failure, invalid initialization, active and malformed-frame faults, independent
healthy Agent work, explicit recovery, persisted blocked faults, preaccepted
FIFO failure without provider prompts, same-Pi-Lead callbacks, error-secret
exclusion, native CLI operations, changed durable fault rejection, daemon-death
fencing, and retention/delivery of ordinary replies to faulted Leads.

Release results are recorded in `rust-core-refactor-progress.json`. The selected
Rust contract command remains the one in `native-provider-lifetime-20261011.md`;
it includes the new fault module. Broader legacy Python provider tests remain
recorded compatibility work; this checkpoint does not reinstate plugin snapshots.

Final release validation passed all 251 selected Rust contracts, including 27
provider-fault cases. Rust unit tests (7), format check and Clippy with warnings
denied passed. The expanded fault suite's first release run exposed a test
observer error: a claimed mailbox head can be temporarily invisible while the
receiver Job is still running. The test now awaits that same Job's terminal
result before checking mailbox consumption. No process was restarted for that
observation. No remaining fault fixture process was observed after the final
suite finished, and repository bin hashes match the release artifacts.

Still incomplete: isolated forced termination of a live provider process tree,
reconciliation of interrupted running tasks, Keeper/Herdr/full config and CLI
parity, successful real Claude collaboration, native installer/default activation,
Unix process ownership, performance/rollback verification and Python retirement.
