# Native Claude tool permission requests

The native Claude transport now uses stdio permission prompts and partial
assistant events. Existing `pending` / `respond` commands and public
`provider-requests` / `provider-respond` operations support both Codex and Claude.
The daemon has no automatic approval policy.

An actionable Claude request must belong to the current owned runtime, Job and
execution. Its `tool_use_id` and tool name must match an observed tool call in
an assistant message or partial stream bound to the submitted user-message UUID.
Nested tool calls are admitted only through an already observed parent tool.
A foreign top-level user stamp clears both stream/message binding caches.
Requests arriving before their tool anchor remain visible as
`awaiting_tool_anchor` and cannot be answered. Older producers without the
required ownership evidence are not silently trusted.

The current response interface accepts one-time `allow` and `deny` decisions.
An allow preserves the exact requested input; callers may omit `updatedInput`
or provide the identical object. A denial supplies a bounded message and may
request interruption. An interrupting denial commits the native cancellation
intent before the response reaches Claude, fencing subsequent approvals. The
native response carries the matching tool-use ID.
Input rewrites, persistent permission updates, decision-classification overrides
and unknown answer fields are rejected. Identical repeated answers are
idempotent; changing an already queued answer is rejected.

Consent metadata, including `default_to_no`, `suppress_always_allow_rule`,
`classifier_approvable` and `decision_reason_type`, remains visible to the
authorized caller. Requests marked `requires_user_interaction`, and
`AskUserQuestion` requests, cannot be answered through a generic allow/deny file.
They are reported as `native_ui_required`. No custom dialog kinds are advertised;
undeclared `request_user_dialog` frames are not answered, including with a
synthetic cancellation. Other unsupported controls return an explicit protocol
error rather than receiving an inferred decision.

The explicit response action is durably audited before provider IO. Only native
handles and execution identities are persisted: tool input, decision-reason text
and answer content stay in memory. Provider/session history may have its own
logging. CLI permission previews escape terminal controls, bidi controls and
selected invisible formatting characters while preserving JSON round-trip values.
Programmatic API consumers receive the original data and must render it safely.

Permission acknowledgements never complete the task. Completion still requires
the existing Claude result/session/replay checks. Provider withdrawal removes a
handle. Cancellation, deadlines, termination, quarantine and terminal completion
retire pending requests. Durable cancellation also fences responses before the
provider finishes interrupting. Conflicting reuse of a wire request identity
quarantines the provider; unrelated Agents continue. A permission request with
no active input-owning execution is quarantined without a reply that could
advance an inherited or old workflow. Capacity limits bound pending
requests, parameter bytes and observed tool identities.

The implementation follows the matching publisher Agent SDK 0.3.296 permission,
withdrawal and user-message attribution declarations. Installed Claude Code
2.1.296 accepts the required launch flags. Protocol fixtures verify full and
partial attribution, nested calls, early requests, allow/deny, idempotence,
invalid/widening answers, identity fences, cancellation, withdrawal, fault
isolation, unsupported dialogs and CLI routing/display.

This checkpoint did not claim complete Claude interaction support. Subsequent
structured-question support is recorded in `native-claude-questions-20261012.md`.
Host hook callbacks, general dialog rendering, persistent permission
configuration and unowned/background or inherited request recovery still need
work. The local control-plane token remains an administrative capability; this
interface does not attest that an API caller is human, so callers must obtain
the required user authorization. At this checkpoint, successful authenticated
Claude inference had not been verified; later Keeper/Lead tests and the MCP
acceptance below supersede that limitation.
The earlier Codex policy rejection remains unresolved. Production installation
and the default runtime were not changed.

Validation: 115 affected interaction/stream/control/fault cases passed before
the final fencing refinements. The final rebuilt release passed all 40 new
Claude permission cases and the Windows Unicode/argv case (41 total). Rust
tests (11), formatting and Clippy with warnings denied passed; both repository
binary hashes match release outputs. The full 455-case inventory was not run.

## MCP elicitation (2026-10-12)

`pending <agent>` now exposes Claude `elicitation` requests through the same
live request handle. `respond <agent> <request-id> <answer-json-file>` accepts
`{"action":"accept","content":{"choice":"alpha"}}` for a matching form,
or `{"action":"decline"}` / `{"action":"cancel"}`. URL-mode answers carry
only an action. CCB does not open URLs or treat acceptance as proof that an
external authentication flow has finished.

The request includes the MCP server name, message and form schema or URL.
Form answers are checked against the bounded flat schema and cannot introduce
unrequested fields. Nested object forms and schema references are rejected.
Answer bodies and URL parameters are not written to CCB's session metadata or
action audit. The provider and the caller's answer file have their own retention.

Claude's elicitation envelope has no tool-use ID. These entries explicitly say
`session_request_during_active_execution`, rather than claiming stamped tool-call
attribution. The current execution must first be acknowledged by its replayed
user request; runtime, execution, job and request handles still fence responses.
Unowned requests require recovery. Withdrawal, cancellation and completion
invalidate pending handles. An answer is not a completion signal.

The authenticated installed Claude CLI called a local test MCP tool once,
received one explicit form answer and returned its value in the same task.
The test Keeper stopped. URL responses have controlled-protocol coverage;
browser navigation, GUI rendering and unowned/background request recovery remain
unverified or unimplemented. See `native-claude-elicitation-20261012.json` for
tests, sources, retained initial failures and real-run evidence.
