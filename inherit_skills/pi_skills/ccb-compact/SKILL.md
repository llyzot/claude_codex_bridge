---
name: ccb-compact
description: Compact CCB agent conversation context with `ccb compact`. Use when the user writes /skill:ccb-compact, $ccb-compact or $ccb_compact, or asks to summarize one or more CCB agent contexts without restarting agents or deleting project state.
---

# CCB compact

Request provider-native context compaction for configured CCB agents.

Use the CCB executable and `--state-root` from the `command_prefix_argv` in your
`<ccb_interactive_coordination>` block; otherwise run `ccb` from the project
directory.

PowerShell:

```powershell
& '<ccb executable>' --state-root '<state root>' compact
& '<ccb executable>' --state-root '<state root>' compact '<agent>' '<agent2>'
```

POSIX shell:

```bash
'<ccb executable>' --state-root '<state root>' compact
'<ccb executable>' --state-root '<state root>' compact '<agent>' '<agent2>'
```

Rules:

- Without names it targets all configured agents; with names only those.
- Busy or queued agents are reported before any input is sent; providers
  without a verified compaction command are reported as unsupported.
- It does not delete `.ccb` state, workspaces, authentication, saved sessions,
  logs or memory.
- Run once, report the command output and stop. Do not poll or substitute
  `ccb clear`, restarts or `ccb kill`.
