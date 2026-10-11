# Native stopped-server recovery checkpoint

`ccb-native herdr open --recover-stopped` explicitly recovers a quiescent,
previously CCB-created Herdr session. It verifies the former server and provider
process lifetimes have ended, refuses unfinished jobs or delivery state, validates
native history, and journals the recovery before starting a new server. It retires
only the recorded restored workspace after checking its identity, panes and idle
occupants. Unrelated workspaces and duplicate live native sessions are protected.
Old CCB namespace/mount records are archived; native conversation files remain.

The new workspace resumes Pi through its exact native session path and Codex
through its verified UUID. A Codex agent without any previous task/history can
start fresh. Existing explicit startup session selectors require a manual plan.
Uncertain close receipts are observed without blindly repeating the operation.
This does not recover busy or ambiguous jobs, preserve custom manual pane layout,
or offer atomic protection against concurrent external Herdr changes.

## Real Windows result

The original Windows Terminal/pwsh launcher recovered the stopped test session
`ccb-rust-foreground-c43bdcf5` from workspace `w1` to `w2`. Pi resumed session
`01a12b99-ca02-774f-8690-bbf38c3f0bd3` and recalled `FOREGROUND_READY_OK` without
reading files or using tools. Its response included `RECOVERY_MEMORY_OK`.
The machine-readable evidence is in
[native-herdr-stopped-recovery-20261012.json](native-herdr-stopped-recovery-20261012.json).

The subsequent recovered Pi-to-Codex chain **did not pass**. Codex exited and
the submitted task text appeared at the PowerShell prompt; PowerShell reported
the arithmetic request words as unknown commands. No matching Codex rollout was
found. The native runtime quarantined the worker execution as
`provider_protocol_error`, retained the parent callback dependency and did not
resend. The cause of Codex's exit is unconfirmed. This is evidence of a terminal
input/foreground-process race, not successful result delivery. Eliminating that
race requires an atomic Herdr dispatch guarantee or a provider-native input
channel; an additional client-side process check alone is insufficient.

The opener also refused a recovery launched from the tool shell because its
synthesized environment differed. Reusing the original pwsh launcher succeeded.
The request signature currently includes inherited PATH; cross-shell environment
migration is not implicit.

## Verification and rollout boundary

Eight dedicated offline recovery cases passed, including exact Codex history
resume, unresolved work refusal, lost close acknowledgement, controller death
after close, missing history, foreign restored agents, extra panes and duplicate
native sessions elsewhere. One earlier test teardown used the Windows locale
instead of UTF-8; explicit UTF-8 fixed that fixture error.

Rust unit tests passed (25, plus one helper invoked by its parent), formatting and
Clippy with warnings denied passed. All 73 selected opener/mount/namespace regression cases passed in 245.90 seconds. This is not a full repository test run.

Only the disposable test prefix/session was used. The production installation
and account PATH were not changed. Original user workspaces and agents were not
controlled. The unfinished test job and callback remain recorded for diagnosis.
The overall Rust migration remains incomplete; production promotion must not use
this checkpoint as proof of reliable recovered terminal dispatch.
