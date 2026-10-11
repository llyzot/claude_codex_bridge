# Native mailbox-aware pend and queue

The current legacy command registry has no independent `back` command. `back`
is a sidebar communication-stage label; task results are returned by the
controller's reply/callback lifecycle. The real interface gaps addressed here
are mailbox-aware `pend` and detailed `queue` observations.

```powershell
ccb-native --state-root <directory> pend lead
ccb-native --state-root <directory> pend --inbox --detail lead
ccb-native --state-root <directory> pend --queue --detail lead
ccb-native --state-root <directory> pend --watch <job-id> --timeout-ms 2000
ccb-native --state-root <directory> queue --detail lead
```

`pend <job-id>` remains a single-job snapshot. `pend <agent>` shows the existing
latest-job observation and adds separate `mailbox_reply_*` fields for a reply
at the actual FIFO head. Thus an agent's old own-task answer is not substituted
for a newer incoming reply. An agent with no own job can still report
`status: mailbox_reply`, with `job: null`; in that legacy-compatible shape the
top-level job ID identifies the source job. `mailbox_reply_job_id` is explicit
in every incoming-reply view.

A pending task at the head is never skipped to show a later reply. Replies
currently being automatically delivered remain observable as `delivering`.
`mailbox_reply_ready` means a reply is present, not that delivery finished or
that manual acknowledgement is allowed. The separate `ack` authority still
enforces FIFO and execution/delivery ownership.

`queue` preserves native `queues` scheduler slots and adds legacy mailbox
summaries. Ordinary summaries read the cached mailbox projection without
decoding the full inbox history. Missing/error summary status must not be
interpreted as proof of an empty live queue. Named `--detail` expands pending
events, source lineage and reply previews through the existing read-only inbox
view. As in the legacy service, `queue all --detail` remains an aggregate summary;
select an agent to expand its events. The legacy `agent.active` detail field can
be the pending head candidate; use `queues[].active_job_id` for scheduler-slot
ownership rather than treating that candidate as proof of running execution.

`pend --watch` uses the existing bounded observer and pins the resolved job ID.
A timeout never cancels, restarts, acknowledges or resubmits work. Native
`--wait`, cursor and timeout observer options remain available. The optional
positive legacy `pend` count is accepted as parser compatibility: the old
Python service also returned one snapshot instead of selecting N historical
replies. Counts cannot be combined with other observer modes. Invalid or
conflicting flags are rejected, and command output escapes terminal controls
while preserving decoded JSON values.

Mailbox summary schema/agent identity validation is now shared by queue and
inbox-derived views. Invalid projections are reported without overwriting them.
An ordinary queue query still exposes scheduler slots when an inbox body is
corrupt; requesting detail reports the decoding error. Observations do not
discard stale events or repair caches.

Tests compare queue summaries and incoming-reply fields against the Python
implementations, and check stale own answers, recipients without jobs, FIFO
blocking, automatic delivery, malformed projections, timeout behavior, CLI
validation and unchanged durable mailbox/journal files. A prior project-view
test compared the entire metrics map; it now checks the intended invariant—no
additional history reads/rows—while allowing the newly introduced metadata
signature-check counter to increase. No production behavior was changed to
hide that test failure.

Exact verification counts and release hashes are in the adjacent evidence file.
These tests use isolated native controllers and controlled completions, not
real model calls. Other command/API parity and production promotion remain open.
