# Native Codex control with a separately attached terminal

The experimental Rust `codex_app_server` driver now accepts `tui_host_root`, an
absolute directory for per-process host records. Each runtime creates a fresh
UUID subdirectory and starts an owned Codex app-server. The native scheduler uses
the existing thread/turn RPC driver over an authenticated loopback WebSocket;
`codex-tui-attach` opens the original Codex TUI on that same server and thread.
Task content never enters the terminal input queue in this mode.

This addresses the architectural cause of the previously observed prompt-to-shell
hazard. Herdr v0.9.0 source checks the foreground provider before queuing text and
a delayed Enter; its queued submission is not bound atomically to that provider's
process lifetime. An extra CCB preflight cannot close that window. The original
Herdr driver has not yet been replaced by the new transport.

## Runtime behavior

- The native host owns app-server's Windows process scope. Host death or stdin EOF
  retires the server. No reconnection, replay, global daemon takeover or local TUI
  fallback is implicit.
- A random token is stored with the existing native private-file ACL machinery.
  Only its hash enters the server command line. The TUI receives the token through
  a dedicated environment variable; host metadata and logs do not contain it.
- Frames and queues are bounded. RPC replies retain their original IDs. A
  successful correlated thread start/resume updates the attachable thread ID.
- Attaching verifies both process lifetimes, authenticates the endpoint, and uses
  the recorded working directory. If the server exits, the attachment closes its
  TUI. Closing the TUI alone leaves the controlled server and its work running.

Example native agent configuration:

```toml
[agents.worker]
model = "gpt-6.1-sol"
thinking = "low"
tui_host_root = 'C:\absolute\project-state\codex-hosts'

[agents.worker.thread_options]
sandbox = "read-only"
approvalPolicy = "never"
```

Start with the native `start` command, then obtain the host directory from
`providers/codex/worker/session.json` → `state.tui_host.path` and run:

```powershell
ccb-native codex-tui-attach 'C:\absolute\project-state\codex-hosts\<runtime UUID>'
```

The initial configuration supports a directly resolved Codex executable with
`app-server --stdio`; extra server startup arguments are explicitly rejected.
Thread/turn options and inherited provider configuration remain available.
The socket transport is experimental in Codex itself. This does not promote the
production runtime or complete Herdr startup/recovery integration.

## Evidence

The [live evidence](native-codex-tui-host-20261012.json) records real Pi RPC and
Codex models, native session IDs, job lineage and process identities:

- Codex returned `HOST_RESULT_1517` through RPC while its original terminal client
  was attached in Windows Terminal/pwsh.
- After a clean native-daemon restart, a fresh host resumed the same Codex thread.
- Pi delegated the next task, released its turn and automatically reviewed the
  returned result as `HOST_REVIEW_OK_2021`. The callback edge reached `done`.
- After a later task was admitted, the exact test TUI process was closed. The
  unchanged server completed it as `HOST_SURVIVES_UI_CLOSE_8633`.

These are no-tool arithmetic turns. They do not prove Codex shell-tool execution
or pixel-level TUI rendering. Only disposable test processes were controlled;
production installation, account PATH and existing workspaces were unchanged.

The test also exposed Pi's handling of a Win32 verbatim cwd. Pi incorporates cwd
into a session-folder name, turning `\\?\` into an illegal `?` component. The
installed-Pi regression failed with an exited process before the fix. Its process
now receives the existing portable Windows path representation; journal identity
paths are not rewritten. The regression then passed.

Seven host/Pi-path cases plus the existing Codex/Pi RPC selection passed (22 total).
Rust tests passed (25 plus the parent-invoked subprocess helper), Clippy with
warnings denied and formatting passed. No full repository regression is claimed.

Sources inspected: [Herdr v0.9.0 agent input implementation](https://github.com/herdrdev/herdr/blob/v0.9.0/src/app/api/agents.rs),
[official Codex app-server protocol](https://learn.chatgpt.com/docs/app-server),
and the installed Codex 0.162.1 command help. The undocumented stdio proxy did not
speak the custom socket's WebSocket handshake in the local probe; it was not used.
