# Native held-agent repair

When a stopped project is recovered with `ccb herdr open --recover-stopped`, each
agent resumes only its verified native session. If that evidence is missing,
corrupt or ambiguous, for example a deleted Codex rollout or Claude transcript,
the agent is **held**. `ccb agents` reports `activation.reason:
recovery_evidence_required`. A held agent receives no work, and its unfinished
execution stays `running`. CCB never replaces the session silently and never
replays the task.

A held agent stays held across later recoveries until an operator decides what
to do. If the evidence can be restored, for example by putting the history file
back, simply recover again. Otherwise, explicitly accept the loss and start a
fresh session.

## Fresh-session repair in pwsh

The project must be fully stopped. The command does not stop it for you.

```powershell
ccb kill
ccb herdr repair worker --fresh-session --dry-run
ccb herdr repair worker --fresh-session
ccb herdr open --recover-stopped
```

`--dry-run` reports:

- the agent;
- the recorded hold reason (`prior_error`);
- every unfinished execution that will be closed (`abandoned_executions`).

It changes nothing.

The real command records a durable intent in `.ccb/ccbd/herdr/operator-repairs.json`
and an audit row in `operator-repairs.jsonl`. Repeating it returns the same
`operation_id`.

The next stopped-server recovery consumes the intent. It then:

- closes each abandoned execution as `incomplete`, with reason
  `operator_fresh_session` (or `cancelled` if cancellation was already
  requested). Callers receive that result. The task is never resent, and work
  may already have taken effect, so inspect the project before an explicit
  retry;
- drops the agent's cached history binding and launches it without a resume
  selector, so the provider starts a new native session that is then verified
  normally;
- appends an `applied` audit row with the recovery ID and removes the intent.

## Refusals

The command refuses when:

- the agent is not held;
- the daemon, Keeper or a Herdr opener is still active;
- the agent has a blocked provider fault (use provider recovery instead);
- an unfinished execution is an uncertain reply delivery, which still requires
  explicit recovery.

Repair applies only to the agent you name. Other held agents stay held.
