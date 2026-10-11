# Native Git workspace planning and startup

Native config compilation now plans `git-worktree` workspaces instead of
rejecting that mode. It retains the existing compact `agent:provider(worktree)`
syntax and supports rich configuration for managed per-Agent directories,
`workspace_group`, an explicit `workspace_root`, and existing external
`workspace_path` checkouts. In-place execution remains the default.

Compilation is read-only: it resolves Git identity, validates configuration and
existing bindings, and returns a workspace plan. Materialization runs inside
the background native startup process, after acquiring the native store writer
lock and before starting providers. Startup observation expiry retains the same
creator identity rather than launching another checkout attempt. The startup
fingerprint does not change merely because the source repository's HEAD advances.

Managed paths follow the existing layout: the target project's effective CCB
state directory plus `workspaces/<agent>` or `workspaces/groups/<group>`. An
explicit root uses `<root>/<project-slug>/<agent>`. Relative overrides resolve
against the target source directory, avoiding dependence on the invoking shell's
current directory. Branch templates support `{agent_name}`, `{project_slug}`
and UTC `{date}`. `base_commit` accepts an explicit local commit/ref for creating
a new branch; an already existing branch is reused without reset.

The managed `.ccb-workspace.json` binding uses the existing schema 2 record and
ordinary Windows path spelling for compatibility with the Python reader.
Internally canonical paths remain available for identity checks. Git arguments
also use Git-compatible path spelling; the first tests exposed that Git worktree
creation rejects Rust's extended Windows path prefix. CLI project discovery from
a bound worktree requires an untracked marker, a registered linked checkout and
matching project/repository identity before routing back to the source project.

## Ownership and failure behavior

Creation uses a per-path lock and a small adjacent intent record. The record
contains source/workspace identity, branch and starting commit, never provider
credentials or a repository snapshot. The intent is durable before Git runs;
the binding is published without replacing an existing file.

Existing managed bindings must match the source project, path and branch.
Registration and Git common-directory identity are checked, so an arbitrary
directory or a checkout from another repository is not adopted. A missing
binding requires a matching native creation intent. An uncertain prepared
creation is resumed only when the checkout is clean at its recorded commit.
Confirmed owned worktrees preserve staged, unstaged and untracked work on reuse.
External workspaces are inspected without adding a managed binding.

No path is automatically reset, forcibly checked out, pruned or removed. Stale
registrations, dirty uncertain creations, branch conflicts, tracked bindings and
foreign/unowned paths stop startup and preserve existing files. There is no
directory-copy fallback. Checkout hooks are disabled for native creation through
a command-local hooks path that names the regular intent file; explicit setup
and role lifecycle hooks still require their own migration. Normal Git checkout
behavior, including configured content filters, otherwise remains Git's own.

Plans are inspected before materialization begins. A later IO/Git failure can
leave earlier successfully created workspaces in place; the next authorized
start reuses them. Providers are not started until the workspace stage succeeds.
Stopping the daemon retains worktrees and branches. Automatic retirement/merge
cleanup, copy-mode workspaces and the broader Keeper/reload lifecycle remain open.

## Evidence

Tests use disposable local Git repositories. Differential checks cover existing
Python planning for default, explicit-root, grouped and external paths. Native
startup tests check actual provider cwd, worktree-to-project routing, shared
groups, repeated start, dirty-work preservation after source HEAD advances,
external checkout preservation, branch/base selection, invalid configurations,
foreign/unbound paths, tracked markers and interrupted creation recovery.
Additional cases cover missing registered directories without pruning,
checkout-hook suppression, observation-expiry reuse and relocated runtime state.
The schema-2 marker is also read through the existing Python binding loader.

No worktree was created in the development repository or the user's existing
Workspace. No production installation or default-runtime switch was performed.
Final validation counts are recorded in the migration progress JSON; this
checkpoint does not claim full configuration/workspace lifecycle parity.

Final validation passed all 27 new workspace cases and all 70 affected
workspace/config/startup/role cases. Rust tests (11), formatting and Clippy with
warnings denied passed; both repository binary hashes match release outputs.
The full 482-case native inventory was not run.
