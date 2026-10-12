# Native active-job followup

Native CCB now exposes Codex same-turn steering through its owned app-server
session, including the hosted transport used by native Herdr composition:

```powershell
ccb-native --state-root <directory> followup <job-id> --message "additional instruction"
ccb-native --state-root <directory> followup-status <followup-id>
ccb-native --state-root <directory> trace <followup-id>
```

This supplements the current running job. It does not enqueue a new logical task,
start another provider turn, approve a tool request or complete the current job.
The scheduler slot must still point to the job, and the owned session must have
the exact runtime, execution instance, thread and active turn. Queued/terminal
jobs, pending cancellation/context controls, unbound turns and unsupported
providers are rejected. Pi, Claude and arbitrary terminal injection are not
implemented by this command; this checkpoint ports the legacy Codex mechanism.

The API admits a durable operation and returns immediately. Per-agent outbox
entries are sent in admission order, with at most one awaiting acknowledgement.
The Codex request uses `turn/steer`, `expectedTurnId` and `clientUserMessageId`.
The installed Codex 0.162.1 schema exporter was inspected to verify these fields.
Only an acknowledgement returning the expected turn ID becomes `injected`.
That status proves acceptance of the supplemental input, not that the task or
its requested edits have finished. Normal turn completion remains authoritative.

The CLI generates an operation UUID and observes the receipt for up to five
seconds. `--timeout-ms 0` only submits and reports the initial receipt; a status
other than `injected` produces a nonzero exit without cancelling the job. Inspect
`followup-status`, or repeat the exact job/message with the same
`--operation-id <uuid>` after a lost response. Reusing the UUID with different
content is rejected. New UUIDs represent deliberate new supplemental inputs.

The outbox commits `sending` before writing to the provider input channel. If
eight seconds elapse without the matching response, the turn disappears, or a
daemon restart loses the sending runtime, the receipt becomes `uncertain`.
Uncertain sends are never automatically replayed or rebound to a new turn.
There is an unavoidable crash window between the committed send intent and the
external write; that window is conservatively classified as uncertain too.
Unsent accepted operations may proceed only while their original binding still
exists. Provider rejection is reported without echoing raw provider error text.

The new `native-followups.jsonl` stores operation history separately from legacy
Python records. Public receipts and trace omit the message and its digest.
Accepted outbox records contain the message required for sending. Operators can
retire payloads and redundant transitions for terminal followups:

```powershell
ccb-native --state-root <directory> followup-compact --dry-run
ccb-native --state-root <directory> followup-compact
```

The preview reports bytes before/after, removed records and retained terminal
receipts. The live daemon performs the replacement through its writer journal;
restart recovery completes an interrupted replacement. Accepted and sending
operations retain their history, content and admission order. Terminal receipts,
including uncertain sends, retain their operation IDs, content digests and
bindings so status/trace and duplicate-request rejection still work after
restart. The command is idempotent. Foreign, unknown-version, unknown-status or
incomplete histories are rejected without replacement.

This is explicit history compaction, not secure erasure or an automatic age/size
policy. One receipt per terminal operation remains indefinitely to prevent old
operation IDs from becoming new sends. Provider transcripts and filesystem
backups are not modified.

Validation covers same-job result changes, acknowledgement versus completion,
FIFO ordering, rejection/mismatched replies, no-ack timeouts, cancellation,
receipt replay after restart and real daemon death after sending. Existing Codex,
host, trace and automatic callback tests are also selected. The installed Codex
host is additionally probed with a nonexistent turn in an empty temporary home;
this negative protocol test does not request model inference or copy credentials.
Exact results and binary hashes are recorded in the adjacent evidence file.

Subsequent [real-model acceptance](native-followup-live-20261012.md) verified
direct same-turn steering and automatic Pi review of a steered worker result.
Legacy Unix socket adoption, other-provider steering, automatic retention and
production default promotion remain unfinished. The installed production prefix
was not changed.
