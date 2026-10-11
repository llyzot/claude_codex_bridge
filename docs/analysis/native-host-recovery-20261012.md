# Native-host display reattachment and stopped-server recovery

Two recovery paths now cover hosted Codex in native Herdr mode:

- Repeating `ccb herdr open` after the previously bound Codex display exits can
  attach a new display to the unchanged native host and thread. The former
  Windows process lifetime must have ended, the original owned pane/terminal and
  shell must match, and the pane must have no other agent or foreground process.
  The old mount row is archived and its replacement is journaled atomically.
  A missing inventory entry alone does not authorize a new launch.
- `ccb herdr open --recover-stopped` can recover a fully stopped, quiescent native
  host workflow. It checks the saved Codex session belongs to the recorded host,
  both host/server process lifetimes have ended, and the native history header
  matches its UUID and workspace. The new native daemon resumes that saved
  session; no terminal `resume` arguments are substituted for RPC state.

The original history remains intact. Missing history or a foreign session record
fails before starting Herdr. Existing unfinished jobs, mailboxes, callbacks and
provider-fault holds remain blockers. This command does not automatically stop a
running daemon, resolve uncertain work or reset a partially bound startup.

## Real Windows acceptance

[Evidence](native-host-recovery-20261012.json) continues the previous independent
Windows Terminal/pwsh project and named Herdr session, rather than starting fresh
conversations. It records two completed recovery epochs (`w1 → w2 → w3`).

After the first recovery, both native session identities were unchanged. A real
Pi task asked each agent to recall its previous answer without reading files or
being given that answer in the prompt. The automatic continuation returned:

```text
HERDR_CLOSED_UI_REVIEW_OK_4087
HEADLESS_HOST_4087 WORKER_NATIVE_MEMORY_OK
BOTH_NATIVE_SESSIONS_RESUMED_OK
```

Pi's normal formatting prefix was preserved. The callback completed through the
native Codex RPC and original Pi TUI paths.

The exact Codex display was then closed and `herdr open` repeated. Its display PID
changed from 34456 to 34128; the full host record, including native host/server
process lifetimes and thread ID, stayed unchanged. One attachment epoch was
archived, and reopening returned `running`.

For the second recovery, Herdr was stopped before the native daemon while the
reattached display was present. Recovery again reached `running` with the same
Pi session and Codex UUID. The new test processes were stopped afterward. No
production installation, account PATH, existing user workspace, credentials or
folder-trust setting was changed during this checkpoint.

## Validation and remaining limits

The reattachment test failed on the previous implementation with
`native_hosts_starting`; the hosted stopped-server test failed with the explicit
unsupported-operation guard before implementation. The first recovery selection
passed 14 cases, including existing terminal-mode recovery cases. Broader final
results and binary hashes are recorded in the progress ledger/evidence.

The attachment path only replaces a confirmed dead display on an unchanged live
host/thread. It does not take over foreign occupants, replay uncertain launches,
restart a host under a still-running Herdr server, or rebind a display after a
context clear. Cold recovery requires the prior daemon and Herdr server to have
stopped and all work to be quiescent. Broader active-task reconciliation, legacy
configuration migration and production/default promotion remain unfinished.

Final checks: the expanded host-recovery/open/native-host selection passed 39
cases in 190.43 seconds, including refusal to relaunch a still-live display whose
inventory entry disappeared. Rust tests passed (25 plus the parent-invoked helper),
formatting, Clippy with warnings denied and diff checks passed. Final release/bin
hashes and declared capabilities were verified; no full repository run is claimed.
