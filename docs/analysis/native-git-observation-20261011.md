# Native Git review-tree observations

Rust now computes `git-tree:<object-format>:<tree-id>` identities through Git
without modifying the real index, refs or repository object database. The
observer uses a temporary index and object directory, with the existing object
database available as an alternate. Temporary Git objects are discarded after
observation; this is a content identity, not a retained checkout or an object
the caller can necessarily dereference later in the original repository.

The observer seeds HEAD, overlays the real index for the selected workspace
scope, and incorporates working-file changes and untracked files. Staged edits,
additions, deletions, renames, intent-to-add and explicitly staged ignored files
are included. This corrects a Python observer blind spot: with controller-state
filtering enabled, a staged-only change could be omitted because the old path
selection considered unstaged modifications against the real index.

Ignored operational paths match the controller's existing categories: `.ccb`,
`docs/plantree`, the workspace binding, Python bytecode/cache paths and the
named tool caches. Their HEAD content is retained while current operational
changes are excluded. `--include-controller-state` disables that filtering.
For a subdirectory workspace, changes outside that workspace remain at HEAD in
the identity. Git attributes and normal clean-filter semantics still apply;
configured filters are Git-managed programs and can have their own side effects.

Paths travel through NUL-delimited Git interfaces and literal pathspecs. The
implementation supports repository-selected SHA-1 and SHA-256 object formats.
It computes twice and requires the same result, then checks that HEAD and the
real index did not change. Unmerged indexes, selected skip-worktree/sparse entries
and dirty submodule contents are rejected rather than represented by a misleading
top-level tree ID. This is an observation, not a filesystem lock: edits after
capture remain possible and review/integration must verify the identity again.

## Integration

`ccb-native workspace-tree <path> [--include-controller-state]` exposes the
observer without starting a daemon. The kernel has a matching `--inspect-tree`
entry point and private `git.current_tree` operation. When the transitional
Python runtime selects Rust, `GitOperations.current_tree_digest` delegates to
the standalone native observer without starting another store writer or falling
back to Python on failure. Other Git integration operations still require
migration; the default Python mode remains unchanged.

Native restricted `ask --chain` admission now obtains the active parent's
workspace from authoritative Job/config state and computes its tree identity.
Caller-provided review path/digest claims are removed and replaced when binding
is requested. Parent chain policy is checked before observation, and
failed binding admits no child or callback edge. The verified fields are stored
with the child request and callback edge, and the provider prompt states the
workspace and identity to review. Native callback registration still checks
the required binding before committing admission.

Git observation is performed only when explicitly requested by the review-chain
policy or inspection command. The current authority performs that observation
synchronously during admission; large repositories or slow configured filters
can increase admission latency. No timeout is treated as proof of completion,
and no automatic task replay is added. Performance and async observation design
remain part of the broader migration gates.

## Evidence and limits

Tests compare observed identities with Git's own trees, check source index/ref/
object-store preservation, exercise staged-only cases missed by the old Python
algorithm, and compare unchanged/unstaged cases with the existing observer.
Coverage includes Unicode/literal filenames, operational paths, linked
worktrees, subdirectory scope, SHA-256 repositories, unmerged/sparse/submodule
refusal, native Python delegation, forged-claim removal, atomic failed
admission and review context reaching the native provider prompt.

Initial failures came from test setup: a merge lacked fixture committer identity
and therefore did not create the intended conflict, and Git for Windows required
a relative submodule destination. Both setups now verify the intended state.
This checkpoint performs no install/default-runtime switch and does not claim
that all workgroup integration/controller logic is already native.

Final validation passed all 26 new observation cases and all 82 selected
observation/admission/task/daemon/finalization cases. Rust tests (11), formatting
and Clippy with warnings denied passed; both repository binary hashes match
release outputs. The full 508-case native inventory was not run.
