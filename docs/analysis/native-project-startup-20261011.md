# Native project startup checkpoint

The Rust CLI now discovers `.ccb/ccb.config` above the current directory, reads
compact/TOML/hybrid configuration, resolves existing Pi/Codex/Claude programs and
starts its owned standalone daemon in the background. A manual internal JSON
config or executable manifest remains available for integration tests and custom
installation layouts. No interpreter, shell, package manager or installer is
invoked for core startup. Provider runtimes retain their own dependencies.

```powershell
# From a project containing .ccb/ccb.config, or one of its subdirectories:
ccb-native start
ccb-native ask worker "Task description"
ccb-native queue

# An explicit project is also accepted:
ccb-native --project C:\path\to\project start
```

Windows launcher native selection (`CCB_NATIVE_CLI=1`) now forwards an ordinary
`ask worker ...`, including optional global project/state directories, into this
native context. Production/default routing remains Python until the remaining
migration gates pass. Native project startup is stdio-owned provider execution;
it does not yet implement `ccb herdr open` or a graphical pane attachment.

Runtime location follows the existing project anchor, absolute
`CCB_RUNTIME_STATE_HOME` override or a matching `runtime-root-ref.json`.
Overrides/references keep the `ccbd` directory below the selected runtime root.
Malformed or foreign references fail before startup instead of silently writing
to a fallback directory. Full WSL relocation and project identity migration
remain incomplete.

Default Windows discovery selects native Codex vendor binaries beside Node's
installed npm packages, the existing earendil Pi CLI entry with Node, and Claude
on PATH or in the account's `.local/bin`. Known Scoop executable shims are read
as path data; no shim script or package manager is run. `config compile` accepts
an optional explicit executable manifest. Discovery is not installation or
proof that an executable implements the expected protocol.

The caller serializes the compiled plan into a private bootstrap pipe; there is
no extra persisted copy of plugin/skill directories or generated daemon config.
A startup lock, durable PID/startup UUID/config fingerprint and authenticated
ping fence concurrent calls and readiness. A matching running daemon is reused;
a foreign project or changed configuration is rejected without replacing it.
Observation expiry returns `starting` and retains the process; subsequent
observations use that same identity. This is not yet the full Keeper/reload or
interrupted-running-task reconciliation implementation.

Windows standard handle inheritance is temporarily fenced during the single
threaded CLI spawn and restored afterwards. This prevents the detached daemon
from retaining the caller's captured output pipes; its own streams go to native
log files. The daemon owns its existing kill-on-close process Job Object.

An Agent can use its scoped native token reference and `CCB_AGENT_NAME` to route
`ask` from a work directory outside the project. Project ID is inferred from the
project or authenticated daemon. Admission still rejects an unknown sender and
applies existing nested-ask/callback policy.

Tests use new temporary projects, fake providers and state directories. They
cover captured-pipe return/background lifetime, nested-directory configuration,
state relocation, parallel/idempotent startup, timeout observation without
restart, foreign/config-change rejection, preflight without runtime writes,
read-only executable discovery and Agent context routing. Existing Workspace,
`codex_impl`, installed CCB, PATH and provider authentication were unchanged.

Remaining scope includes full CLI/config/worktree/role parity, Keeper/reload,
Herdr integration, Unix process ownership, actual model startup validation,
installer/default native activation and Python core retirement.
