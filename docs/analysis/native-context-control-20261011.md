# Native context controls — 2026-10-11

Rust now owns asynchronous `clear` and `compact` commands for its persistent
Pi RPC and Codex app-server runtimes. They use provider protocols directly and
keep the provider process alive. They do not type slash commands into a terminal,
submit a natural-language request to a model, erase provider histories, or copy
resource trees. The installed account default remains Python.

| Provider | Clear | Compact | Verification in this checkpoint |
| --- | --- | --- | --- |
| Pi 1.1.0 | `new_session`, then correlated `get_state` with a different session ID | Completed `compact` result, then same-session idle `get_state` | Real executable clear/restart and real-model compaction with subsequent memory check; fixtures cover cancellation, veto and failures |
| Codex CLI 0.162.1 | `thread/start`, then correlated idle `thread/read` with a new thread ID | Fresh turn plus matching context-compaction item lifecycle, terminal turn and idle history read | Real executable clear/restart and real-model compaction with subsequent memory check; fixtures cover cancellation and failures |
| Claude / other drivers | Not connected | Not connected | Explicitly blocked; no text-prompt or terminal fallback |

## Durable operations and usage

Use the repository's built native executable or a native-prefix installation:

```powershell
ccb-native --state-root <native-state-root> clear lead --dry-run
ccb-native --state-root <native-state-root> clear lead --wait --timeout-ms 15000
ccb-native --state-root <native-state-root> compact worker
ccb-native --state-root <native-state-root> context-status <operation-id>
```

The CLI generates a UUID before sending; use `--operation-id <uuid>` to supply
one explicitly. Request and observation errors include it. Accepted operations
record that ID, normalized selectors and per-Agent Job receipts in
`context-operations.jsonl`, atomically with Job/FIFO admission. Reusing the same
ID returns the original operation's current status, including after daemon
restart; a different action or target list cannot reuse the ID.

The API accepts `clear` / `project_clear_context`, `compact` /
`project_compact_context`, and `context-status`. Names, uniquely bound role IDs
and `all` work; omitted names mean `all`. Preflight covers every target before
admitting any Job. Unsupported, disabled, unavailable or busy targets block the
whole admission. Existing unfinished work, mailbox events, callback edges,
outstanding replies and provider faults are checked using the same authority
as reload/restart. Thus an idle Lead expecting a Worker reply cannot be cleared.
Provider execution after admission is not an all-or-nothing transaction across
Agents: inspect each result if one succeeds and another fails.

Each control is a reserved management Job, bound to the exact provider runtime
observed at admission. It bypasses role prompt preparation and never becomes an
ordinary model prompt. Existing FIFO execution admits later tasks behind it.
The control has normal Job history, cancellation intent, completion persistence,
owned-process evidence and crash reconciliation. Both automatic retries and
the general `retry` command are disabled for these Jobs. After inspecting an
uncertain outcome, a deliberate new management operation requires a new ID and
passes preflight again.

`--wait` only observes. Timeout returns `observation_timeout` and the receipt;
it does not cancel, kill a process or submit again. `context-status` aggregates
the authoritative Jobs: `pending` while any is nonterminal, `completed` only
when all completed, otherwise `failed` with per-Job outcomes. A Pi extension
veto returns a cancelled Job, not successful clearing. An accepted API receipt
does not claim the provider operation is complete.

## Provider evidence and interruption

Pi requires matching request and command identities. `new_session` must report
its cancellation outcome; a successful reset then needs an idle state with a
different session ID. A veto/error must leave the same idle session. Compaction
needs its native result shape and a subsequent same-session idle state. Neither
`agent_end`, `agent_settled`, an unrelated response, nor a bare command ACK is
sufficient. Compaction summary contents are not copied into controller receipts.

Codex records a fresh idle-thread baseline before dispatch. Compaction must use
a turn absent from that baseline, a matching started/completed
`contextCompaction` item, and terminal turn evidence reconciled against a
correlated `thread/read`. A delayed RPC ACK is supported. Old completed turns,
deprecated compaction-only notifications, or `{}` alone cannot finish the Job.
Cancellation requests `turn/interrupt` only for the observed current turn.
Pi cancellation uses `abort`; its installed source also aborts compaction.
Cancellation cannot undo a clear that already completed successfully.

Invalid or contradictory protocol evidence quarantines the provider and keeps
the active Job unresolved. Provider exit without confirmation yields incomplete;
it does not trigger an automatic replay. A daemon crash leaves the prior running
Job fenced. Existing `job-recovery` / `job-reconcile` requires owned-process
termination proof before resolving it as incomplete/cancelled. Inspect saved
provider state before a new operation: the old command may have taken effect.

## Empty contexts after clear

Real executable tests exposed that both providers may defer writing a newly
cleared conversation to disk. Resuming that nonexistent file/thread fails.
After a verified clear, Rust records an empty-context marker. It permits creation
of a fresh empty context only when the corresponding history is absent, and
records the prior identity in `recreated_from`. This preserves the cleared
state, not its transient provider ID. Existing history is resumed normally.

The marker is consumed in the same durable transaction as the next Job's
`job_started`, before provider input. The in-memory session also drops it, so a
later metadata write cannot restore permission. A started/uncertain task, missing
nonempty history, permissions error, corrupt existing file or arbitrary initial
session does not receive this exception. Broader empty-startup and legacy-history
recovery remain separate work. Provider-owned old histories are retained; Codex
old loaded threads are not explicitly unsubscribed by this change.

## Evidence and limits

Primary local protocol evidence: installed Pi `dist/modes/rpc/rpc-mode.js`,
`rpc-types.d.ts`, `core/agent-session.js` and `core/session-manager.js`; Codex
0.162.1 generated `ThreadCompactStartResponse`, `ContextCompactedNotification`
and thread/item schemas. The upstream [versioned app-server notes](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/app-server/README.md)
also warn that an unused new thread is not guaranteed to survive restart.

Tests use disposable services, real owned child processes and protocol fixtures.
The two explicit real-program cases used new private homes, copied no credentials,
sent no model turns, preserved the same process during clear, and restarted the
daemon successfully afterward. Pi's automatically created empty `auth.json` is
allowed; it contained no credentials. Two further explicitly selected real-model
tests used protected credential copies in temporary homes. They created two
small text turns, compacted with the native management command, confirmed the
same runtime identity, and correctly recalled a verification word on the next
turn. Pi used a private test setting of 64 recent tokens so the small transcript
could actually compact. These two cases passed in 41.10 seconds; the no-model
clear/restart cases passed in 6.99 seconds.

The affected context/Pi/Codex/recovery/restart/reload/view regression passed all
117 selected cases in 257.18 seconds. Two added tests then verified that the
empty-context marker is durably consumed while a new prompt is still running
(both passed in 3.72 seconds). There are 25 dedicated context fixture cases.
Rust unit tests: 11 passed; formatting, clippy with warnings denied and diff
checks passed. The full curated 697-case native inventory was collected, not
executed; the last complete run remains the recorded 613-case baseline.

Claude management, Herdr/original terminal UI integration, large-history limits,
broader empty-startup recovery, old loaded-thread cleanup and a retention policy
for context-operation receipts remain incomplete. The account installation and
production default were not changed.
