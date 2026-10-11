# Native provider exit and Pi cancellation

This checkpoint changes the standalone Rust runtime. The installed/default CCB
remains Python; existing Herdr Workspaces and `codex_impl` are not controlled.

## Evidence and behavior

Ten owned Pi/Codex protocol tests reproduced daemon death on idle provider exit,
failure to continue the Agent FIFO after active process exit, loss of a buffered
valid result, and premature terminal failure before stdout reached EOF. All ten
failed against the preceding release. The fixtures run only in temporary roots
and invoke no model or existing Agent.

Pi and Codex now distinguish a live session, observed process exit, drained
stdout and a fully closed session. Their bounded readers send an explicit end
event after all preceding records. A verified correlated result has precedence
over exit fallback. Missing-result fallback requires both observed process exit
and drained stdout. A dead writer prevents new prompt admission; it does not
prove task completion. Live or uncertain processes are not replaced because an
observer timed out or a pipe closed.

An idle closed session leaves the daemon and other Agents available. Its owned
process is replaced only when the Agent has dispatchable mailbox work and the old process and
output stream are confirmed closed. The normal session binding rules still
apply: exact saved Pi session file/ID where available, and saved Codex thread
identity unless ephemeral/session resume is disabled. Previously uncertain
running jobs after daemon death remain fenced; this change does not replay them.

Dispatch availability includes an actual mailbox head, not just scheduler queue
depth. Ordinary result delivery materializes a receiver Job only when it starts;
the former queue-depth check missed a closed Lead with such a reply. This gate
is shared by Pi, Codex and Claude and refuses replacement while an old running
execution still owns the scheduler slot.

Pi cancellation now uses the same durable actor admission as Codex and Claude.
Job and cancellation event commit before provider control IO. Each active
execution validates immutable identity before synchronizing that intent. Abort
is sent once; repeated cancellation preserves its reply policy. Deadlines are
admitted after polling terminal evidence, and recorded before sending abort.
Provider acknowledgement alone is never terminal authority. Process-exit
fallback retains cancellation/deadline diagnostics, suppresses silent replies
and prevents automatic task retry. Natural provider completion keeps its status.

The lifetime suite also checks that a Worker exit automatically produces a
result-chain continuation into the same persistent Pi Lead, that cancellation
survives daemon death without replay, and that a stdout-holding descendant does
not cause premature release. The holder exits only when the test explicitly
releases it. Pi abort fixtures read the durable Job/event files at the moment
the control arrives to prove the ordering.

## Validation and limits

Use `CCB_TEST_KERNEL_BIN` and `CCB_TEST_NATIVE_BIN` to select the release artifacts
for native contract tests. The older admission, completion, control-plane,
kernel, lifecycle and record test harnesses now honor that selection instead of
silently testing debug artifacts. These are test-only selection overrides.

Final release validation passed all 224 selected Rust contracts, including 18
provider-lifetime cases. Rust unit tests (7), format check and Clippy with
warnings denied passed. Repository bin artifacts match the final release hashes.
Results are also recorded in `rust-core-refactor-progress.json`.
The new lifetime cases are protocol fixtures, not evidence of real-model
interruption. Existing installed Pi/Codex read-only handshakes may run as part of
the broader suite; real-model tests require a separate explicit opt-in.

Run the selected Rust contracts from the repository root with the release
artifacts (the Python executable is a test harness, not the runtime):

```powershell
./bin/build-ccb-kernel.ps1
$env:CCB_TEST_KERNEL_BIN = "$PWD/rust/target/release/ccb-kernel.exe"
$env:CCB_TEST_NATIVE_BIN = "$PWD/rust/target/release/ccb-native.exe"
$cases = @(rg --files test | Where-Object {
    $_ -match 'test_native_.*\.py$' -and
    $_ -notmatch 'test_native_(real_models|cli_provider_execution|cli_providers|cli_completion)\.py$'
})
python -m pytest @cases -q
```

Rust contract selection excludes three older Python provider test modules that
also happen to start with `test_native_`: `test_native_cli_completion.py`,
`test_native_cli_providers.py`, and `test_native_cli_provider_execution.py`.
An exploratory run including them produced 295 passes and 65 failures. Failures
were confined to the latter two modules: Windows symlink privilege failures,
missing POSIX fixture executables, obsolete Pi snapshot assertions, and older
home/environment projection expectations. They remain recorded compatibility
work, not a green whole-repository test result; Rust changes do not reinstate the
user-rejected plugin snapshot behavior to satisfy those historical assertions.

Still incomplete: provider startup/protocol-error isolation, explicit forced
termination when a live process loses transport, reconciliation of interrupted
running executions, full Keeper/Herdr/config/CLI/installer parity, successful
Claude model-backed collaboration, Unix process ownership, performance/rollback
verification, native default activation and transitional Python retirement.
