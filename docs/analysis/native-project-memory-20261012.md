# Native shared and private project memory

The native prompt preparation path now reads these existing project files when
an agent's next task is dispatched:

- Shared: `.ccb/ccb_memory.md`
- Agent-specific: `.ccb/agents/<agent-name>/memory.md`

The configured project root is authoritative, including when an agent works in
a separate worktree. An explicit workspace path is the fallback for direct
daemon configurations without a project root; the daemon's current directory
is not an implicit source. No project files are created, seeded or rewritten.
Prompt source labels use absolute project paths in portable Windows form, so a
worktree agent can locate the source instead of confusing it with a local copy.

The merge order is shared project memory, existing live role memory, then the
current agent's private memory. This matches the relevant legacy source order.
Each turn reads the current files; pending tasks do not retain a copy taken at
submission. Missing or empty sources are represented explicitly so the current
prompt identifies them as having no current content. Edits during an already
running turn apply to later turns. Reading two files is not an atomic multi-file
snapshot.

The common entry point is used by native Pi RPC, Codex app-server, Claude stream
and Herdr prompt preparation. Provider-native project/user instructions such as
`AGENTS.md` and `CLAUDE.md` are left to the provider's normal loading behavior.
The source files are not copied into managed homes or runtime projection files.
Context-control turns bypass task memory preparation, preserving clear/compact
behavior.

Shared and private sources together have a 256 KiB UTF-8 limit per turn; the
existing role-memory limit is separate. Directory sources, invalid encoding,
oversized input and symlink/reparse components below the project root fail the
affected task before sending a prompt. The provider stays available for a new
task after the files are repaired. The generic failure does not echo memory
contents. Linked source arrangements accepted by older Python paths require
explicit migration; they are not silently followed.

`project_memory_loaded` events record the agent, project root, source paths,
existence, byte counts and hashes. They do not duplicate the source text, and
the persisted task request remains the original request. Provider session
histories naturally retain delivered context, and model replies can quote it.
Removing a source file does not erase earlier conversation history. Selecting
only one agent's private file is not filesystem access control: an agent with
ordinary access to the shared project could still read other files with tools.

## Verification

The project/role-memory selection passed 28 cases, with a further queued-task
dispatch-time update case passing separately. Role delivery, Herdr workers,
context controls, Claude context and native daemon regression passed 81 cases.
Rust unit tests passed 27 cases plus the parent-invoked subprocess helper.
Formatting, Clippy with warnings denied, diff checks and binary equality passed.
After the absolute-source-label refinement, all ten project-memory cases passed
again on the final release binary in 20.63 seconds.

A real hosted Codex session (`gpt-6.1-sol`) returned the first pair of markers,
then the updated pair, then `NO_CURRENT_MEMORY` after deletion, all on the same
process and thread without tools. An explicit cold-resume run used a new
process, retained the same thread and returned the newly recreated source
values. A final-release follow-up returned a further new pair and verified the
absolute source labels in native history. Across the five CCB turns, native
history and audits verify current source delivery and the
absence of the peer's private marker. Owned test processes were confirmed
absent after cleanup. IDs, hashes and responses are in
[the evidence](native-project-memory-20261012.json).

These are controlled-provider regression tests plus one real Codex sequence.
Actual Pi/Claude model behavior, legacy seed/template migration, full provider
projection/coordination-rule parity and production/default promotion remain
separate work. Existing production installation and workspaces were unchanged.
