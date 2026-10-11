# Native daemon contract checkpoint

This checkpoint is an experimental executable path inside `ccb-kernel`; the installed CCB runtime and existing Agents were not changed. It advances the Rust-core migration, but does not complete it.

## Entry point

`ccb-kernel --state-root <isolated-directory> --token-file <protected-file-under-root/kernel> --daemon-config <json-file> [--standalone]`

The supplied token follows the existing `ccbd-control-plane-token-v1` contract. `ccb-native token-create <new-file>` creates it using native OS randomness and protected same-user ACLs on Windows (0600 on Unix). `ccb-native --state-root <dir> daemon --config <json>` provisions its own token and starts the standalone service. Native CLI request commands authenticate the existing endpoint; they do not start Python. Complete normal CCB CLI/config/installer parity remains work in progress.

The config has `schema_version: 2`, a `project_id`, and an `agents` object. Each entry declares `provider`, an optional `profile_family`, optional `workspace_path`, and either a `command` argv array or no command for externally claimed work. The current format is an internal checkpoint format; conversion from the full existing CCB config is not complete.

## Verified behavior

- Public API-2 requests keep the existing `{api_version, op, request}` and flattened success/error envelope. The current operations are ping, submit, watch, queue, retry, inbox, mailbox-head, work, complete, cancel, provider-requests, provider-respond and shutdown. Complete operation/output-shape parity is not yet claimed. Cancellation controls are implemented for owned Pi and Codex sessions; explicit Codex interaction is available through native pending/respond commands.
- A serialized actor owns durable mutation and a separate periodic tick. A request receipt does not wait for command execution. Child stdin writing runs separately from the actor; stdout records are bounded.
- A transient command receives a Job JSON on stdin. A result envelope must contain matching job ID, agent, provider, execution-instance ID, and a version-2 CompletionDecision. Process exit and stdout EOF are required before terminal admission. Foreign output is diagnostic failure, not task evidence.
- A Worker result can atomically record its reply and prepare a continuation. The daemon admits that continuation and starts the configured Lead executor. Lead does not poll during its initial submission turn.
- Ordinary replies become receiver Jobs at the existing mailbox head. Their send/start does not consume the reply; successful receiver completion does.
- Retry admission keeps the logical Message and records the new Job/Attempt/FIFO admission together. Result policy preserves configured runtime-retry opt-out, nonretryable account failures, and empty-result notices.
- Standalone mode continues after creator stdin closes. Mounted lease/lifecycle ownership and heartbeat are native. A live/uncertain previous owner blocks startup before journal recovery. Graceful shutdown only stops processes owned by this runtime and releases its own lease.

## Native Pi RPC checkpoint

An agent can select `driver_kind: "pi_rpc"`, `provider: "pi"`, and a command argv which starts Pi RPC mode. The process is persistent across tasks. A task completes only after its prompt ID is accepted, the user message contains its matching `CCB_REQ_ID` anchor, `agent_settled` arrives and an ID-correlated `get_state` confirms the same session is idle. `agent_end`, command acceptance and foreign anchors do not complete a task. Session identity/activity metadata is persisted separately from model/auth configuration.

The installed local primary source is `@earendil-works/pi-coding-agent 1.1.0`; its `docs/rpc.md`, `docs/cli-integration.md`, `docs/rpc-commands.md` and `dist/modes/rpc/rpc-mode.js` establish this contract. Initial transport checks exercised only read-only `get_state`. Later fixtures verified exact-session clean restart and provider-confirmed cancellation/deadlines; real Pi/Codex automatic collaboration is recorded in [the real-model checkpoint](native-real-model-checkpoint-20261011.md).

On Windows the daemon owns a nested Job Object with kill-on-job-close and is assigned before spawning executors. A daemon-death test verified cleanup of an owned descendant. Unix process-tree ownership is still an explicit remaining requirement.

## Remaining requirements

Native Pi/Codex/Claude execution is integrated, as detailed in [the provider checkpoint](native-provider-protocols-20261011.md). Codex interactive requests have native API/CLI handling; real-model interaction and graphical UI integration remain unverified. Other remaining work includes Claude control/hooks and successful real inference, Herdr startup/Agent management, complete native CLI/config/install integration, full public operation parity and Keeper behavior.

An old running Job with no owned process handle is kept active/fenced after restart. This avoids interpreting an observation gap as completion or replaying uncertain work. Durable provider recovery, operator reconciliation and Unix process-tree ownership are not finished. Windows descendant cleanup is verified for this owned-process driver; it is not a claim of controlling unrelated Herdr Agents.

Performance against the Python baseline, state rollback across all supported providers, and a default Rust runtime are still unproven. Real persistent Pi Lead wakeup has been verified separately; it does not establish successful Claude or Herdr-integrated execution. See [migration progress](rust-core-refactor-progress.json) for recorded checks and remaining scope.
