# Windows native Worker process-tree termination

The user has authorized closing the existing Workspace and `codex_impl` when
needed for this migration. Their continued operation is no longer a migration
constraint. This checkpoint exercises owned test runtimes; production controls
are recorded separately when actually performed.

## Ownership from process creation

The Pi/Codex/Claude drivers now create each Windows provider in its own anonymous
Job Object. `CreateProcessW` receives both a Job list and an explicit list of the
three child stdio handles. Association occurs as part of creation, so a provider
cannot start an untracked descendant before the authority attaches it. The
provider Job nests under the existing daemon Job, and its handle is not inherited.
This uses the documented [process attributes](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute)
and [nested Job semantics](https://learn.microsoft.com/en-us/windows/win32/procthread/nested-jobs).

The native launcher preserves UTF-16 arguments, environment values and native
drive-directory environment entries. It applies CRT argument quoting, keeps
parent pipe ends private, uses null stderr, and does not open a console window.
Executable lookup follows the relevant ordering in the
[Rust Windows implementation](https://raw.githubusercontent.com/rust-lang/rust/1.98.0/library/std/src/sys/process/windows.rs):
configured child PATH, application/system directories, then parent PATH.
It does not implicitly search the project's working directory. Explicit paths
remain caller-relative. Batch scripts are rejected; native discovery already
selects Node plus Pi's JavaScript entry and native Codex/Claude executables.
This component implements the fixed stdio contract used by these three drivers,
not every option exposed by a general-purpose `std::process::Command`.

The process scope has a fresh UUID and queries its actual active process count.
A missing-result fallback and closed-session transition require parent exit,
drained stdout and an empty Job. Descendants with null stdout therefore still
prevent premature completion. A fully verified provider result retains its
normal precedence. Windows provides the accounting and termination operations
through [QueryInformationJobObject](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-queryinformationjobobject)
and [TerminateJobObject](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-terminatejobobject).

## Control and recovery

`provider-status` includes the owned scope and current Job/execution binding.
`provider-terminate` validates runtime instance, PID, scope UUID, Job ID and
execution instance before acting. Job/Event and fault intent commit before OS
termination. Repeating the same request retains the termination ID and can retry
the same owned OS operation; a different binding or reply policy is rejected.
The operation uses retained process-tree handles, never an arbitrary caller PID.

```powershell
ccb-native --state-root <runtime-root> provider-status <agent>
ccb-native --state-root <runtime-root> provider-terminate <agent> [--silent]
```

The response acknowledges the request, not completion. Completion waits for
the owned tree to close and records `provider_terminated` with the empty-scope
authority. Silent policy suppresses replies and cancellation prevents automatic
retry. A natural verified result that won the race keeps its original status.
The Agent stays faulted until explicit identity-fenced `provider-recover`;
faulted callers cannot dispatch additional work. Termination intent persists
across restart, while uncertain prior running executions still require separate
reconciliation.

## Validation boundaries

The Windows tests create real child/grandchild processes inside temporary roots.
They cover a live parent and a parent that exits while silent descendants remain,
foreign/stale identities, repeated termination, silent policy, durable intent,
an already running sibling provider retaining its PID and scope, idle CLI
termination, restart/recovery and Unicode/quote/backslash/PATH handling.
Release results are recorded in `rust-core-refactor-progress.json`.

Final validation passed 264 selected Rust contracts, including 13 Windows
termination cases. Rust unit tests (7), formatting and Clippy with warnings
denied passed. The process lookup regression first failed with a project-local
namesake executable and then passed with the corrected search order. Mailbox
recovery tests now delegate from a healthy idle Lead before inducing its fault;
they verify retention and recovery without bypassing the new caller fence.

Non-Windows tree termination is explicitly unsupported; its prior parent-process
contract remains. This checkpoint does not prove Unix ownership, real-model tool
termination, Herdr attachment, full interrupted-task reconciliation or default
native installation. Those remain migration gates.
