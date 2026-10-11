# Native Herdr open composition — 2026-10-11

The experimental native CLI now composes the existing Rust namespace, Agent,
terminal-history and daemon components through `herdr open`. It supports Pi and
Codex terminal Agents in one workspace. This is not production/default promotion
or proof of live Herdr compatibility.

## Public entry point

Use a fresh native prefix containing the native `ccb`/`ask` aliases. The project
must already have `.ccb/ccb.config`; a minimal layout is `lead:pi; worker:codex`.

```powershell
ccb-native --project C:/test/project herdr open --herdr-exe C:/path/to/herdr.exe
```

Options are `--herdr-exe`, `--session` (also `--herdr-session`), `--no-attach`,
`--timeout-ms` (0–60000, default 10000), `--dry-run`, and `--options <json>`.
Without an explicit executable, discovery checks `CCB_HERDR_EXE`, PATH and the
standard Windows application directories. PowerShell functions are not executable
files; installations exposed only through a function need the actual EXE path.
The default session is derived from the project ID and differs from legacy
Python session names. This is not automatic migration of an existing installation.

`--dry-run` creates no state or processes and omits environment values. Normal
open starts a hidden background coordinator. The foreground waits only for the
requested observation interval; expiry returns an explicit pending phase and
coordinator process identity while the same coordinator continues. Repeated opens
reuse its live process, rather than starting another Agent batch. The coordinator
has a 60-second observation window; a still-pending result can later be resumed
with another open, using its recorded intents and bindings.

When both input and output are terminals and `--no-attach` is absent, the caller
focuses its recorded workspace and runs `herdr session attach <name>` in the
invoking terminal. It does not launch WezTerm or another terminal window. A
running daemon result does not by itself certify UI attachment. Actual foreground
attachment remains unverified in this checkpoint.

## Durable stages and routing

The opener journals the request hash, coordinator lifetime, topology, session
bindings and expected daemon configuration. Configuration/ownership conflicts are
rejected instead of replacing an active service. Bootstrap configuration passes
through bounded stdin; raw environment values are not copied into the open record.

1. Read explicit session-scoped server status. Start a hidden server only after
   confirmed absence and a committed launch permission. Transport errors do not
   imply absence. A lost server-start outcome is not blindly retried.
2. Materialize configured native workspaces, prepare the owned Herdr layout and
   mount original provider TUIs through the previously journaled Agent controller.
   Pi receives the generated event extension; source profiles remain live.
   Reopening does not rewrite unchanged generated extension/manifest files, so
   reconnecting cannot trigger a file watcher merely by touching identical content.
3. Resolve Pi's small current-state manifest and exact Windows process lifetime.
   If Herdr has no Pi session reference, journal and publish its verified native
   session path with `pane report-agent-session`, then re-observe it. This is
   cooperative CCB metadata, not an independent authentication channel. Lost
   acknowledgement can recover from the observed metadata; absence never permits
   blind replay of that report.
4. Bind Codex only to the native session UUID reported by Herdr. Discover the
   matching filename under its configured session root, then verify the actual
   header, session ID and workspace. Discovery visits at most 20,000 entries and
   four nested levels, does not follow symlink directories, and does not guess
   from the newest file. Positive bindings are saved and reused. Missing session
   metadata/history leaves startup pending; no warm-up model prompt is sent.
5. Start or reconnect the Rust daemon only when all bindings exist. Preserve its
   configuration fingerprint before launch, so a crash after daemon creation can
   reconnect to the same configuration. Busy bound Agents do not force relaunch.

Each Agent shell receives `CCB_AGENT_NAME` and `CCB_NATIVE_STATE_ROOT`; the latter
lets native CLI tools locate the daemon even after changing directories. Old
inherited token/endpoint variables are cleared. The native executable directory
is prepended to PATH. In an installed native prefix this selects its native
`ccb`/`ask` aliases. Development binaries without those aliases should be invoked
explicitly; this does not replace account-wide commands or shell functions.

The options JSON can override `command` with a Herdr argv array, `session`,
`transport_timeout_ms`, `history_roots` keyed by Agent, and explicit Codex
`bindings` using the terminal-history binding schema. An optional compiled
`config` is provided for advanced use and offline tests. Ordinary usage compiles
the project's existing compact/TOML/hybrid configuration. Headless protocol-only
options that have no terminal mapping are rejected; use explicit terminal
`startup_args`. Other providers and multi-workspace layouts are not yet supported.

## Verification and remaining work

The integration test runs the public CLI, background coordinator, actual native
daemon, generated Pi SDK adapter and native `ask` callback chain. Herdr and model
responses are offline fixtures with real Windows process identities. Cases cover
cold server startup surviving coordinator exit, prompt/review round trips,
immediate observation expiry, coordinator process death after Agent start,
delayed session metadata, conflicting daemon/configuration, scoped Pi reporting
including lost acknowledgements, PATH case handling and automatic caller/root
routing from outside the project directory.

The installed Herdr 0.9.0 static schema confirms the session-report request fields;
existing Python CLI composition supplied the server and foreground attach syntax.
No actual Herdr server, workspace or user Agent was controlled. Test results and
exact binaries are recorded in the progress JSON.

The broader opener/startup/security/transport/namespace run passed 100 cases in
125.80 seconds. After the extension-rewrite regression was reproduced and fixed,
the final release passed all 33 opener/Pi-terminal cases in 161.30 seconds,
including the opted-in actual Pi no-model probe. Rust unit tests passed 25 cases;
formatting and clippy with warnings denied passed. This is not a full-suite run.

Remaining acceptance includes actual Windows Terminal/pwsh foreground behavior,
fresh Codex session publication timing, real model completion, stopped-server
replacement and cross-server recovery, explicit resolution of uncertain startup
permissions, original Claude terminals, broader configuration parity and measured
startup/idle performance. Negative history discovery can repeat while waiting;
very large homes should use an explicit binding. The native provider-specific
active cancellation/context/retirement limits from earlier checkpoints remain.
Production installation and default routing are unchanged.

The subsequent [real Windows validation](native-herdr-live-20261011.md) found that
fresh Codex sessions can have no persisted rollout before their first real input.
The current runtime can therefore start a process-bound deferred Codex driver and
bind its history using that input's unique structured request/turn evidence. This
supersedes the earlier requirement to keep all fresh sessions pending at step 4.
That validation also records real automatic Pi review after a 35-second Worker
task, controller restart recovery, and the remaining shell-helper limitation.
