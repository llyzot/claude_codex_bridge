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

## Native configuration reload

The native actor now accepts an explicit configuration cutover without replacing
the daemon. Use the repository-built executable while default installation still
uses Python:

```powershell
ccb-native reload --dry-run
ccb-native reload
ccb-native reload-status
# Optional bounded observation; expiry does not cancel or restart anything:
ccb-native reload --wait --timeout-ms 15000
```

Reload shares startup's `--project`, `--state-root`, `--executables` and `--config`
inputs. Public API operations are `reload-plan {config}`, `reload {config,
expected_fingerprint}` and `reload-status {}`. A blocked plan reports Agent names
and the relevant Job, mailbox event, callback or provider-fault IDs. Resolve the
work/fault, then submit the configuration again; blocked operations are not
automatically queued for later execution. Repeating an accepted target returns
the same operation while pending; an already-applied target is a no-op.

Changed and removed agents must have no unfinished jobs, pending inbound events,
open callbacks, outstanding replies to receive, or blocked provider faults.
An idle Lead waiting for a Worker result is therefore protected even before the
reply appears in its inbox. Adding an unrelated Agent leaves active work and
FIFO queues intact. While applying a change, submissions to/from affected agents
are fenced, including broadcasts and retries. Other agents keep executing.

Workspace preparation runs off the authority actor. Only after it succeeds is
an owned idle provider tree retired on Windows; cutover waits for process exit,
stream EOF and empty owned scope. The scheduler rebuild, configuration identity,
lease fingerprint, resume-pointer changes and applied receipt are committed
together. Unchanged provider handles remain alive. Replacements start normally
after publication. `applied` means the configuration was published, not that all
providers have authenticated or completed model inference; inspect
`provider-status` for startup failures. Shutdown is refused during an accepted
reload until it settles; forced process death remains a recovery event.

`configuration.json` stores hashes and historical Agent names; `reload.json`
stores one most-recent operation receipt with process identities and plan IDs.
Neither file contains compiled configuration, argv bodies, environment values or
credentials. Historical tasks remain queryable after removal and restart.
Changes to provider, driver, workspace or explicit environment/effective provider
home clear only CCB's native resume pointer, preserving provider history files.
Same-binding changes can resume a saved conversation on a new process.

Explicit stop/start also checks configuration changes against durable unfinished
work before materializing new workspaces. Legacy state without a verifiable
configuration identity must be drained with its previous runtime first. A crash
during reload does not replay the in-memory target configuration. The next
explicit start uses its supplied configuration and marks the old operation
interrupted. Existing worktrees and branches are retained, including incomplete
preparation; no automatic reset, prune, deletion or source copying is introduced.

This is bounded native data-plane reload. Global runtime/namespace changes,
busy-agent drain orchestration, graphical layout/Keeper/Herdr reconciliation,
workspace retirement, automatic rollback of filesystem preparation, non-Windows
live provider replacement and provider-internal background-task discovery remain
outside this implementation. Git filters may execute during checkout, as with
native startup. Provider readiness and live resource refresh are separate from
configuration publication. Production defaults remain unchanged.

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

## Herdr command transport checkpoint

The transitional Herdr adapter now uses Rust for its bounded CLI control calls
when `CCB_CORE_RUNTIME=rust`. The DTO bridge locates `ccb-native` beside the chosen
kernel, or uses an explicit `CCB_NATIVE_BIN`. Default Python selection retains
the existing runner. This is a transport migration, not the complete native
`ccb herdr open`, Server bootstrap, workspace planner or interactive Agent UI.

`ccb-native herdr-transport` consumes one schema-1 JSON request from stdin:
`operation`, explicit `session`, executable `command` argv, operation `args`,
`expect_json`, and a bounded `timeout_ms`. It is an internal transport interface;
CCB still determines namespace and pane ownership above it. It allows explicit
workspace/pane/tab/agent controls and observation commands, but rejects bare
interactive startup, Server/session/machine lifecycle commands, inherited or
retargeting selectors, implicit mutation targets and `--wait` task waits.

Rust invokes the CLI directly without a shell, removes inherited Herdr caller
and socket selectors while retaining account config discovery, closes stdin and
captures bounded UTF-8 output. On Windows the CLI tree enters its owned Job at
creation; stdout/stderr handles are explicitly inherited and parent pipe ends
are not inheritable. Existing provider transports still use null stderr.
Completion requires CLI exit and stream EOF; a descendant holding a pipe cannot
turn an early parent exit into a successful receipt. Timeout/output overflow
terminates only that owned CLI tree, never an existing Herdr Server.

The response distinguishes `acknowledged`, read-only `unobserved` and mutating
`unknown` outcomes. Every response explicitly says `task_completion:false` and
`automatic_replay:false`. Error stdout/stderr and request bodies do not enter
error diagnostics. Unknown failure statuses, malformed output, wrong helper
identity and helper claims of task completion/replay are rejected. The Rust
branch bypasses the old automatic Server-start/retry path after command failure.
A lost connection or CLI timeout is not proof that a server-side mutation failed
to happen; inspect the explicit session before deciding on recovery.

Validation uses an offline executable fixture only, plus existing owned-provider
contracts. The current Codex process is not in a Herdr-managed pane, so the Herdr
skill's context gate prevented live-session validation. No real Herdr session
was inspected, created, stopped or controlled. Full native topology/application,
explicit Server lifecycle, original provider terminal interaction, completion
history adapters, owned-binding recovery and live Herdr validation remain open.
The release capability `herdr_integrated` therefore remains false.

## Native Herdr namespace allocation

`ccb-native herdr-namespace` is an internal JSON/EOF interface for `plan`,
`prepare`, local `inspect`, and explicit pending-operation `confirm`. Requests
contain schema version 1, the compiled native `config`, an explicit Herdr
`session`, executable `command` argv and optional bounded `timeout_ms`. It does
not implement the full `ccb herdr open` command or launch providers. Its
`provider_commands_sent:false` result is not a claim about processes that Herdr
templates might start themselves; provider occupancy/control remains separate.

The planner reuses the native compact grammar, resolves named rich-config leaves
against compiled providers, preserves mixed nested splits and explicit relative
percentages, and matches the existing first-specified-percentage and ties-to-even
balancing rules. A default balanced layout is supported. The current plan must
cover its configured agents exactly, with `cmd` first when present. Multiple
explicit windows, topology changes after allocation and partial Agent selection
are rejected pending their own migration. Work directories must be materialized
before allocation; this controller does not silently create/copy workspaces.

The authority lives under `<native state root>/herdr`, with its own exclusive
writer and redo journal. `namespace.json` stores the plan, random ownership
identity, returned workspace/pane IDs, verified binding markers and one pending
mutation. The executable/environment discovery context is hashed; provider
environment values and command bodies are not copied into this record.
`inspect` reads the atomic local record without waiting for the writer or calling
Herdr, reports `live_checked:false`, and reports plan mismatch explicitly.

Every create/split/metadata call first commits an operation ID and then a sent
intent. Successful allocation binds IDs returned by Herdr, preserving the
current focus through `--no-focus`. It never closes existing workspaces or
guesses pane IDs. Workspace/project/owner/slot markers are checked before reuse;
missing or foreign bindings stop preparation. Metadata acknowledgements may be
JSON, plain text or empty, but success still requires observing the expected
tokens in the inventory. A lost metadata acknowledgement can be reconciled by
observation without writing again.

Creation has no known server idempotency key. A lost acknowledgement therefore
remains `uncertain`, even if an extra workspace or pane appears. Repeated prepare
calls do not create a replacement. A caller deliberately resolving it must send
the exact `operation_id`, `confirm_pending_creation:true` and the observed
`workspace_id`/`pane_id` as appropriate. The controller rejects objects present
in the pre-creation baseline and validates the target namespace/label/ownership.
The same explicit confirmation can permit reapplying pending metadata. This
local administrative operation is not a human-identity/authentication mechanism.
If creation did not happen, there is currently no automatic intent reset or
speculative retry. Persistence failures are fatal rather than overwritten with
a guessed recovery record.

Tests use a persistent offline inventory fixture, including intentional lost
receipts and killing the owned controller after a simulated remote mutation.
They verify duplicate prevention, explicit recovery, operation-identity fencing,
local observation during an active writer, existing-workspace preservation,
corrupt-ledger rejection and no provider launch commands. Real Herdr validation
is still pending permission to use an independent test session from the current
non-Herdr task context. Full native startup composition, original provider UI,
binding to active Agent lifetimes, API/version-specific runtime confirmation,
multi-workspace reload/retirement and cross-server-generation recovery remain
open. Ownership markers and inventory reads are not a server-side transactional
compare-and-swap guarantee across concurrent external changes or restarts.

## Native Windows installation lane

The repository-built core can now install itself to a **new, explicit Windows
x64 prefix**. No Python runtime, virtual environment, Git Bash, WezTerm or package
manager is involved in this installation or native core execution. Provider
programs retain their own prerequisites and authentication. Git workspace and
review-tree features require Git. Herdr integration is still pending.

```powershell
# Parent directory must already exist. Preview creates no prefix:
bin\ccb-native.exe install --prefix C:\tools\ccb-rust --dry-run
bin\ccb-native.exe install --prefix C:\tools\ccb-rust

# Equivalent Windows installer entry, with no account PATH/config changes:
.\install.ps1 install -Runtime rust -InstallPrefix C:\tools\ccb-rust

# These installed commands run Rust by default; no activation or selector needed:
& C:\tools\ccb-rust\bin\ccb.exe --version
& C:\tools\ccb-rust\bin\ccb.exe installation verify --prefix C:\tools\ccb-rust
& C:\tools\ccb-rust\bin\ccb.exe start
& C:\tools\ccb-rust\bin\ask.exe worker "Task description"
```

`ccb.exe`, `ask.exe` and `ccb-native.exe` share the native CLI implementation.
Aliases use hard links within the new prefix where supported and a bounded file
copy otherwise. `ask.exe` handles the ordinary ask syntax, global project/state
options, redirected input and errors directly. The version output identifies
both the CCB release and Rust core crate. The installation contains these native
commands, license, version, usage guide and a manifest; it does not bundle the
legacy Python implementation or its auxiliary commands.

Provider subprocesses launched by an installed native core receive its `bin`
directory first in their process-local PATH, preserving any explicit provider
PATH after it. They also receive the matching install prefix and native launcher
selector, so old account launchers cannot silently select Python. This does not
modify account PATH or global provider skills, settings or credentials.

Installation stages files beside the destination, verifies their hashes, then
publishes a complete new directory. Existing unowned prefixes, reparse points,
modified installed files and different builds are refused. Reinstalling the
same verified build is idempotent. A creation lock fences simultaneous installs;
an interrupted installer can leave that lock/stage for inspection. There is no
automatic removal or overwrite recovery. For version changes, use a separate
prefix and retain the prior one for explicit rollback, stopping owned services
and checking state compatibility before switching. `installation verify` checks
the fixed owned inventory; it is a corruption/provenance check, not a signature
or malware verifier. It does not validate the account's external provider tools.

The PowerShell entry also accepts local release archives and SHA256 verification.
Rust mode requires an explicit prefix and bypasses legacy prerequisite checks,
Python setup, user PATH edits, provider configuration and legacy cleanup. The
legacy installer refuses a Rust-owned prefix. The normal installer default is
still Python until migration parity is proved.

Release packaging now compiles all native binaries from the exported, resolved
commit rather than mixing an archived ref with the current checkout. Kernel and
CLI builds share one Cargo target directory. Archives require both native core
binaries, and metadata distinguishes the experimental Rust prerequisite set from
the existing Python default. The actual binary's `--capabilities` version and
installation contract determine whether the Rust install lane is advertised;
older exported refs are not falsely marked as supporting it. The PowerShell
entry checks this contract before installation. This is not a production/default rollout; full
in-place upgrade/removal, installer-death recovery, signed release provenance,
Herdr integration and the remaining core feature parity still need work.
