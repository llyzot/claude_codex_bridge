# Native Claude role skills and retained reply recovery

The native Claude transport now registers a per-Agent skill discovery directory
with `--add-dir` and refreshes it through the stream control protocol's
`reload_skills` request before sending task input. The directory contains live
references to the declared original skill directories: Windows directory
junctions, or Unix directory symlinks. CCB does not copy the skill bodies or
their supporting trees and does not run an installer.

Rust owns initialization, filesystem references, refresh correlation, task
admission, cancellation and completion. The installed Claude Code 2.1.296
protocol was checked against its actual responses and the publisher's matching
Agent SDK 0.3.296 type declarations. No SDK package is a runtime dependency.
The relevant operations are `initialize` and `reload_skills`; user/project
settings files are not rewritten. See the
[official SDK reference](https://platform.claude.com/docs/en/agent-sdk/typescript)
and [Claude skill documentation](https://code.claude.com/docs/en/skills).

## Discovery ownership and verification

The native provider state contains `role-view.json`, an ownership/intent ledger,
and `role-view/.claude/skills`, containing generated directory references.
Aliases derive from a persistent random owner and each canonical source path.
Changing a file updates the original file seen through the reference; changing
the declared directory set updates only owned references.

The ledger records intended ownership before creating, publishing or removing
references. Interrupted temporary links can be reconciled. A new owned Claude
process clears stale owned entries and rebinds current declarations before its
first task. Unmarked entries, substituted normal directories and changed link
targets fail closed rather than being overwritten or recursively deleted.
Removal operates on the link itself, never on the original directory. Windows
creation uses native reparse-point APIs without an elevated shell or symlink
privilege; this implementation requires local-drive targets on Windows.

Claude's control response exposes command names and aliases, not source paths.
CCB therefore combines its verified filesystem references with exact generated
aliases in the refreshed catalogue. Missing, duplicate, stale or built-in
matches reject task input. A role command whose public name conflicts with
another command is also rejected. The native catalogue remains responsible for
visibility and invocation policy; CCB does not insert another skill catalogue
into the task prompt, including for user-only skills. Explicit skill `name`
frontmatter retains the native name. Without it, Claude's directory-name
fallback uses the generated discovery alias.

Source settings are respected. If the caller disables the relevant skill
discovery, CCB reports failure instead of changing those settings. Skill roots
containing plugin manifests, MCP/LSP configuration, hook configuration or
plugin settings are rejected for now; they require the separate role-hook and
permission migration. The discovery view's extra directory is scoped to that
view, not to the whole user role store. Actual tool permissions remain with
Claude and are not bypassed by the controller.

## Refresh and result handling

Initialization must acknowledge the owned control request. Each task then
waits for its correlated skill-refresh response before any user-message input.
Cancellation/deadlines during refresh wait for that response and discard the
pending task input; they do not issue a global interrupt into resource loading.
Foreign responses cannot release the prompt. Malformed responses quarantine
the provider, while normal discovery failures remain task-local.

The shared result-delivery path also gained a durable hold. Previously, a
failed ordinary reply-delivery attempt retained its mailbox head but could
immediately materialize another attempt on every tick. A failure now commits
the terminal attempt, retained original reply and recovery hold together.
This applies to native Pi, Codex and Claude receivers. Existing recovery-phase
fault values are retained, with additional evidence fields.

After repairing configuration, `provider-recover` with the exact fault ID can
keep the live runtime only when the recorded native role preflight proves that
no task prompt was sent, the durable terminal execution matches the fault,
the same owned runtime is ready and no execution remains active. Uncertain
executions keep the existing closed-process/recovery-scope requirements. The
hold survives daemon restart and never discards the original Worker reply.

## Evidence and remaining work

Windows unit tests exercise live references, source-preserving unlink,
interrupted-link recovery and refusal to delete substituted user content.
Protocol fixtures cover add/remove/refresh, collisions,
user-only metadata, cancellation, initialization failure, malformed/foreign
responses, automatic Lead callback and saved-session restoration. Two actual
installed-CLI probes, with and without bare mode, read the Rust-created links,
observe changed descriptions and confirm removal without model inference.

Final affected-path runs passed 28 new Claude/reply-delivery cases and 106
existing stream/control/fault/recovery/lifetime/task cases. An earlier run
passed 38 Claude/role-memory cases. All 11 Rust tests, formatting and Clippy
with warnings denied passed. Repository binaries match final release outputs.
The initial link tests exposed DOS versus extended Windows path spelling;
normalizing only that prefix fixed comparison without resolving or deleting
the source tree. This is focused regression, not a full native inventory run.

Claude authentication still reports `loggedIn: false`, so successful Claude
model invocation, skill-body execution and real Lead review are not claimed.
The previously recorded Codex skill-read policy rejection also remains open.
Unix link behavior, production extension interactions, role permissions and
activation/hooks, Herdr integration, default native installation and the other
migration gates still require work. Production installation and existing
Workspace/Agent processes were not changed.
