# Native live role memory

The native Pi RPC, Codex app-server and Claude stream transports now receive
the configured role's memory on each dispatched turn, including result-chain
continuations and ordinary reply delivery. The role store, selected manifest
and declared files are resolved again for every turn. Editing a declared file
or removing its declaration affects the next turn without restarting the
provider. No role directories are scanned, copied or snapshotted.

The existing project/user store precedence and role aliases are shared with
compact role-name lookup. `rolepack/v1` uses `memory.files`; an
`agent-role/preview-*` manifest uses `contents.memory` followed by the CCB
adapter's `memory` paths. Only the default `append_after_project_memory`
strategy is supported. Identity-only legacy manifests without a schema remain
accepted for compatibility with existing role bindings.

The request body and its `CCB_REQ_ID` anchor stay intact. Current memory is
appended as a marked role-context section in the provider's user input. A new
section says it replaces prior CCB role memory, including an explicit empty
section when all declarations are removed. Provider-native project context
still loads through the provider itself. This change does not implement the
entire Python shared/private project-memory projection or promise that a model
forgets text already present in its conversation history.
Every dispatched turn carries the declared memory again, so large role files
increase model input and conversation size. The byte cap is an IO bound, not a
guarantee that any selected model has enough remaining context.

Files must be UTF-8 and use relative portable paths inside the resolved role
root. Parent traversal, absolute/drive paths and resolved file targets outside
that root are rejected. Declarations are limited to 64 files and 256 KiB total
content per turn; selected TOML files retain the existing 1 MiB limit. Unlike
the Python lookup's silent omission of unavailable files, a missing or invalid
declared native memory file produces a terminal `role_context_invalid` failure
before provider prompt input. The provider remains usable, other Agents keep
running, and a later submission can succeed after the role is repaired.
Configuration failures cannot trigger automatic retry.

Before sending provider input, the authority commits a `role_context_loaded`
job event containing the role, root, relative source paths, byte counts and
SHA-256 hashes. This is a context-preparation audit, not an input-delivery
acknowledgement or completion signal. Role text is held only in the outgoing
prompt; it is not duplicated into native Job/Event records or cached role
trees. Providers may retain the supplied prompt in their own session history.
Invalid TOML excerpts and role content are excluded from failure diagnostics.

Scope remains deliberately explicit: this implements memory delivery for the
three persistent native providers. External/manual workers and generic command
drivers do not gain role injection. Role skills, tool hooks, compatibility and
permission enforcement, activation policies, full project-memory parity, role
lock warnings and Keeper configuration reload still require migration. No
production installation/default-runtime switch or new real-model execution is
part of this checkpoint.

Tests use actual Rust daemon processes and protocol fixtures to check both
manifest formats across all three providers, updated Unicode memory in the
same process/session, ordered adapter memory, empty declarations, persisted
hashes without content copies, invalid-file failures followed by repair, and
automatic Pi Lead resumption after a Worker finishes while role memory changes.
Exact final validation results are recorded in the migration progress JSON.

Validation: 80 affected role/provider/task contracts passed. Following the
Clippy-only expression refactor, the rebuilt final release passed all 56
role-memory/config/startup/result-policy cases, including all 19 new role-memory
cases. Rust tests (7), formatting and Clippy with warnings denied passed; both
repository binary hashes match the release outputs. The prior full-suite
checkpoint remains 305 passes; the current 344-case inventory was not run in
full. Initial test-observer/API fixture mistakes were corrected before these
passing runs.
