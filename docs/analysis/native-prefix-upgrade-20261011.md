# Native prefix upgrade and recovery — 2026-10-11

The Windows Rust installer can explicitly upgrade an existing native prefix,
retain the previous program directory, recover an interrupted cutover, and roll
back its latest recorded upgrade. This applies to the native schema-1 fixed
program inventory. It does not convert a Python installation or migrate project
state. The account installation and default runtime were not changed here.

## Using the new build

Stop the native commands/services using the selected installation, then run the
new build from outside that prefix. It never stops a service automatically.

```powershell
.\bin\ccb-native.exe installation upgrade --prefix <existing-native-prefix> --dry-run
.\bin\ccb-native.exe installation upgrade --prefix <existing-native-prefix>
.\bin\ccb-native.exe installation status --prefix <existing-native-prefix>
```

The PowerShell entry point supports the same explicit selection:

```powershell
.\install.ps1 install -Runtime rust -InstallPrefix <existing-native-prefix> -Upgrade -DryRun
.\install.ps1 install -Runtime rust -InstallPrefix <existing-native-prefix> -Upgrade
```

Ordinary `install` still refuses to replace a different build. The script checks
the payload's upgrade capability before using it; Python mode rejects `-Upgrade`.
No Python/venv, account PATH change, provider configuration change or network
download is needed by the native cutover itself. Source archives keep their
existing explicit checksum validation.

`--operation-id <uuid>` makes a specific upgrade retryable after a lost response.
`--expected-manifest <sha256>` binds it to the exact old manifest from the plan's
`from.manifest_sha256`. A retry of the same completed operation accepts its
original precondition and returns the recorded result. Reusing an identity with
a different source/precondition fails. Repeating an upgrade after its rollback
requires a new identity. The dry run verifies content/ownership but defers the
active-use exclusion to apply; it creates no stage or transaction record.

## Cutover and Windows exclusion

The installer requires the fixed native files and rejects extra/missing entries,
changed hashes, links/reparse points, or unverified manifests. It shares the
existing per-prefix installation lock with fresh installation. Windows volume
and file identities recognize hard-linked command aliases. A mapped/running
executable or incompatible file handle blocks mutation; an independent prefix
can continue running.

The new program directory is fully built and verified alongside the destination.
Its old and new identities are written to a bounded transaction record before
directory changes. Paths are derived from the selected canonical parent, prefix
name and UUID; the journal cannot supply arbitrary move destinations. Moves use
no-replacement semantics. No recursive directory deletion is performed.

Windows rejects directory rename while ordinary child file handles remain open,
even with delete sharing. It also permits directory rename of a mapped executable,
so directory rename alone is not evidence that the program stopped. The native
implementation first excludes active images, then holds writable file mappings
as launch barriers while briefly closing ordinary handles for rename. It never
writes through those mappings. Real executable tests confirm that launching an
alias is denied before and after rename, bytes stay unchanged, and launch works
after release. Files/identities/hashes are rechecked after each directory move.
This coordinates the installer and command launch; it is not a security boundary
against a user independently modifying installer-owned files.

Forward phases are `prepared → old_retired → committed`. The old directory is
retained at the returned backup path. A process failure can leave the selected
prefix temporarily absent; the verified old and new directories remain available
for recovery. Completed directory moves may precede their next journal update.

## Recovering and rolling back

```powershell
.\bin\ccb-native.exe installation recover --prefix <existing-native-prefix> --dry-run
.\bin\ccb-native.exe installation recover --prefix <existing-native-prefix>
.\bin\ccb-native.exe installation rollback --prefix <existing-native-prefix> --operation-id <uuid>
```

Use an external native executable for operations that need to change this prefix.
Recovery checks actual old/new directory identities and resumes the recorded
direction; it never guesses from age or PID alone. New native runtime commands
refuse a pending recorded cutover, while metadata and installation-management
commands remain available. Fresh install cannot fill the gap in a pending swap.
Status is read-only and may report files unavailable while another installer holds
them. A corrupt, foreign or changed transaction is preserved for explicit repair.

Rollback is `rollback_prepared → new_retired → rolled_back`. It restores the old
program directory and retains the replacement at the returned stage path. Only
the latest recorded transaction is automatically managed. Older retained copies
and stages orphaned before a journal was written are not adopted or deleted.

**Rollback restores program files only.** Project/runtime files are outside this
inventory and are untouched. The output explicitly reports
`state_compatibility: not_checked`; assess the old build's ability to read current
project state before starting it. This is not a database rollback, a signed-release
authenticity check or a general mixed-version migration guarantee.

## Validation scope

Tests use private temporary prefixes. They exercise complete upgrade/rollback,
alias execution, dry runs and stale preconditions, retained file equality,
running-image rejection, independent live prefixes, concurrent installers,
tampered/extra/foreign inputs, relative transaction recovery after parent
relocation, pending-launch fences and the PowerShell entry point.

Eight directory/journal boundary layouts are recovered explicitly. A Rust test
also runs the real cutover in child processes and kills them after each forward
directory rename, before the next journal update; recovery then completes the
upgrade. Crash hooks exist only in test builds. Another OS-level test verifies
the mapping launch barrier and byte preservation. Process-death recovery is
tested; power-loss durability and all filesystem variants are not claimed.

Program removal, orphan/retained-version cleanup, authenticated release signing,
project-state rollback and production/default promotion remain open work.

Final verification: all 79 selected upgrade/install/lock-recovery/CLI-security/
startup/release-builder cases passed in 70.43 seconds, including 23 dedicated
upgrade cases. Rust tests passed 13 cases; its normally ignored crash-worker
helper was explicitly executed by the process-death parent test. Formatting and
clippy with warnings denied passed. The 747-case curated native inventory was
collected, not fully executed; the last complete run remains the recorded
613-case baseline. Two additionally attempted legacy Bash release-entry tests
failed before sourcing `install.sh` because they pass Windows paths to Bash;
they are not included in the successful native regression claim.
