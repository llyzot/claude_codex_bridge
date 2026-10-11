# Native Herdr Codex task dispatch — 2026-10-11

The experimental native daemon can now dispatch queued tasks to a previously
mounted Codex terminal Agent through Herdr and deliver its verified result through
the existing Rust mailbox/callback machinery. A persistent Pi RPC Lead can finish
its initial turn and later receive a continuation in the same process and session.
This checkpoint connects task execution to the earlier Herdr mounting foundation;
it does not yet provide a complete native `ccb herdr open` workflow.

The subsequent [Pi terminal checkpoint](native-pi-terminal-20261011.md) also
supports a Pi terminal Lead and idle extension reload. The Codex behavior below
remains valid; its original Codex-only limitation describes this earlier checkpoint.

## Execution and completion

The daemon configuration opts an Agent into `driver_kind: "herdr_tui"`. Currently
this driver supports only `provider: "codex"`, with `command: null`: Herdr owns the
provider process. The daemon has no process-tree termination rights for that Agent.
Queued cancellation remains available; active interruption, native clear/compact
and native provider restart are explicitly unsupported for this driver.

The dispatch sequence is:

1. Claim the existing FIFO head and journal the execution UUID.
2. On a background thread, validate the saved native Herdr namespace, mount plan,
   unique alias, pane, terminal and Windows PID creation identity. Require an
   interactive-ready Agent with an explicitly reported matching Codex session ID.
3. Read the exact configured rollout header and save its file identity, byte
   offset and boundary digest. No directory discovery or newest-file guessing is
   performed. The exact future path may be absent if the session ID is already
   known; its eventual header must still match.
4. Ask the authority actor to commit `provider_options.herdr_dispatch` and the
   `herdr_prompt_permitted` event. Only its post-commit grant allows sending.
5. Recheck the live binding and readiness, then issue one `agent prompt` with an
   exact first-line `CCB_REQ_ID: <job-id>:<execution-uuid>` marker. No `--wait` or
   raw pane input is used. Failed/lost CLI acknowledgement never permits replay.
6. Observe complete UTF-8 JSONL records from the saved offset. Require a new
   `task_started` turn, the exact request marker, and a matching-turn
   `task_complete` or `turn_aborted`. Ignore foreign turns, subagent output and
   commentary. A nonempty terminal error cannot become success. Prefer the
   terminal `last_agent_message`, otherwise an explicit `final_answer`.
7. Commit completion through the existing result policy, reply mailbox and chain
   continuation machinery. The result may be delivered before the Worker returns
   to idle, but the next FIFO request waits for fresh readiness.

Transport subprocesses and history observation run outside the single-writer
actor, so slow CLI acknowledgements do not block requests or Lead execution.
Background observers never write the kernel journal themselves. Native mount
observation is read-only and does not acquire a second journal writer.

## Recovery and limits

A controller crash leaves Herdr-owned providers alive. On restart, a running task
with a saved dispatch permit is observed from its original baseline; no prompt is
sent again. A task with no permit can safely resume preflight because the prior
worker never received send permission. File replacement, truncation, an altered
baseline boundary, a mismatched header/session, process replacement or an
ambiguous new turn holds the task and FIFO instead of inventing a result.

If completion was committed just before a controller crash but the provider is
still busy, the next request resumes asynchronous readiness preflight. Losing
the old in-memory observer does not send the next prompt or fail it merely
because the preceding turn has not returned to idle yet.

The permit-before-send gap is deliberately conservative: if the controller dies
after committing permission but before sending, observation may never find a
result. The system cannot guarantee exactly-once delivery or unconditional
automatic progress without a provider-side idempotent request protocol. A timeout
holds the uncertain task; it does not report a fabricated cancellation or retry.
Preflight failures before any grant are instead recorded as known-unsent failures.

For a faulted, still-running permitted execution, the authenticated API-2
`herdr-reobserve` operation accepts `agent_name`, `job_id` and
`execution_instance_id`. It clears the observation fault and reads the same
history again; it never resends the prompt. It is also accessible through the
native CLI's generic `request` command. Recovery of an idle fault after a known
terminal result, replacement/migration of an external provider, and operator
resolution of a permanently absent result remain further work.

History observation is bounded: the header is limited to a 4 MiB record, each
poll consumes approximately 1 MiB plus at most one record, and records have a
4 MiB limit. Baselines hash only the last 64 KiB before the offset plus the offset
itself, not the entire prior history. This verifies append-boundary continuity,
not arbitrary historical-file tampering. No provider profile/resource snapshot
is made. Original role memory can be composed into a prompt; explicit native
role-skill loading in a Herdr terminal is rejected before sending. Windows prompt
transport currently limits the composed input to 8,000 UTF-16 code units.

The public Herdr contract has no atomic compare-and-send process generation
precondition. A live binding can still change between the last observation and
the server's prompt operation. Unique mount names and process lifetime checks
reduce accidental misdelivery but do not remove this protocol-level race.

## Configuration and minimum offline test

This is a low-level integration, requiring an already prepared/mounted native
namespace and an explicitly bound Codex session. Global `config.herdr` holds the
same request used to create that namespace and mount plan. Its nested `config`
contains the terminal-mounted Agents, while the daemon's top-level `agents` can
also contain an independent Pi RPC Lead. Project IDs, project roots and Worker
workspaces must match. Do not replace the nested mount plan with the mixed daemon
configuration after mounting.

An opted-in Worker has this shape (paths and session ID are illustrative):

```json
{
  "provider": "codex",
  "driver_kind": "herdr_tui",
  "command": null,
  "workspace_path": "C:/test/project",
  "timeout_s": 3600,
  "herdr_history": {
    "root": "C:/test/codex-home/sessions",
    "path": "C:/test/codex-home/sessions/rollout.jsonl",
    "session_id": "the-exact-reported-session-id",
    "cwd": "C:/test/project"
  }
}
```

After building the repository release binaries, set `CCB_TEST_KERNEL_BIN` and
`CCB_TEST_NATIVE_BIN` to those absolute paths and run:

```powershell
python -m pytest test/test_native_herdr_worker.py -q
```

The test uses disposable project directories, a persistent offline Herdr protocol
fixture and harmless pytest-owned processes for real Windows lifetime checks.
Pi also uses a protocol fixture. It requires no model credentials, contacts no
real Herdr server, and leaves production installation and existing Agents alone.
Fixtures exercise the native actor, journal, transport subprocesses, log reader,
mailbox and callback delivery rather than replacing those core components.

Live Herdr/provider acceptance, original Pi/Claude terminal task adapters,
automatic new-session discovery, full `herdr open` composition, graphical input
handling, polling costs and production/default promotion remain unverified or
unfinished. See the progress JSON for exact tested binaries and regression counts.

Final release validation passed all 15 dedicated Worker integration cases in
105.84 seconds and 22 Rust unit tests (the ignored subprocess helper is exercised
by its parent test). Formatting and clippy with warnings denied passed. An earlier
37-case Worker/role-memory/role-delivery run passed; all 148 pre-existing cases in
the broader Herdr/daemon/Pi/view/reload selection also passed. The corrected
oversized-input fixture and the final readiness-crash regression are included in
the final 15. No full curated-suite rerun or live-provider acceptance is claimed.
