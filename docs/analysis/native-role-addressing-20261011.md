# Native role identity and addressing

The Rust configuration reader now expands a compact role token such as
`agentroles.archi:codex` to the installed role's default Agent name and records
the canonical role binding. Hybrid overlays address the expanded Agent name.
The reader renders layout precedence consistently with the existing Python
layout model, including necessary parentheses and leaf percentages.

Role lookup checks project `roles/installed`, then `.ccb/roles/installed`, then
the configured user role store. It supports `AGENT_ROLES_STORE`, explicit
`CCB_SOURCE_HOME`, direct manifests and `current` directories/links. Legacy
`ccb.archi` and `agentrole.ccb_self` identifiers resolve to their canonical
identities, including legacy installed directory names. Default naming honors
the existing identity-name, CCB-adapter and role-suffix precedence.

Lookup reads only the selected TOML files. It does not install packages, execute
role hooks, scan/copy a role tree or create a snapshot. Invalid IDs, manifest-ID
mismatches, invalid/default-name collisions and malformed selected manifests
fail before runtime startup. Parent traversal IDs and empty role components are
rejected. A legacy managed provider HOME is not mistaken for the Windows user
role store.

The native authority resolves role-addressed submissions against its loaded
configuration. Exactly one binding is required; ambiguity includes disabled
bindings and is reported instead of choosing an Agent. A unique disabled Agent
still rejects dispatch. The stored request names the resolved project-local
Agent, so existing admission, sender, queue and callback rules continue to use
Agent identities. A role is not accepted as a substitute sender identity.

```powershell
ccb-native --state-root <runtime-root> agents
ccb-native --state-root <runtime-root> ask agentroles.archi <message>
```

`agents` reports names, providers, canonical role bindings and disabled state.
Explicitly named rich-TOML Agent bindings retain their names while role IDs are
canonicalized. Compact/hybrid project startup and executable discovery use the
same project-aware reader as `config show` and `config compile`.

This checkpoint covers identity, compact default-name lookup and submission
addressing. It does not yet materialize role memory/skills, enforce all role
activation policies or execute lifecycle hooks in the native runtime. Those
capabilities, full worktree/role parity and default native installation remain
migration requirements.

Verification uses the existing Python parser and target resolver as differential
oracles, plus real native CLI/authority fixture processes. Coverage includes
canonical/legacy IDs, project/user precedence, adapter fallback, source-home
lookup, invalid manifests/names, ambiguous/disabled routing, rich bindings and
role-addressed work after project startup. Exact results are recorded in the
migration progress JSON.

Final release validation passed 63 affected role/config/startup/task-command
cases, including 20 role cases. Rust unit tests (7), formatting and Clippy with
warnings denied passed. The full native suite was not repeated for this focused
configuration/routing change; its preceding full result remains 305 passes.
