# Native task command parity

This checkpoint moves ordinary task submission, observation and queued
cancellation onto the Rust command path. The default installed entrypoint still
uses Python until the remaining migration gates pass.

## Submission and text transport

Native `ask` accepts the normal leading `--chain`, `--compact`, `--silence`,
`--artifact-request`, `--inline-request`, `--artifact-reply` and `--artifact-io`
options. Task ID, reply target and message mode are forwarded. The existing
native `--from`/project-ID extensions remain accepted, as does `from <sender>`
immediately after the target. Once message text starts, its tokens are preserved;
`--` explicitly separates literal message text from options.

Redirected UTF-8 stdin is appended to the message, with UTF-8 BOM handling and a
512 KiB input limit. Empty messages, unknown options, conflicting artifact modes
and oversized inline requests fail before admission. Reply-mode guidance matches
the existing CLI's compact/silent rules and explicit-output overrides. Submission
remains asynchronous; no waiting is added to the Lead's normal ask path.

The CLI requests automatic text transport. Above 4 KiB, or when explicitly forced,
the native authority stages a text artifact and its SHA256/size/reference together
with Job/Message/Attempt/mailbox admission in the existing journal. A rejected
admission rolls back the generated artifact. The provider receives an instruction
to read the full artifact, matching the existing transport contract; the full
request is not silently truncated. The public RPC frame remains bounded to 1 MiB.
Forced reply artifacts use the existing native completion-artifact path.

## Task control and observation

```powershell
ccb-native --state-root <runtime-root> ask --compact <agent> <message>
ccb-native --state-root <runtime-root> ask --artifact-io <agent> <message>
ccb-native --state-root <runtime-root> ask get <job-id>
ccb-native --state-root <runtime-root> cancel <job-id> [--silent]
ccb-native --state-root <runtime-root> retry <job-id>
ccb-native --state-root <runtime-root> watch <target> [--cursor <n>]
ccb-native --state-root <runtime-root> watch <target> --wait --timeout-ms <ms>
```

`ask cancel` is an alias for cancellation. Accepted/queued cancellation commits
intent, terminal state, mailbox abandonment and queue removal without sending a
provider prompt. Cancelling a middle request does not clear another Job's active
slot. Repeating cancellation does not duplicate the outcome. Running Pi/Codex/
Claude tasks retain their existing cooperative cancellation/evidence contract.

`watch` reports the logical result of a delegated request: it remains pending
while the callback is outstanding and uses the final review's status and reply
once available. `job_status` retains the initial physical Job status for diagnosis.
Thus an initial successful delegation cannot conceal a failed final review.

Observation is a snapshot by default. `--wait` pins the first resolved Job ID and
advances its event cursor. Expiry returns a JSON observation with
`observation_timed_out=true` and a nonzero CLI exit; it neither cancels nor
resubmits work. A zero millisecond deadline observes once. Transport failure
does not launch or replace a daemon. `get` and `pend` provide snapshot aliases.

## Verification and limits

The initial command loop reproduced six failures: leading options became target
names, stdin was ignored, long input was not admitted, queued cancellation was
missing and delegated work was prematurely terminal. Fixtures now cover ordinary
syntax, differential reply guidance, exact artifact contents and rollback,
middle-queue/idempotent/restart cancellation, running-provider receipts, explicit
retry, logical review outcome, cursor observation and timeout without replay.
Release counts are recorded in the progress JSON.

Final release validation passed 305 selected Rust contracts, including 20 task
CLI cases. Rust unit tests (7), formatting and Clippy with warnings denied passed;
repository bin artifacts match the tested release hashes.

This is not full CLI parity. Workgroup waits, role/worktree operations, Keeper/
reload, Herdr controls, remaining provider protocols, legacy recovery and native
installer/default routing remain migration work. Existing legacy Python test
failures are still recorded separately.
