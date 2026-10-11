# Native provider restart — 2026-10-11

The Rust service now exposes `restart` and the sidebar-compatible
`project_restart_panes` API, plus durable `restart-status` queries. This restarts
owned persistent Pi RPC, Codex app-server and Claude stream-json processes.
It does not restart Herdr, replace terminal panes, clear a conversation or compact
context. The account installation/default and existing user Agents are unchanged.

## Usage

Use the built `ccb-native.exe` (or a native-prefix `ccb.exe`) with the selected
service root. These commands are examples; substitute the actual native root:

```powershell
ccb-native --state-root <native-state-root> restart worker --dry-run
ccb-native --state-root <native-state-root> restart worker --wait --timeout-ms 15000
ccb-native --state-root <native-state-root> restart-status
```

Names, uniquely bound role IDs and `all` are supported. No names means `all`.
`all` skips disabled and nonpersistent Agents; naming one explicitly rejects the
request. Any blocker among the selected providers rejects the whole restart.
Use `--operation-id <uuid>` when a caller needs an identity known before sending.
The CLI otherwise generates one and prints it in the receipt. Repeating the same
ID and normalized selectors returns the stored operation; it does not kill a
replacement process, even after the daemon itself restarts. Reusing an ID with
different selectors is rejected. A new intentional restart needs a new ID.

`restart-status <uuid>` queries that exact operation. Without an ID it returns the
last accepted restart. Blocked attempts and dry runs create no durable operation.
The wire response uses `reload` for the receipt because restart shares the
configuration retirement workflow; the record distinguishes `action: restart`.

## Guarantees and limits

- Unfinished jobs, pending mailbox events, open callback edges, outstanding
  reply-bearing messages and unresolved provider faults prevent retirement.
  An idle Lead expecting a Worker result is protected too. A missing/not-ready
  runtime or unsupported process-scope proof blocks the operation.
- The service commits the operation before touching a process and freezes
  affected admissions. Unaffected providers keep their runtime identities.
  It waits for owned-process exit, pipe closure and an empty owned process tree
  before publishing. The daemon PID and configuration fingerprint stay unchanged.
- Restart bypasses workspace materialization. It preserves dirty files, existing
  worktrees and bindings; it does not scan/copy resource snapshots or rebuild Git
  checkouts. Normal provider startup/resource discovery still occurs.
- Receipts follow `preparing → retiring → applied` or a recorded failure.
  `applied` confirms retirement and publication; it does **not** confirm that the
  replacement authenticated or is ready for a task. Inspect `provider-status`.
  If replacement startup fails, use the explicit provider recovery workflow.
- Session metadata/history is retained and ordinary configured resume behavior
  applies. A Pi session with a saved session file resumes that session. Ephemeral
  sessions or disabled resume cannot promise conversation continuity.
- `--wait` observes the receipt. Timeout returns `observation_timeout: true`
  and the operation identity; it neither cancels nor replays the operation.
  A lost transport response is ambiguous: query/retry the same ID, not a new one.
- Accepted restart records are retained in `runtime-controls.jsonl`, separate
  from the replaceable `reload.json` latest-change view. On explicit daemon
  recovery, an unfinished receipt becomes `interrupted`; it is not replayed.
  This journal has no retention/compaction policy yet.
- The native sidebar accepts preparing/retiring/applied responses and reports
  blocked/failed/unknown responses as errors. It does not automatically retry
  an ambiguous helper timeout; inspect `restart-status` before another action.

Validation uses real disposable Windows processes speaking provider fixture
protocols. This is not evidence of real model inference, credentials, original
provider terminal UI or live Herdr integration. Full native migration remains
incomplete, including clear/compact and production default promotion.

The 16 dedicated restart cases are included in the 79-case affected regression
run (all passed, 160.34 seconds). After final CLI error-identity and view-label
changes, the rebuilt release passed all 42 restart/view/CLI-security cases in
61.68 seconds. Rust unit tests: 11 passed; sidebar library/render tests: 64 passed;
native sidebar integration: 2 passed. Both crates passed formatting and clippy
with warnings denied. The 672-case native inventory was collected, not fully
rerun. Interrupted-receipt tests seed committed preparing/retiring records; they
do not establish process-crash coverage at every restart write boundary.
