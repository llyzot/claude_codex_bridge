# Herdr open with native Codex protocol input

Native `ccb herdr open` now supports a Codex agent configured with
`tui_host_root`. Pi stays in its original Herdr TUI, while the Codex TUI attaches
to the owned app-server used by Rust's existing RPC driver. Codex task content is
never submitted through Herdr terminal input in this mode.

## Startup sequence

1. Create the owned namespace and launch Pi with its native settlement bridge.
2. Bind Pi's verified session and start the Rust daemon, including the native
   Codex host. The Codex pane remains at its shell during this phase.
3. Once the host publishes a matching runtime/process/thread record, journal one
   UI launch permission and run the native attach helper in the PowerShell pane.
   This command only starts the display; it does not contain a task prompt.
4. Verify the detected Codex foreground argv targets the recorded endpoint and
   thread, then assign its reserved Herdr name. Lost acknowledgements are observed
   without repeating launch or rename.

The daemon's task readiness and the terminal's folder-trust dialog are separate.
Herdr reports manually started Codex without `interactive_ready`; native-host
mounting relies on the owned host and exact attachment argv. `running` therefore
does not assert that a pending UI trust dialog has been answered. Normal scoped
folder-trust decisions were accepted for the two new test repositories.

## Real defect found during integration

Codex initially returned a fresh thread ID and future rollout path, but had not
created that file. The TUI's `resume` failed with `no rollout found`. A no-model
probe showed that the native `thread/name/set` method persists the empty thread.
For a new durable hosted thread, Rust now waits for that acknowledgement and
checks the history exists before publishing session readiness. There is no model
warm-up request or fabricated history file. Ephemeral threads are rejected for
Herdr attachment.

## Actual acceptance

[Machine-readable evidence](native-herdr-codex-host-20261012.json) records the
independent Windows Terminal/pwsh session, process lifetimes and job lineage.

- Real Pi submitted a Codex child task and ended its initial turn. Codex's result
  returned automatically; the same Pi TUI replied `HERDR_HOST_REVIEW_OK_3127`.
- Herdr's Codex pane showed that task and `HERDR_HOST_WORKER_3127` in the original
  Codex interface, read through Herdr's terminal output API.
- A second Pi chain was submitted and the exact Codex TUI process was closed.
  The native server still completed the child; Pi automatically replied
  `HERDR_CLOSED_UI_REVIEW_OK_4087` while the Worker pane was back at PowerShell.
- Both callback edges reached `done`. No Worker prompt was sent to that shell.

The arithmetic tasks used no model tools. This does not resolve the separately
recorded Codex shell-helper issue or constitute pixel-level UI inspection.
Test processes were stopped; native conversations and evidence files remain.
The production installation and existing user workspaces were unchanged.

## Configuration and remaining work

Use the native-host configuration shown in
[the host checkpoint](native-codex-tui-host-20261012.md), then `ccb herdr open`.
Use `thread_options` for Codex sandbox/approval settings; terminal `startup_args`
are not silently translated. The native host currently requires PowerShell panes.

This mode remains explicit and experimental. Existing terminal-mode records are
not silently migrated. Hosted stopped-server recovery is explicitly rejected;
closing or replacing a previously bound display does not yet automatically
reattach it on a later `herdr open`. Context-clear display rebinding, other native
server launch arguments, role/UI permission interactions and final default
promotion remain open. The general Rust migration is not complete.

## Regression checks

The functional release passed 58 selected opener/mount/host/Codex RPC cases in
241.02 seconds. After the final preflight and capability changes, four focused
cold-open/lost-acknowledgement/invalid-host cases passed in 19.37 seconds. Rust
unit tests passed (25 plus the parent-invoked subprocess helper); formatting and
Clippy with warnings denied passed. Final repository binaries match the release
hashes recorded in the evidence file. This is not a full repository test run.
