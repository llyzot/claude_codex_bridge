---
name: ask
description: Send a request to another CCB agent with `ask`. Use when the user asks you to delegate to, consult, or hand off work to a configured CCB peer, or when a CCB task or project memory says to use CCB ask.
---

# CCB ask

Use this when the user asks you to delegate with CCB, or when a CCB-delivered
task or project memory says to use CCB `ask` for collaboration. Contact the
existing peer agent; do not substitute pi-subagents or launch another CLI.

## Command form

Your context contains a `<ccb_interactive_coordination>` block. Use exactly its
`command_prefix_argv` (CCB executable and `--state-root`) and your own
`agent_name` as `--from`. Put the message after `--` as one argument, so the
same form works in PowerShell and POSIX shells.

PowerShell:

```powershell
& '<ccb executable>' --state-root '<state root>' ask --from '<your agent name>' '<TARGET>' -- '<MESSAGE>'
```

POSIX shell:

```bash
'<ccb executable>' --state-root '<state root>' ask --from '<your agent name>' '<TARGET>' -- '<MESSAGE>'
```

Quote the message as a single argument (in PowerShell double any `'` inside
single quotes). Insert selected flags after `ask`, before `--from`, for
example `ask --chain --artifact-reply --from ...`. If no coordination block is
present, use plain `ask '<TARGET>' -- '<MESSAGE>'`.

For a very long message, prefer `--artifact-request` with repo paths the target
can read. A message given as arguments is the whole message; `ask` reads stdin
only when no message argument is given, and refuses a stdin that stays open
without input for 60 seconds.

## Decision card

Before every ask, decide:

1. Need delegation? If not, answer directly.
2. Dependency: default to no `--chain`. Use `--chain` only when the current
   active CCB task cannot finish until this exact child result arrives. Follow
   the `required_result_mode` of a newer `ccb_runtime_coordination` binding when
   one is present.
3. Result intent:
   - `--silence`: execute only; success result not needed (failures and
     blockers still surface).
   - `--compact`: only distilled findings, status, risks and next actions.
   - add `--artifact-reply` when the full report should be preserved.
   - plain `ask`: short question or handoff.
4. Request fidelity: add `--artifact-request` for exact transient input (logs,
   diffs, configs, structured text); `--artifact-io` when request and reply
   both need artifacts.

## Guardrails

- Submit once, then end your turn. Do not wait, poll, or run `ask get`,
  `pend`, `watch` or `ping`; CCB delivers the result back to this conversation.
- Never add `--chain` just to make a rejected plain ask succeed. If CCB says
  `ask --chain requires an active parent job`, retry once with plain `ask` for
  user-requested delegation, otherwise report the error.
- Avoid mixing `--chain` with `--silence`, and `--silence` with
  `--artifact-reply`.
- If the current task is a CCB result-chain continuation or reply delivery,
  answer it directly; do not `ask` the original caller to receive the result.
- Give the receiver only its stage: task, inputs, constraints, expected
  reply. A `completed` job only means the provider finished; report its status
  and verification.
- Correct an active job only with `ccb followup <job_id> --message "..."` when
  the target supports it; on `rejected`, `too_late` or `terminal`, cancel and
  resubmit the complete task.
