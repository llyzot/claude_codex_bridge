# Native provider protocol checkpoint

This checkpoint implements owned Pi RPC, Codex app-server and Claude stream processes. Existing Herdr Workspace/Agents, installed CCB, user provider settings and `codex_impl` were not controlled or changed.

## Local primary sources

- Installed Pi package: `@earendil-works/pi-coding-agent 1.1.0`. Its `docs/rpc.md`, `docs/rpc-commands.md`, `docs/json.md` and `dist/modes/rpc/rpc-mode.js` define prompt dispositions, LF-framed events, session state and `agent_settled`.
- Installed Codex executable: `codex-cli 0.162.1`. Its `app-server --help` confirms stdio transport. `app-server generate-json-schema` was run into a unique temporary directory, without starting a model task. The generated InitializeParams, ThreadStart/ResumeParams, TurnStartParams, TurnCompletedNotification, ItemCompletedNotification, ThreadReadParams/Response and TurnInterruptParams schemas were inspected directly.

## Codex driver

`driver_kind: "codex_app_server"` requires `provider: "codex"` and a command argv that starts an app-server. Initialization and thread binding use ID-correlated responses. Saved non-ephemeral thread IDs resume through `thread/resume`; a different returned ID is rejected. Configured `thread_options` and `turn_options` are forwarded; the driver does not silently grant approvals.

Turn submission sets the owned thread, unique request ID and `clientUserMessageId`. Notifications arriving before the start response are bounded and buffered, then filtered against the accepted turn ID. Commentary and partial-answer messages are not final results. A matching `turn/completed` triggers `thread/read`; only the same terminal turn in the same idle thread completes the task. Final-answer text is preferred, with phase-unknown compatibility for legacy models. Foreign thread/turn events cannot release the slot. A persistent Pi Lead + native Codex protocol fixture passed automatic callback execution.

An actual installed app-server completed initialize/thread-start in a private `CODEX_HOME`, private working folder and ephemeral thread. No turn/model request was sent. This is real transport/schema evidence, not proof of real model execution.

Later model-backed Pi/Codex checks are recorded separately in [the real-model checkpoint](native-real-model-checkpoint-20261011.md).

### Codex cancellation and deadlines

Explicit API-2 cancellation and configured `timeout_s` deadlines append a Job cancellation intent and event through the native journal before provider IO. The intent fixes the reply policy and binds the running execution; duplicate requests do not send another interrupt. If the start response is delayed, the intent waits for its accepted turn ID. The owned driver then sends `turn/interrupt` with that exact thread/turn pair. The empty response is acknowledgement only, as defined in the [official App Server protocol](https://learn.chatgpt.com/docs/app-server) and the installed TurnInterruptParams/Response schemas.

Neither acknowledgement, rejection, elapsed time nor a foreign terminal event frees the slot. Completion still requires the matching terminal turn and idle-thread verification. A natural completion racing cancellation retains the provider's actual status; silent cancellation suppresses its reply. A failure after a cancellation request cannot automatically retry the task. Control acknowledgements/errors are visible in safe session metadata and the final decision.

Daemon death preserves the explicit intent; an old running execution remains fenced. This is durable intent and replay prevention, not completed running-task reconciliation. These controls were tested with owned protocol fixtures; successful real-model interruption remains a separate acceptance check.

## Pi restore and cancellation

When a saved Pi binding has a session file, native startup passes that exact file via `--session` and checks the returned session ID. A missing file or different ID fails closed. A `--no-session` binding has no durable history to resume.

The public native cancel operation queues Pi `abort`; its response reports cancellation requested, not task completion. The driver waits for the anchored run's `agent_settled` and a correlated idle state. Silent-cancel intent is carried into the terminal decision and prevents reply synthesis. Pi deadlines request abort and similarly wait for provider evidence; elapsed time alone never releases the slot.

## Unfinished scope

Running-task reconciliation across daemon crashes, Codex approval/user-input handling, Claude hooks/cancellation/deadlines, Herdr attachment/startup/Agent management, full config/CLI parity, production installer activation, successful real Claude inference, performance comparisons and default Rust selection remain incomplete. Claude stream execution and real Pi/Codex automatic callback evidence are recorded in [the real-model checkpoint](native-real-model-checkpoint-20261011.md). These implementations do not connect to or adopt an existing user app-server/session merely because its process or pathname exists.
