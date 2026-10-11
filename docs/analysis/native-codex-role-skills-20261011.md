# Native Codex role skill registration

The Rust authority now resolves Codex role skill declarations directly to their
live directories. Before sending a task requiring role skills to its owned Codex process,
it replaces that process's extra skill roots, waits for the matching response,
forces skill discovery to reload, and verifies that each declared `SKILL.md`
is reported at the expected path with `enabled: true`. Only then is `turn/start`
sent. A prior nonempty registration is explicitly cleared when declarations
become empty. Each Agent owns a separate app-server process.

This uses the installed Codex 0.162.1 protocol's `skills/extraRoots/set` and
`skills/list` methods. Its generated `SkillsListParams` exposes `cwds` and
`forceReload`; the implementation does not assume newer optional fields in
online documentation. The extra-root operation is process-local and does not
persist configuration. See the [official app-server reference](https://learn.chatgpt.com/docs/app-server).

`rolepack/v1` uses `skills.codex`. Preview manifests combine `contents.skills`
and CCB-adapter `skills`, in that order, for the adapter's supported/recommended
providers (default Codex and Claude). Exact duplicate directories are removed.
Each declaration must stay within the selected role root, name a directory,
and contain a `SKILL.md` inside that directory. The declaration limit is 64.
The provider parses skill frontmatter and handles invocation semantics; CCB
does not rewrite skill documents into memory or create provider skill copies.

Registration and discovery responses are correlated to the current native
execution. Foreign responses and old turn notifications cannot release the
pending prompt. A rejected registration, missing/disabled skill, or unusable
discovery result fails the current task before model input, without automatic
retry or destruction of a healthy provider session. Invalid response envelopes
quarantine the uncertain provider through the existing fault mechanism.

Cancellation/deadlines during registration wait for the outstanding operation's
response before freeing the Agent slot. The queued prompt is then discarded,
with a cancelled decision identifying that no turn input was sent. This avoids
a late root mutation crossing into the next task. Existing owned-process
termination and cold recovery remain the escape paths for a nonresponsive
provider; observation expiry alone does not restart or replay it.

## Evidence and current limit

Protocol fixtures cover both role formats, live refresh/removal, adapter order
and provider filtering, registration/discovery errors, malformed responses,
foreign responses, task isolation, explicit/silent cancellation, deadlines and
same-session recovery. A separate installed-CLI probe, using a private home
without model calls, registers the actual directory, observes a changed skill
description after forced reload, clears the roots and verifies removal. It
does not write the user's settings or installed skill directories.

A real-model test was also attempted with the native daemon, a private Codex
home and a read-only workspace. Registration was confirmed; the skill appeared
in the actual model-facing skill catalogue. The model's PowerShell read of
`SKILL.md` was rejected with `blocked by policy`. Consequently the test that
requires reading a linked token file and then re-reading changed skill text in
the same thread **has not passed**. It remains an opt-in test in
`test_native_real_models.py`; this checkpoint does not substitute catalogue
visibility for verified skill-body execution or relax that test's permissions.

Pi and Claude native role-skill integration, role permissions/tool hooks and
activation policies remain migration work. The installed Pi RPC implementation
has no direct resource-reload command. Claude's documented skill-directory
watching requires a separate provider integration; see
[Claude skill updates](https://code.claude.com/docs/en/skills#update-a-skill).
Full default Rust ownership, native installation and wider migration gates
remain open. No production installation or existing Workspace was changed.

Validation: 114 affected role-memory/Codex/control/request/fault cases passed.
After optimizing discovery-path verification and rebuilding, all 23 final
role-skill cases passed, including the installed-CLI catalogue probe and the
Pi Lead callback test. Rust tests (7), formatting and Clippy with warnings
denied passed; repository binary hashes match release outputs. The separate
real-model skill-body test failed for the policy rejection described above.
This is focused regression, not a full 367-case inventory run.
