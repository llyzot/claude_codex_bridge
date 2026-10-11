# Real Windows Herdr validation — 2026-10-11

The Rust runtime completed real Pi Lead / Codex Worker collaboration in an
independent Windows 11 project and named Herdr session. A roughly 35.2-second
Worker task automatically returned to the original Pi session: Pi ended its first
turn roughly 30.6 seconds before Worker completion, then reviewed the delivered
result. Its initial tool call was asynchronous native `ask`, without polling.

Machine-readable job IDs, timestamps, callback edges, process lifetimes and scope
facts are in [the evidence file](native-herdr-live-20261011.json). It records the
actual sessions and model results, not fixture-generated replies.

## Setup and scope

- Herdr 0.9.0, protocol 22, dedicated session `ccb-rust-live-6586638a`.
- A fresh native prefix and Git project under the recorded trial directory.
- Pi `gpt-6-astra` Lead and Codex `gpt-6.1-sol` Worker, both with low reasoning.
- pwsh panes, plus a separately launched Windows Terminal/pwsh Herdr client.
- Codex was configured with read-only sandbox and `never` approval policy.

The prior human instruction expressly authorized running this combination in an
independent Windows Terminal/pwsh test directory. The caller was outside Herdr;
that flag was not forged. Every live control named the new test session and its
owned Agents. Existing user workspaces/Agents and the production CCB prefix were
not controlled or replaced. The new test repository's Codex folder-trust prompt
was accepted; that scoped trust decision was saved. Normal provider session logs
were created in their existing homes. No credentials were copied and no account
PATH or provider authentication settings were manually edited.

## Actual failures found and corrected

The real server-status query returns a flat `status: not_running`/`running: false`
object. The old fixture returned a nested object, concealing that the generic
transport rejected valid status data. The fixture now reproduces the actual shape.
Transport accepts consistent running/not-running data only for the explicit
`status server --json` query; errors and inconsistent state remain failures, and
the same status strings do not turn a mutation into success.

A fresh Codex TUI had a native session ID but no rollout file before the first
model request, and Herdr had not yet reported that ID. Requiring a persisted
history before starting the daemon therefore deadlocked ordinary fresh startup.
The runtime now supports deferred Codex history binding:

1. Validate the exact mounted Herdr alias, namespace, terminal and Windows process
   lifetime, and commit a permission for the real task input.
2. Send that input once, with a job/execution-specific request marker. No warm-up
   model request, hook-trust bypass or profile copy is used.
3. Discover recent `.jsonl` candidates inside the declared Codex session root.
   Validate their session header, UUID and exact workspace, exclude subagents,
   and use the existing structured turn/anchor/completion reader. An optional
   expected session UUID narrows this further. Modified time is a bounded search
   filter, not completion authority.
4. Complete only the correlated turn. Multiple observed matching histories hold
   the task as ambiguous. Persist the resulting binding through the single-writer
   actor for later tasks. Recovered permitted tasks only observe, never resend.

Discovery is bounded to 20,000 filesystem entries, four nested levels and 32
recent matching-workspace candidates. Cached history is preferred on later tasks;
when native session metadata is missing, discovery can still locate a subsequent
session instead of assuming the old cache remains current. This is cooperative
process/routing/nonce evidence, not protection against a same-user adversary
forging provider logs. Discovery limits or unprovable outcomes remain holds.

## Live results

- Arithmetic chain: Codex returned `WORKER_RESULT_391`; the same Pi process and
  session automatically returned a review containing `REVIEW_OK_391`.
- Controller restart: a running Codex turn continued while the Rust daemon was
  stopped and reopened. The unchanged Codex process produced `RECOVERY_LIVE_OK`.
  Its transcript contains one matching user request, not a replay.
- Long task: Worker took 35.233 seconds. Pi had released its initial turn 30.627
  seconds before Worker finished. The callback completed with `LONG_REVIEW_OK`.
- Three callback edges reached `done`. Pi retained its own configured formatting
  prefixes; the runtime preserved those responses rather than rewriting them.

One attempted long test used a shell sleep. Codex's shell helper failed with
`helper_unknown_error`; the failure description was nevertheless delivered and Pi
correctly reported failed verification. No sandbox policy was relaxed. The cause
of that tool-process failure remains to be isolated. The long successful test used
no Worker tools, so it does not prove coding-tool execution in that sandbox.

## Cleanup and limits

The test daemon and only this named Herdr server were stopped. The original Pi,
Codex, server, client and final daemon PIDs were then no longer observable. Trial
files, logs and native-prefix backups remain available for review.

Windows Terminal client launch and its running Herdr attach process were observed;
there was no pixel-level inspection, and the native CLI's automatic foreground
attach branch was not the path used to create that client. Stopped-server recovery,
the shell-helper failure, original Claude terminals, complete configuration parity
and default/production migration remain unfinished. This checkpoint proves the
real asynchronous result/continuation path, not completion of the entire refactor.
