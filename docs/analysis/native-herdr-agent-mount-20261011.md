# Native Herdr provider mount — 2026-10-11

## Namespace retirement update — 2026-10-12

`herdr-namespace` accepts `action: "retire"` with the original namespace request.
It requires the daemon and Keeper to be stopped, excludes concurrent open/start
operations, and verifies the stored plan plus live workspace/pane ownership.
Foreign or untracked panes prevent closure. A missing namespace creates no state.

One close permission is journaled before sending `workspace close`. Later calls
observe the same operation: an absent workspace completes retirement, whereas
an uncertain close with a still-present workspace remains `retiring` without
resending. A reappeared retired ID is rejected. This closes only the recorded
workspace; the server and unrelated workspaces are preserved. Namespace records
and provider histories remain available for lifecycle recovery.

This is the low-level close operation. Public `ccb kill` orchestration and
reopening a deliberately retired namespace with verified session continuity
remain to be connected. Ordinary preparation refuses retired records instead of
silently creating fresh sessions. The material below records earlier checkpoints.

Rust now has an internal `herdr-agents` controller for launching original Pi,
Codex and Claude terminal programs into a previously prepared native namespace.
It is a prerequisite for native `herdr open`, not a completed replacement for
that command. Its response explicitly reports `daemon_connected: false` and
`task_completion_supported: false`. Native task dispatch/history completion and
automatic result delivery through these terminal Agents are separate work. The
subsequent [Codex terminal Worker checkpoint](native-herdr-worker-20261011.md)
connects an explicitly configured Codex binding to the native daemon and Pi RPC
Lead; the standalone mounting command itself still does not connect a daemon.

## Evidence and interface

The installed Herdr 0.9.0 Windows executable contains its JSON protocol schema.
That static data was extracted without running Herdr, contacting a session or
reading terminal content. Selected structural definitions (descriptions removed)
are embedded in `src/contracts/herdr-0.9.0.json`. The executable SHA-256 is
`9b3bf49f94c2d09b1d62e11171b132865768dafc36b1327fb946c0cef9ca0d00`.
The embedded source schema SHA-256 is
`6f99da6cfe3044562740568c967bb6b61d090cc2e80ef041e26dfd149fd8b1ce`.

The source contract confirms `AgentInfo` uses `agent`, `name`, `agent_status`,
`interactive_ready`, `launch_pending`, `pane_id` and `terminal_id`; process
information includes `shell_pid`, foreground PIDs and optional cwd. It does not
provide a public Agent process-generation ID. The native observer therefore
records Windows PID creation time and Windows session ID, without acquiring
termination rights or adopting that process into a CCB-owned process tree.

`ccb-native herdr-agents` accepts one JSON request on stdin. It uses the same
schema-1 `config`, explicit Herdr `session`, executable `command` and bounded
transport `timeout_ms` as `herdr-namespace`. Its actions are:

- `plan`: validate a launch plan without contacting Herdr or changing files.
- `mount`: launch configured enabled Agents into the already-owned namespace.
- `observe`: refresh saved bindings without issuing launch commands.
- `inspect`: read the local ledger without live queries or writes.

`interactive_args` maps logical Agent names to arguments passed after Herdr's
`agent start ... --` delimiter. RPC/headless command arrays from the standalone
daemon configuration are not reused as terminal commands. Provider arguments
such as Pi's `--session` remain native arguments; Herdr selector checks apply to
the control prefix before the delimiter. No raw `pane run` fallback is used.
The default startup timeout is 30 seconds, with an accepted range of 3001–60000
ms. The outer transport observation can expire earlier without authorizing a
second launch.

## Namespace environment and cwd

`herdr-namespace` now passes an Agent's configured `env`, overlaid by optional
per-slot `slot_env`, when creating its root Shell or split pane. `cmd` can receive
an explicit slot overlay as well. Values are passed as argument values, not
assembled into a shell script. Herdr-owned variable names are rejected, as are
invalid names, NULs, oversized input and ambiguous Windows case variants.
The saved plan contains an environment hash, not environment values.

Root-pane creation now uses the root slot's actual workspace path. Previously,
an Agent rooted in a Git worktree could get a Shell in the project directory
instead. Existing workspaces are never silently moved or reconfigured: changed
environment, launch arguments or namespace plans require explicit migration.

## Launch and recovery rules

The controller shares the namespace store lock and checks its prepared ownership
tokens before launch. All requested panes must be free of Agents and at an
observed Shell process with the expected cwd. Unknown cwd or ambiguous process
information is refused. It generates session-wide unique Herdr names and records
every per-Agent intent before calling `agent start`. The server additionally
owns its available-Shell check; CCB never types a command into an arbitrary pane.

After submission, a fresh snapshot must match the unique name, provider kind,
workspace, pane, terminal lifetime and namespace tokens. Binding also needs a
live local process identity. Once bound, PID reuse or a replacement process is
not silently adopted. During known Agent activity, the previously bound process
can remain alive while a tool temporarily controls the foreground; fresh idle
readiness must still resolve back to the bound provider.

A lost start receipt can be reconciled from the unique live alias and matching
process identity. An absent alias after a sent request remains uncertain and is
never treated as proof that the command was not applied. Disappearance after
binding is reported as lost. Blocked startup is recorded without answering an
approval or restarting the Agent. Only idle/done plus `interactive_ready` can
produce startup readiness. None of these observations finishes a CCB Job.

The ledger stores argument/environment hashes and selected binding fields, not
raw launch argv, environment values, command stderr or terminal output. Local
inspection explicitly says it has not checked live state. Saved session refs
are informational metadata, not permission to read arbitrary files.

## Validation and remaining work

Tests use an offline Herdr protocol fixture and harmless local processes to
exercise real Windows creation-time queries. They cover lost receipts, timeout,
controller process death after committed launch intent, no duplicate start,
blocked-to-ready transitions, changed ownership/terminal/provider/PID bindings,
foreground tools, local inspection, environment/cwd handling and preservation
of an unrelated fixture workspace. The embedded schemas validate observed Agent
and process objects. No real provider TUI was launched in this checkpoint.

The calling desktop task has no `HERDR_ENV=1`. The
[Herdr skill](C:/Users/user/.agents/skills/herdr/SKILL.md) says:
“Do not inspect or control the focused Herdr session from outside Herdr.”
No live Herdr command or session control was performed, and the environment was
not forged. The earlier request for external-session test authorization has not
received a human answer. Static executable/schema inspection is not live-session
evidence.

Full native `herdr open`, Server bootstrap/attachment, provider-history completion,
CCB dispatch/callback wiring, mounted-Agent retirement and configuration migration,
server-generation/CAS guarantees, role launch composition and production live
validation remain incomplete. Display tokens and local files are cooperative
ownership evidence, not authentication against another controller rewriting them.
Startup read/check and server mutation are not a cross-client atomic transaction.

Validation totals: the initial mount/environment suite passed 50 cases in
137.63 seconds. The final mount/namespace/transport/config/roles/startup selection
passed all 131 cases in 161.52 seconds, including native provider argument
forwarding after `--` and foreground-tool handling. Rust tests: 14 passed with
one test-only crash-worker helper ignored in the ordinary runner and exercised
by its parent. Formatting, clippy with warnings denied and diff checks passed.
The full curated 772-case native inventory was collected, not executed; the last
complete native run remains the recorded 613-case baseline. No installation,
account settings, existing Workspace or `codex_impl` was changed or controlled.
