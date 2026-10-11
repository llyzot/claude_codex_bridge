# Native Pi role skills

Rust now resolves Pi role skill paths and controls their registration before
each dispatched task and callback. The installed Pi 1.1.0 RPC protocol has no
standalone resource-reload command, but its extension command context exposes
`ctx.reload()` and its `resources_discover` event accepts live skill paths.
The integration uses those public interfaces.

A small bundled JavaScript extension connects the Pi APIs. Rust owns the role
manifest resolution, routing plan, queue slot, execution identity, cancellation
and completion. The extension reads the native plan, declares skill paths and
handles one randomly named reload command. It does not schedule tasks, decide
completion, copy skills, execute role tool hooks or implement another daemon.

The native store owns exactly two additional per-Agent assets:
`providers/pi/<agent>/native-role-bridge.mjs` (the bundled adapter) and
`providers/pi/<agent>/role-plan.json` (owner, command, execution generation and
absolute source paths). The source skill tree stays in place. The plan and
context audit are committed before control input. A fresh owned process gets a
new bridge owner; saved Pi conversation identity can still resume.

## Dispatch contract

1. Inspect `get_commands` and verify the randomly named bridge command, its
   owner description and exact source path before sending a control command.
   A missing bridge fails the task; the slash command is never blindly sent.
2. Invoke the matching generation through `prompt`. Its response must say
   `handled`. A normal model-run disposition is an uncertain protocol failure,
   not successful reload or task completion.
3. Inspect commands again and verify the newly loaded execution generation and
   every declared skill's actual source path.
4. Read Pi state and require the same session ID, no streaming/compaction and
   an empty pending queue. Only then send the original task with its request ID.

Cancellation and deadlines preserve the pending slot until the correlated
control response arrives. They discard the task input instead of sending an
abort into an in-progress resource reload. A queued next task gets its own
generation. Foreign responses cannot advance preparation. Missing/stale
resources fail only that task; malformed protocol, an unexpected model run,
changed session identity or non-idle state quarantine the uncertain provider.
Owned-process termination and cold recovery remain available through the
existing controls. Observation timeout alone never replays the task.

`rolepack/v1` selects `skills.pi`. Preview roles require Pi in the CCB adapter's
supported/recommended providers; the existing default remains Codex and Claude.
The native provider parses skill metadata and retains its invocation behavior.
Removing declarations reloads an empty role root list. Adding later skills to
a role-bound session works because its owned bridge is already loaded.

## Evidence and limits

An installed-Pi no-model probe registered and removed a live skill through this
extension. Session ID was unchanged and `messageCount` stayed zero: reload
commands did not become model conversation turns. The authoritative interfaces
were checked in the installed package's `dist/modes/rpc/rpc-mode.js`,
`dist/core/agent-session.js`, extension types and resource loader.

The actual native daemon also passed a two-turn real-model test using Pi's
`read` tool in a new workspace. Between turns the skill changed its reference
from `first.txt` to `second.txt`, each containing a different random marker.
Both replies returned the matching marker in the same runtime and session.
The provider's saved conversation records show the tool sequence:
`SKILL.md`, `first.txt`, `SKILL.md`, `second.txt`. This verifies body/reference
use beyond catalogue visibility. It does not change or resolve the separately
recorded Codex `blocked by policy` real-model failure.

Protocol fixtures cover reload/removal, preview provider filtering, errors,
foreign replies, cancellation at each phase, deadlines, quarantine isolation,
same-session repair, saved-session restart with a new owner and automatic
Worker-result delivery to a Pi Lead. An initial release run exposed missing
store path allowances for the new controller assets. Exact Pi-only paths were
added, with a store test rejecting other filenames/providers; the fixed release
passed the affected role-memory cases. Final counts are in the progress JSON.

Pi's reload operation refreshes its full resource/extension runtime and settings,
not only one skill. Extension lifecycle hooks may run; conversation preservation
does not imply preservation of extension-local variables. The real-model test
disabled unrelated discovered extensions and used only the bundled bridge.
Production extension interactions and reload overhead still need evaluation.
This checkpoint does not implement role permissions, tool hooks, activation
policies, Claude role skills, Herdr attachment or default native installation.
Production installation and existing Workspace/Agent processes were untouched.

Final validation: 38 role/role-memory cases passed, followed by 94 affected
Pi/RPC/lifetime/fault/recovery cases including all 20 new Pi role cases. The
real Pi model test passed in 26.03s. Rust tests (8), formatting and Clippy with
warnings denied passed. Both repository binaries match the release outputs.
The full 387-case native inventory was not repeated for this change.
