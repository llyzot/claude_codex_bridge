# Native Pi terminal Lead — 2026-10-11

The experimental `herdr_tui` driver now supports original Pi terminal sessions as
well as Codex. The native daemon can dispatch an initial Pi Lead task, accept the
Lead's ordinary native `ask --chain`, release its initial turn, and automatically
send the Worker result back into the same Pi process/session for review.

The completion adapter is a small JavaScript extension because that is Pi's
extension API. Rust provisions it and owns request admission, durable send grants,
history validation, completion decisions, FIFO scheduling and result delivery.
There is no Python runtime dependency in this path and no profile/resource-tree
snapshot. The generated extension is not a replacement for Pi itself.

## Provisioning and binding

`ccb-native pi-terminal-bridge` accepts a single JSON request on stdin:

```json
{"schema_version":1,"project_root":"C:/test/project","agent_name":"lead"}
```

This creates a small generated extension and its manifest in the native Herdr
journal, plus an Agent-scoped event directory. It returns `bridge.owner`,
`bridge.event_root`, `bridge.extension` and an `interactive_args` array containing
`-e <extension-path>`. It starts no provider or Herdr server and changes no account
settings. Reprovisioning preserves the owner identity and updates the generated
source; it does not copy the user's resources. Use the returned arguments when
mounting Pi through the existing native `herdr-agents` interface.

On `session_start`, the extension creates a unique event file and atomically
publishes `current.json`. The header/current state identifies the extension owner,
instance UUID, Windows process identity, native Pi session ID/file and workspace. A fresh
extension load or session switch allocates a new file and retains earlier logs.
This permits idle extension reload in the same process without reading the old
generation as the new one. Active jobs remain bound to the old instance; reload,
session switch or unmanaged input cannot silently transfer their completion.

Configure a mounted Pi Agent with `provider: "pi"`, `driver_kind: "herdr_tui"`,
`command: null` and `pi_bridge_owner` equal to the returned owner. Its
`herdr_history` object contains the event root, the absolute `current.json` path,
the exact native session ID and workspace cwd. As for Codex, global `config.herdr`
is the original mount/namespace request, not the mixed daemon configuration.
Herdr must independently report either the matching native session ID or native
session file path. On session start/switch the extension calls the read-only native
`process-identity <pid>` helper, with no shell or visible window. Rust requires its
PID, creation time and Windows session ID to match the saved Herdr process lifetime;
PID reuse cannot substitute another process. A manifest alone is insufficient to
adopt an Agent. Unload publishes `ready: false`, so a failed reload cannot keep
dispatching through an old ready manifest.

Each new task resolves the small current-state file to one UUID-named log inside
the declared root. The durable prompt permit saves both the configured selector
and the resolved history binding/producer. Cold recovery reads that original log
instead of following a later `current.json`. Missing/foreign owners, paths or PIDs
fail before sending; generation changes after admission hold uncertain work.

## Completion contract

The installed Pi 1.1.0 types and implementation define `agent_settled` as the point
after retries, compaction and queued continuations have ended. Its extension
context exposes the settled idle state. `agent_end` and `turn_end` are insufficient.

The adapter associates a fresh turn UUID with an execution-specific first-line
CCB request marker. It records actual `agent_start` and the delivered user-message
marker, then emits the latest assistant snapshot only at `agent_settled`, including
idle, abort, stop reason and an error flag. It does not store model headers,
credentials, thinking, tool arguments or full user prompt/role memory.

Rust requires the exact owner/instance/PID/session, a continuous event sequence,
the matching turn and request anchor, and a settled idle/abort record. Only a final
`stop` without an error can be success. Tool-use/length/error endings cannot become
success, and abort is cancellation. Foreign request markers do not complete the
task. Session changes, shutdown and superseding input hold it without resending.
The shared `terminal_history` module retains bounded complete-record reads,
file-identity checks, partial-UTF-8 handling and the 64 KiB baseline-boundary digest.

Idle extension reload changes the event generation while preserving the process
and session. The next task resolves the new generation. A controller restart
continues observing an already permitted old execution and never resends it.
External interruption, context controls, process retirement and idle-fault recovery
remain outside this driver, as documented for the Codex terminal checkpoint.

## Verification and remaining acceptance

`test_native_pi_terminal.py` runs the actual generated extension in a minimal
offline Pi event host. The Lead invokes the real native CLI to submit a chain
request. A fake Herdr protocol transports prompts and observes genuine Windows
process lifetimes; the native daemon, event reader, journal, mailbox and callback
path are real. Tests cover same-process Lead review, nonterminal `agent_end`,
reload/session/input supersession, idle reload, controller death, foreign state
and discontinuous logs. Rust tests cover abort/error/tool-use/length classification,
foreign anchors, missing idle proof and producer/sequence mismatches.

The opt-in `CCB_RUN_REAL_PI_BRIDGE=1` probe loads the generated extension into the
actual installed Pi in RPC mode, using a new empty private home. It checks the
actual PID and session switch/log generation using native clear, without a model
turn or copying credentials. This verifies shared Pi extension APIs; it is not a
real terminal/Herdr/model acceptance test.

The first integration run correctly refused input because Node normalized Windows
path separators while Rust compared the manifest path as a JSON string. Comparing
path components fixed this false rejection; root/generation constraints remain.
No real Herdr session or existing Workspace was controlled. Full `herdr open`
startup composition, automatic discovery/configuration of fresh sessions, original
Claude terminal handling, model-backed terminal acceptance, performance measurement
and default Rust promotion remain unfinished. Exact final test counts and binary
hashes are recorded in the progress JSON.

Final release verification passed all 30 dedicated Pi/Codex terminal cases in
200.64 seconds, including the opted-in real Pi no-model probe, and 25 Rust tests.
An earlier broader 81-case terminal/mount/Pi-RPC/context run also passed. The final
process-lifetime and unload checks were then covered by the final dedicated run;
this was not a complete repository or curated native-suite rerun.
