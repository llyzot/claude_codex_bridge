---
name: ccb-clear
description: Clear CCB agent conversation context with `ccb clear`. Use when the user writes /skill:ccb-clear, $ccb-clear or $ccb_clear, or asks to clear or reset one or more CCB agent contexts without restarting agents or deleting project state.
---

# CCB clear

Clear provider conversation context for configured CCB agents.

Use the CCB executable and `--state-root` from the `command_prefix_argv` in your
`<ccb_interactive_coordination>` block; otherwise run `ccb` from the project
directory.

PowerShell:

```powershell
& '<ccb executable>' --state-root '<state root>' clear
& '<ccb executable>' --state-root '<state root>' clear '<agent>' '<agent2>'
```

POSIX shell:

```bash
'<ccb executable>' --state-root '<state root>' clear
'<ccb executable>' --state-root '<state root>' clear '<agent>' '<agent2>'
```

Rules:

- Without names it targets all configured agents; with names only those.
- It starts a fresh provider conversation for each target. It does not delete
  `.ccb` state, workspaces, authentication, saved sessions, logs or memory.
- Busy agents are reported, not interrupted.
- Do not use `ccb kill`, restarts or `ccb herdr repair` unless the user
  explicitly asks for a runtime reset.
- Run once, report the command output and stop. Do not poll.
