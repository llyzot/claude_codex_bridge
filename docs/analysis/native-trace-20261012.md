# Native trace command parity

The Rust daemon and CLI now implement `trace` for `sub_`, `msg_`, `att_`, `rep_`
and `job_` IDs. This closes a diagnostic gap: existing native failure guidance
already directed users to trace, but the daemon rejected that operation.

```powershell
ccb trace job_0123456789ab
ccb trace msg_0123456789ab --limit 20 --offset 20
```

The response retains the existing selected-record fields and message, attempt,
reply, inbox-event and job summaries. Submission jobs retain their recorded order;
attempts retain retry order. The test suite compares message/attempt/reply summary
fields against the existing Python implementations on the same native records.

Additional native diagnostics include:

- `job.business_status` distinguishes a finished physical Lead turn from
  `waiting_callback`; `visible_reply_id` identifies the eventual logical result.
- `callbacks` and `callback_jobs` follow the connected callback graph, including
  parent, child and continuation jobs, without collecting unrelated jobs.
- Internal reply-delivery jobs expose their `source_reply` and `delivery_event`,
  preserving the distinction between Worker completion and return delivery.
- Missing referenced jobs are listed explicitly. A job or submission from a
  different project is rejected rather than joined into the trace.

Tracing uses the mounted Rust authority. It does not launch a daemon/provider,
acknowledge mailboxes, finish jobs, retry requests or read artifact contents.
Artifact path/size/hash fields describe the recorded reference; trace is not an
artifact-integrity verifier. Request bodies, provider options, environment values
and raw approval payloads are not added to the projection. Reply text remains an
intentional diagnostic output. The CLI escapes terminal control/bidi characters
while retaining valid JSON.

## Bounds and compatibility

`--limit` defaults to 100 and accepts 1–1000 rows per collection. `--offset` pages
all collections together. Counts describe the complete selection; `truncated`
and `next_offset` describe the returned page. Inline reply text is bounded to
32,768 Unicode scalar values with `reply_truncated` and the original `reply_size`.
The callback graph is bounded to 4,096 jobs/edges. Responses over the transport
budget are rejected with guidance to reduce the page size.

This implements the structured native API/CLI surface, not the old CLI's text
renderer. Response limits do not establish latency bounds for very large retained
history; production-scale scan performance remains to be measured. Legacy active
follow-up diagnostics and complete command/API parity remain separate work.

## Verification

The initial public-API test reproduced `unsupported native daemon operation:
trace`. Coverage now includes every direct ID kind, broadcast submissions,
callback completion and waiting, ordinary reply delivery, retry attempts,
explicit paging, invalid/missing targets, cross-project refusal, unchanged
durable job records, Python summary parity and artifact-body independence.
The progress ledger records the final selected-suite results and release hashes.

The installed native README now documents trace and the existing experimental
Herdr workflows. No production installation, model account, existing user agent
or default-runtime selection was changed. The overall Rust migration remains
incomplete.

Final verification: 66 selected trace/daemon/project-view/CLI-security/installation
cases passed in 93.41 seconds. Rust tests passed (25 plus the parent-invoked helper);
formatting, Clippy with warnings denied, diff checks and release/bin hash equality
passed. This was not a full repository regression run.
