# Native daemon contract checkpoint

This checkpoint is an experimental executable path inside `ccb-kernel`; the installed CCB runtime and existing Agents were not changed. It advances the Rust-core migration, but does not complete it.

## Entry point

`ccb-kernel --state-root <isolated-directory> --token-file <protected-file-under-root/kernel> --daemon-config <json-file> [--standalone]`

The supplied token follows the existing `ccbd-control-plane-token-v1` contract and must be provisioned securely before startup. Native token provisioning and the normal CCB installer/CLI path remain work in progress.

The config has `schema_version: 2`, a `project_id`, and an `agents` object. Each entry declares `provider`, an optional `profile_family`, optional `workspace_path`, and either a `command` argv array or no command for externally claimed work. The current format is an internal checkpoint format; conversion from the full existing CCB config is not complete.

## Verified behavior

- Public API-2 requests keep the existing `{api_version, op, request}` and flattened success/error envelope. The current operations are ping, submit, watch, queue, retry, inbox, mailbox-head, work, complete and shutdown. Complete operation/output-shape parity is not yet claimed.
- A serialized actor owns durable mutation and a separate periodic tick. A request receipt does not wait for command execution. Child stdin writing runs separately from the actor; stdout records are bounded.
- A transient command receives a Job JSON on stdin. A result envelope must contain matching job ID, agent, provider, execution-instance ID, and a version-2 CompletionDecision. Process exit and stdout EOF are required before terminal admission. Foreign output is diagnostic failure, not task evidence.
- A Worker result can atomically record its reply and prepare a continuation. The daemon admits that continuation and starts the configured Lead executor. Lead does not poll during its initial submission turn.
- Ordinary replies become receiver Jobs at the existing mailbox head. Their send/start does not consume the reply; successful receiver completion does.
- Retry admission keeps the logical Message and records the new Job/Attempt/FIFO admission together. Result policy preserves configured runtime-retry opt-out, nonretryable account failures, and empty-result notices.
- Standalone mode continues after creator stdin closes. Mounted lease/lifecycle ownership and heartbeat are native. A live/uncertain previous owner blocks startup before journal recovery. Graceful shutdown only stops processes owned by this runtime and releases its own lease.

## Remaining requirements

The test driver is not a real provider adapter. Persistent Pi RPC/session events, Codex session evidence, Claude hooks, Herdr startup/Agent management, native CLI/config/install integration, native token creation, full public operation parity and Keeper behavior still require implementation and verification.

An old running Job with no owned process handle is kept active/fenced after restart. This avoids interpreting an observation gap as completion or replaying uncertain work. Durable provider recovery, operator reconciliation and process-tree ownership are not finished. Controlled child processes are stopped on normal runtime teardown; this is not a claim of crash-proof descendant-process cleanup.

Performance against the Python baseline, true persistent-session Lead wakeup, state rollback across all supported providers, and a default Rust runtime are still unproven. See [migration progress](rust-core-refactor-progress.json) for recorded checks and remaining scope.
