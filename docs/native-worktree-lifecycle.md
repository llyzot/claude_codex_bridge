# Native managed worktree lifecycle

The Rust CLI supports inspecting, merging, retiring, restoring and permanently
removing a CCB-managed Git worktree. This initial workflow is offline: stop the
project's native daemon/Keeper and affected provider processes, and resolve pending
jobs, replies, callbacks and provider faults before changing checkouts. Commands do
not stop agents automatically. Inspection and receipt status can run while agents
are active.

These commands use the last committed native configuration. The controller and
managed worktree must belong to the same source repository. Externally supplied
checkouts are refused. Keep the agent's workspace configuration until its lifecycle
operation finishes; recovery validates the recorded plan against that configuration.
Simple shared-worktree bindings are checked for every affected configured agent.

## Commands in pwsh

Use the new repository-built executable until the running installed version can
be upgraded without interrupting your project:

```powershell
$ccb = 'C:\Users\user\Desktop\work\4.user\2.github\claude_codex_bridge\bin\ccb-native.exe'
# Run these in your intended project. Substitute its actual worker and branch names.
& $ccb worktree inspect worker
& $ccb worktree merge worker --into main --dry-run
& $ccb worktree merge worker --into main
```

`inspect` returns the ownership binding, HEAD, changed/ignored file records,
source checkout branch and current lifecycle operation ID. Review the worktree's
diff and explicitly commit your intended changes before merging. The source
checkout must already be on the requested target branch and both checkouts must
be clean. The command never switches branches or creates a commit from dirty
worker files for you.

Merging first creates a detached integration checkout. A successful merge has a
commit with the recorded target and worker commits as its two parents, plus the
operation UUID in its message. Only that verified result is fast-forwarded into
the target checkout. If the worker commit is already an ancestor of the target,
the merge is a no-op. Hooks, automatic stashing and recursive submodule operations
are disabled for lifecycle effects. No push is performed.

When a conflict occurs, the command returns `status: conflicted`, `operation_id`
and `stage_path`. The original target stays unchanged. Resolve files in that
stage checkout, run `git add` there, then continue the same operation:

```powershell
& $ccb worktree status <operation-id>
& $ccb worktree recover <operation-id>
```

Unexpected checkout changes, target movement, ownership changes and unresolved
conflicts block publication. Recovery observes the saved Git outcome before
continuing; it does not repeat an already published merge. A partially executed
Git operation that cannot be verified is left for explicit inspection, never
repaired with a hard reset or force removal.

## Recoverable retirement and final removal

```powershell
& $ccb worktree retire worker --dry-run
$retired = & $ccb worktree retire worker | ConvertFrom-Json
# Restore the entire checkout, including its staged/unstaged/private files:
& $ccb worktree restore $retired.operation_id
```

Retirement moves the complete registered checkout to a sibling directory ending
in `.ccb-retired-<UUID>`. Staged, unstaged, untracked and ignored files remain in
place. Its HEAD is pinned by `refs/ccb/archive/<UUID>` so Git garbage collection
cannot discard it. The old checkout path is fenced against automatic recreation.
Restore moves it back, preserving its Git index. If you commit inside the archive,
HEAD differs from the receipt and restoration is blocked pending inspection.

Permanent removal is separate and explicit:

```powershell
# Use a currently retired receipt, after its worker commit is merged:
& $ccb worktree purge <retirement-id> --into main --dry-run
& $ccb worktree purge <retirement-id> --into main
# Add this only when you also want the managed branch removed:
& $ccb worktree purge <retirement-id> --into main --delete-branch
```

Purge refuses modified, untracked or ignored user files and requires the archived
commit to be an ancestor of the explicit target branch. It uses ordinary
`git worktree remove`, without force. Branch deletion is opt-in and verifies that
the branch still points to the saved commit and is not checked out elsewhere;
the archive commit pin remains. Purged files cannot be restored with `restore`.
After final removal, remove that agent's worktree configuration or use a new
agent/worktree identity for the next task; the retired identity stays fenced.

## Interruption and operation IDs

Each mutation accepts `--operation-id <canonical-UUID>`. Repeating a completed
operation with the same arguments returns its receipt. For an unfinished one,
use `recover`; changing arguments under the same ID is rejected. `recover`
accepts only the original operation ID, not new mutation options.

Receipts live next to the runtime under `worktree-operations/<UUID>/operation.json`.
They are written before Git effects. The runtime writer lock, repository lifecycle
lock and existing materialization lock coordinate CCB processes. Recovery handles
process death after merge creation, target publication, checkout movement and
checkout removal, plus the intent/pointer and completed/pointer boundaries. Reading
an older completed receipt does not replace a newer operation's workspace pointer.

These are cooperative CCB locks. An independent Git command can still change a
repository; observed divergence blocks recovery. Access-denied observations or
incomplete Git moves/removals do not grant permission to overwrite or delete files.
No automatic agent drain, cross-repository controller migration, push or worktree
recreation is included in this offline workflow.

Development tests use temporary repositories and fixture providers. Crash tests
compile a small Git wrapper with `rustc`, run the real Git operation, pause before
CCB receives its result, terminate only that test CCB process, and recover using
the same receipt. They do not operate on the active sample-project project.
