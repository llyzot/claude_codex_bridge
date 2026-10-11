# Native explicit message resubmission

`ccb resubmit <message-id>` is now implemented by the Rust daemon and CLI.
It creates a new logical message with `origin_message_id` pointing to the selected
message, new Job/attempt IDs, and retry index zero. `retry` retains the existing
logical message and requires its latest non-successful attempt; these operations
are not interchangeable.

Resubmission freezes the original recipient list, even if new agents have since
joined the project. Every original target must still be available. All latest
attempts must be terminal, callback dependencies resolved, and saved request
artifacts valid. Admission uses the same validation, current agent configuration,
FIFO scheduling and atomic store transaction as ordinary submission.

Ordinary requests reuse the latest saved request, matching the legacy planning
policy. For a completed callback parent whose latest attempt is an internally
generated review continuation, the native implementation selects the original
parent ask instead of replaying the review prompt. A chain child cannot be rebound
to an unrelated current parent; restart from the completed root request. Delivery
messages and context-management operations are not replayed as normal tasks.

## Lost acknowledgements

The native CLI allocates an operation UUID before sending the request. To repeat
the same admission after a transport failure, use the ID shown in the diagnostic:

```powershell
ccb resubmit msg_0123456789ab --operation-id <same UUID>
```

The receipt is committed with the new message/jobs. Repeating the same operation
returns the same receipt, including after daemon restart or a lost network reply.
Changing its source message is rejected. A new operation UUID is a new explicit
resubmission and can intentionally enqueue another task. This deduplicates
admission, not arbitrary external side effects of provider execution.

## Verification and scope

The public CLI test failed before implementation. Coverage includes completed
tasks, source lineage and fresh attempt numbering, receipt replay across restart,
lost acknowledgement, active-attempt refusal, completed callback-root replay,
fixed broadcast recipients after adding an agent, atomic rejection after removing
a target, artifact tampering, unresolved callbacks, child-chain retargeting and
context-control replay refusal. Final suite counts and hashes are recorded in
the progress ledger/evidence.

Tests use isolated native daemons and controlled provider fixtures. No real model
inference, production installation/default switch or existing user agent control
occurred. Other command/API parity, Claude authenticated acceptance and default
runtime migration remain open.

Final verification: 92 selected resubmit/daemon/trace/wait/context/reload cases
passed in 181.70 seconds. Rust tests passed (25 plus the parent-invoked helper),
formatting, Clippy with warnings denied, diff checks and release/bin hash equality
passed. This was not a full repository regression run.
