# Native Claude context controls — 2026-10-11

The Rust context-control workflow now includes `claude_stream`. It uses the
same UUID operation receipts, durable Jobs, busy/reply/callback fences, FIFO,
observation, cancellation and no-retry policy as Pi/Codex. It keeps the owned
Claude process alive and does not erase prior conversations or copy resource
trees. Production installation and existing user Agents remain unchanged.

## Contract verified on the installed CLI

Direct private-home probes of Claude Code **2.1.296** confirmed these details:

- A correlated `initialize` control response describes built-in commands,
  `session_state` and outstanding permission/dialog requests. Repeated
  initialization is supported, including after a local command finishes.
- `clear` and `compact` are advertised with `builtin: true`.
- `/clear` emits a UUID-correlated `conversation_reset`, then a new `system/init`,
  replayed user input, an attributed local-command result and a terminal command
  lifecycle event. The reset's **`new_conversation_id` is a UI identity and was
  different from the real subsequent `session_id`**. Only the latter is saved
  for native session resume.
- Empty `/compact` returns a successful local-command result without a
  `compact_boundary`. Rust reports an incomplete management Job instead of
  claiming context was compacted. The process remains usable.

The public [SDK command documentation](https://code.claude.com/docs/en/agent-sdk/slash-commands#commands-in-agent-sdk-sessions)
describes command discovery and warns that an unrecognized slash command can
become a model prompt. It also distinguishes a successful result without a
compaction boundary from actual compaction. Local stream evidence and the SDK's
`SDKConversationResetMessage` / `SDKCompactBoundaryMessage` types were used to
bind the implementation to concrete events rather than result text.

## Native completion checks

Before sending a command, Rust requires a supported version (currently 2.1.296
or newer), an idle control response, empty known pending-request lists, and one
unambiguous catalogue entry for the requested name with `builtin: true`.
Older/missing contracts and shadowed/duplicate command names fail without
sending user input. A declared non-idle/inherited-request state becomes a
provider fault so later tasks cannot silently proceed through that condition.
This command capability check happens after durable Job admission; a mixed-Agent
operation can therefore have different per-Agent execution outcomes even though
its initial controller preflight was atomic.

The input is the exact built-in command and a canonical client UUID. It skips
ordinary role prompt construction and role skill refresh. The normal CCB textual
prompt header would prevent slash-command dispatch, so correlation instead uses
that UUID across the replayed user frame, command lifecycle and result fields.
The result must identify the requested `local_command` and have no queued turns.

Successful clear requires a reset tied to that UUID, a different session ID seen
in initialization, replay, result and terminal lifecycle, then a correlated idle
control response with no pending prompts. It never binds to the display reset ID.
Compaction requires a manual boundary in the owned session after the command
starts, plus its attributed terminal result/lifecycle and idle verification.
Automatic boundaries, a repeated last boundary or a bare `success` cannot stand
in for manual compaction. Private result/error strings and model summaries are
not copied into management receipts.

Cancellation during preflight sends no interrupt or user prompt. Once sent, it
uses the existing Claude cancellation protocol. A queued-cancellation claim
cannot override an already observed reset, boundary or result. Contradictory
frames, another command starting in the managed session, and unowned resets
cause a provider fault and retain unresolved work. A reset outside management
also cannot silently change the context for a later normal Job. Callers use the
existing explicit provider recovery and Job reconciliation workflow; no ambiguous
operation is automatically repeated.

The same process-local role surface remains available after clear. Its old
verification metadata is removed when the session changes; the next ordinary
task follows the normal live-resource preparation path.

## Idle history flush before retirement

The immediate clear/reopen test exposed an intermittent failure: the saved ID
was correct, but the provider file contained only an 83-byte `mode` record.
Direct CLI resume reported `No conversation found`. Delaying stop by 300 ms
made three successive reproductions pass. A deterministic provider fixture that
persists only after stdin EOF then reproduced the same failure for both daemon
stop and native restart. The old retirement path killed the owned process before
its buffered transcript could finish writing.

Idle, healthy Claude providers now receive stdin EOF before stop, restart or
reload retirement. Native code observes process exit, drained stdout and the
owned process tree, with one shared one-second grace budget. It does not add a
fixed wait to context completion. Restart/reload remains tick-driven, so the
actor continues to answer queries during the grace period; protocol faults are
still observed before publishing a replacement. Restart/reload records expiry
before a forced retirement and never labels it a graceful exit. Unmount records
outcomes in its lifecycle snapshot; restart/reload records them in the operation
receipt. The lifecycle snapshot is replaced by the next mount.

Busy and quarantined providers retain the explicit forced-stop/recovery path.
This is an orderly-shutdown improvement, not a promise that an abrupt host crash,
forced kill or vendor shutdown exceeding the bound preserves every buffered
history entry. Previous histories are not deleted, rewritten or recreated.

## Validation scope

Disposable protocol fixtures cover both result/lifecycle orders, correct actual
session binding, positive/no-op compaction, old versions, builtin shadowing,
duplicate names, private rejection text, foreign reset/result identities,
missing local-command evidence, inherited permissions, preflight/running
cancellation, conflicting queue cancellation, role preparation bypass and
unmanaged resets.
Additional fixtures cover EOF-only persistence across stop, restart and reload,
a shared shutdown deadline, a responsive restart with forced fallback, and
unchanged busy-provider stop behavior.

The actual installed CLI was tested in fresh private homes with all model tools
disabled and no copied credentials. Clear kept the process identity; stopping
the native daemon immediately after clear and reopening it preserved a usable
session. Empty compaction was classified as not performed.

A separate actual-CLI test used a loopback-only Messages API fixture with a dummy
key. This fixture never forwards traffic and rejects non-fixture credentials.
The CLI itself performed compaction, emitted its native boundary/lifecycle,
kept the same process, and included a newly generated summary string in a later
request. This validates CLI protocol execution and history application, **not
Claude model quality or authenticated inference**. Local account status was
`loggedIn: false` / `authMethod: none`; no login or account changes were made.

Real authenticated Claude cancellation/error paths, interactive/Herdr UI,
concurrent remote clients, large history, journal retention and full native
default promotion remain outside this checkpoint. Unowned changes detected by
the current protocol are fenced; this is not a multi-controller coordination
or per-Agent authorization protocol.

Final evidence: 27 dedicated Claude-context cases are included in the final
87-case context/stream/control/restart/reload run (all passed, 170.67 seconds).
Earlier affected regressions passed 191 cases in 381.80 seconds before the idle
flush fix, then 174 cases in 306.70 seconds with orderly daemon shutdown before
the same treatment was extended to restart/reload. These are successive scopes,
not another full-suite claim. The final actual-CLI clear/restart/immediate-stop
resume and loopback compaction cases both passed in 10.65 seconds. The formerly
intermittent immediate-stop reproduction also passed three consecutive runs
without the temporary diagnostic delay after orderly shutdown was introduced.

The deterministic EOF-only persistence tests were observed failing against both
old stop and restart paths before the fix. Deadline/fallback tests verify that
three idle providers share one shutdown budget, active work keeps its forced
stop path, and reload/restart continue answering queries while retirement waits.
Rust tests: 11 passed; formatting, clippy with warnings denied and diff checks
passed. Curated native inventory: 724 collected, not fully executed. The last
complete native run remains the recorded 613-case baseline. Diagnostic-delay
instrumentation was removed; only reusable fixtures and regression cases remain.
