# Owned provider transport closure

Pi/Codex/Claude native drivers previously recorded output EOF and lowered
readiness, but a still-live provider root could remain without a durable fault.
New submissions could consequently reach a worker whose output connection was
already unusable. The isolated Pi regression reproduced missing fault publication
before this change.

The normal poll paths now raise a typed transport-closed fault after handling
already validated completion decisions and confirmed full process-tree exit.
Pi and Codex also report an already observed input-channel closure. The durable
fault keeps the existing `provider_protocol_error` reason and adds a validated
optional `transport_closed` direction (`stdout`/`stdin`). No raw provider frame
or exception text is persisted for this classification.

The existing quarantine/recovery lifecycle then applies:

- New admissions to the faulted provider are rejected.
- A dispatched task is not declared finished from EOF while its owned process
  tree may still be executing. Its active slot and pending FIFO remain intact.
- Once exit is confirmed, unresolved active work fails through the established
  protocol-fault path. Preaccepted tasks that were never sent fail with
  `provider_prompt_sent: false`, rather than being replayed.
- An ordinary result-delivery mailbox head remains pending for the receiver;
  it is delivered once after explicit recovery.
- Recovery requires the matching fault identity and proof that the relevant
  execution/process is no longer live. EOF itself neither kills nor restarts it.

A result already validated by the provider protocol remains completed even
when EOF follows immediately. Status-unknown output is not promoted to success.
Intentional provider termination and Claude's dedicated idle-shutdown path keep
their existing handling. A regression exposed the restart path polling after
intentional stdin EOF; Claude now records idle-shutdown intent and routes later
polls through its dedicated drain/flush path, so expected EOF is not quarantined.
No daemon-side observation timeout is used as evidence
that work has stopped.

The regressions use base Python interpreter fixtures because the Windows venv
redirector can retain inherited stdout handles and mask actual pipe EOF. Idle
injection waits for observed protocol initialization, not merely permission to
queue input; Claude can accept input before its init frame. Scenarios cover all
three providers: idle fault/recovery, active unknown work plus queued tasks,
completed result followed by EOF, adjacent verified-result/EOF frames, persisted
fault after daemon restart, and one-time receiver mailbox delivery after recovery.

This does not detect every silent hang behind a still-open inherited pipe or
complete general idle-fault recovery under PID reuse/cross-session migration.
It uses the existing explicit recovery policy rather than introducing automatic
task restart. Validation counts and release hashes are in the adjacent evidence
file. Tests use disposable controllers and controlled provider processes; no
production installation or account configuration was changed.
