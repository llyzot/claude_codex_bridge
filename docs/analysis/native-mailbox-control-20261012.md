# Native inbox and explicit acknowledgement

The native CLI now supports configured-agent inbox inspection and explicit FIFO
acknowledgement:

```powershell
ccb-native --state-root <directory> inbox --detail lead
ccb-native --state-root <directory> ack lead <inbound-event-id>
```

`inbox lead` returns the pending count, enriched head and mailbox summary;
`--detail` additionally returns pending items in admission order. Existing native
`events` remains available and includes the latest revision of terminal events.
Only the pending head includes reply text/artifact metadata; subsequent items use
previews. Reply text is capped at 32,768 characters with explicit size/truncation
fields, and CLI output escapes terminal formatting controls. Artifact bodies are
not read. This is not a paginated API: large retained histories still incur the
existing `events` collection cost and transport size limits.

Queries never retire stale events or repair mailbox summaries. Actual pending
events supply the counts; missing, erroneous or stale cached summaries are
reported. Missing source records are visible as `lineage_missing`; known foreign
project jobs are rejected. This deliberately differs from the legacy detailed
inbox path, which can discard stale heads while reading.

Acknowledgement requires the FIFO head. Raw task replies require matching reply,
attempt, message and project-scoped job lineage. A reply carrying a delivery job
reference is owned by automatic delivery and cannot be manually acknowledged.
Any active scheduler slot or another event's acquired mailbox lease also blocks
acknowledgement. A stale task request requires both its attempt and job to be
terminal; missing evidence is rejected. Queued cancelled requests are abandoned;
other proven terminal requests are consumed. No acknowledgement clears a
scheduler execution slot or declares a running task complete.

The event transition and a small `mailbox-acks.jsonl` receipt commit in one store
transaction. The receipt references the reply instead of duplicating its body.
Repeating an explicit agent/event ID returns `already_acknowledged: true`, with
the current next head, even after daemon restart. It never consumes that next
head. The CLI resolves an omitted event ID before sending the mutation and shows
that exact ID if the response is not observed. A direct API caller omitting the
ID on each call intentionally acknowledges the current head on each call; such
calls do not have replay identity. Retry with the exact ID after uncertainty.

Validation uses isolated native daemons and controlled worker completions. It
covers FIFO refusal, restart/lost-response replay, active task and automatic
delivery protection, foreign leases, terminal stale requests, missing/foreign
lineage, read-only mailbox files, CLI validation/escaping and Python item-summary
equivalence. Counts and binary hashes are in the adjacent JSON evidence file.

No real model inference or production installation/default-runtime change was
performed for this checkpoint. Public `mailbox-head` retains its existing native
claimable-head semantics; full command/API compatibility and Rust default
promotion remain separate work.
