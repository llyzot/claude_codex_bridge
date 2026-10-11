# Hosted Codex context clear and display following

Native `clear worker` now persists a new hosted Codex thread before reporting
success. The context controller names the new thread, waits for the correlated
reply, rereads its idle state and verifies its history file exists. The old
history is retained. This fixes the installed-Codex regression where clear
completed but its empty thread could neither be displayed nor resumed unchanged.

The attach supervisor follows the native controller's published session record.
It requires the same host/server identity and agreement between the host manifest
and controller thread ID, then replaces only its own TUI child with a display of
the new thread. Publication occurs before visual following, which is asynchronous.
There is no terminal prompt injection or model warm-up.

Each display publishes a process-lifetime proof under its attachment UUID. On a
subsequent `herdr open`, the mount controller verifies that proof, the original
display's exit, the live supervisor, the unchanged host, the new foreground
process and the owned pane before updating its binding. A forged process proof
is rejected. Ordinary model work does not cause display replacement.

Herdr can report an attached Codex display as working during initialization or
model activity. Hosted attachment readiness is now distinct from terminal input
readiness. Reopening a fully bound workflow does not wait for either agent's
current model turn to end and does not relaunch its display.

## Real acceptance

[The evidence file](native-host-context-switch-20261012.json) records the existing
independent Windows Terminal/pwsh Herdr test session:

- Clear changed the native thread from
  `01a12bf9-cc30-7b11-a395-6ab02a4ff10e` to
  `01a12c14-a64a-7220-b73e-c07593f95ef6`, retaining host PID 44404.
- The supervisor automatically changed its TUI child from PID 45268 to 30152;
  the recorded display epoch advanced to 1 and targeted the new UUID. Herdr's
  terminal output showed the fresh Codex prompt.
- A subsequent Pi → Codex → Pi chain returned `CLEARED_HOST_REVIEW_OK_5183`.
  The exact Worker job marker is present in the new native history and absent
  from the retained old history. Its completion decision names the new UUID.
- After stopping and recovering the workflow, the cleared thread UUID remained
  unchanged and the workflow reached `running`.

All arithmetic turns used no Worker tools. Terminal text and process identities
were checked; no pixel-level UI acceptance or shell-helper fix is claimed.
The test daemon, Herdr server and recorded provider/display process lifetimes
ended. Production installation and original user workspaces were unchanged.

## Regression and limits

The installed-Codex clear test failed before the persistence fix; the busy-display
open test failed before the readiness fix. Coverage includes immediate cold
resume after clear, real native RPC control, exact display proof adoption, forged
proof refusal and no extra launch during a busy workflow. The progress ledger
records the final selected-suite counts and release hashes.

Visual following applies to supervised native-host attachments. Herdr's saved
mount binding is reconciled on `herdr open`; it is not continuously rewritten by
the provider thread. Direct, concurrent human TUI submissions or independently
created TUI threads are not serialized with CCB context controls. Host restart
while Herdr remains alive, broader interactive coordination, Claude TUI support
and default/production promotion remain open.

Final verification: 68 selected native-host/context/recovery/open cases passed
in 273.26 seconds. Rust unit tests passed (25 plus the parent-invoked helper);
formatting, Clippy with warnings denied, diff checks and final release/bin hashes
passed. This was not a full repository regression run.
