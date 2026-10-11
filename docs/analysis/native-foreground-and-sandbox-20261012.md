# Foreground startup and sandbox diagnosis — 2026-10-12

The public native `ccb herdr open` foreground branch was exercised from Windows
Terminal using the user's Scoop pwsh installation. It launched the Herdr attach
client as a child of `ccb.exe`. This found and fixed a Pi startup interoperability
problem that the earlier headless-plus-manual-attach test had not exposed.

## Pi lifecycle-hook readiness and initial name recovery

Herdr reported the Pi process as idle, with its native session and
`screen_detection_skipped: true`. `agent explain` identified
`full_lifecycle_hook_authority`. In this state, `interactive_ready` was absent.
The Pi process remained alive, but its initial reserved launch name disappeared.
The native controller could therefore neither bind nor dispatch to it.

The installed Herdr Pi integration reports lifecycle state from Pi SDK events.
The controller now accepts the integration-backed idle/done state for Pi when
screen detection is skipped and the session source is exactly `herdr:pi`.
An explicit false readiness value, blocked state or another source is not an
override. This is input readiness only; task completion still needs correlated
Pi settled events.

Initial name recovery is limited to a launch that was already recorded as sent,
has never obtained a process binding, has no currently assigned name, and is no
longer launch-pending. It requires the CCB native Pi bridge to appear in the
recorded launch arguments. The controller verifies its owner, native session,
workspace and exact Windows process lifetime against the original owned pane and
terminal. It then commits one name-repair permission before `agent rename` and
rechecks the process. It never launches another Pi process, takes over a different
name, repairs a later lost bound Agent automatically, or repeats an uncertain
rename. Read-only observation does not perform this repair.

For a verified native Pi bridge, the initial Herdr ready wait is capped at four
seconds, within the requested startup ceiling, with a transport deadline one
second longer. The coordinator continues observing/recovering the existing
process. Other provider/primitive launch deadlines are unchanged.

The failing fixture reproduced the live missing-name/hook-authority state before
the fix. Positive, lost-acknowledgement, foreign-owner and missing-bridge checks
cover the repair boundaries. Live recovery restored the original Pi PID `25812`
and its unchanged creation time, brought the Rust daemon online, and completed a
request containing `FOREGROUND_READY_OK`. The attached Herdr client was a child of
the actual public `ccb.exe ... herdr open` command. See
[the foreground evidence](native-herdr-foreground-20261011.json).

The first test client was explicitly closed to update the disposable native
prefix, leaving its server and providers alive. At final cleanup, the named test
server and daemon were stopped and the recorded test processes were gone. Stopping
the server severed the foreground client and produced exit code 1; normal keyboard
detach was not tested. This does not claim pixel-level UI validation.

## Independent Codex sandbox failure

A deterministic, no-model reproducer outside CCB and Herdr was:

```powershell
codex sandbox --include-managed-config --permission-profile :read-only -- C:/Windows/System32/cmd.exe /d /c echo CCB_SANDBOX_PROBE
```

It exited 1 with `helper_unknown_error: setup refresh had errors`. The
[official Windows sandbox documentation](https://learn.chatgpt.com/docs/windows/windows-sandbox)
describes the native sandbox and its diagnostic command surface. The test kept
the configured `elevated` implementation and explicit read-only profile.

The setup log identifies a fatal runtime read/execute ACL validation failure for
the desktop app's `runtimes/cua_node/.../bin/node_repl.exe`: Windows sharing error
32. Multiple existing desktop `node_repl.exe` processes mapped that executable.
The separate profile-directory error in the log was explicitly nonfatal; the
runtime executable validation populated the fatal error list.

The same failure persisted when the probe temporarily disabled the configured
node_repl MCP, replaced its disabled command, removed desktop CODEX environment
variables, removed desktop runtime PATH entries, or disabled the two computer-use
plugin entries for that invocation. No global configuration was changed by these
overrides. An intentionally invalid sandbox implementation was rejected in both
CLI option positions, confirming that overrides were being parsed.

`codex doctor --json` also reported sandbox provisioning failure and a missing
`CODEX_WINDOWS_REGISTERED_CORE` variable for the optional node_repl MCP. The
redacted doctor report remains in the private trial directory rather than being
committed wholesale. No credentials or sandbox secret files were read.

This isolates the observed failure from CCB's dispatcher, Herdr and pwsh command
syntax. It does not prove how permanently to repair the upstream setup behavior.
Stopping shared desktop tool processes would affect other chats, so separate human
approval was requested; no approval has been received and none were stopped.
Sandbox mode, filesystem/network restrictions and approval policy were not
weakened. The broader Rust migration remains active.
