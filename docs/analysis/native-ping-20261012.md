# Native agent health observations

The native CLI now accepts `ping`, `ping ccbd`, `ping all` and
`ping <agent>`. The empty/ccbd form preserves the existing service response and
its startup fingerprint; it describes the controller, not every provider.
Legacy valid bootstrap nonces are echoed for readiness probes.

Agent responses identify the project, provider, runtime state and local health,
with current task/queue facts and bounded diagnostics. For an owned Windows
protocol provider, healthy local state requires a live matching process creation
identity, observed protocol initialization, an open transport and consistent
provider/scheduler task identities. Pending user requests, provider faults,
reload drains and disabled dispatch have separate states. A disabled agent
remains observable rather than passing through the submission guard.

Known Codex thread activity without a corresponding CCB task is reported as
untracked activity, not idle. A malformed or foreign job lineage makes that
agent unknown while allowing `ping all` to return other agents. Diagnostics do
not include approval contents, provider error text, environment values or auth
secrets. A successful command exit means the observation succeeded, not that
every returned agent is healthy.

The untracked-activity test exposed an existing driver gap: thread-status
notifications were ignored when no CCB turn was active. The driver now records
status for its bound thread before that early return, and observed non-idle
status prevents a new CCB turn from being dispatched. An idle notification
permits queued work to resume but never completes an active CCB job. Foreign
thread notifications are ignored. This respects observed activity; it does not
lock out all concurrent manual TUI input or resolve every cross-client race.

Health is deliberately scoped to **local runtime observations**. It does not
test account authentication, model/network availability, quotas or inference.
Protocol readiness is last-observed state; a live process and apparently open
pipe cannot prove that the provider will respond to future requests. Unsupported
process probes and external/on-demand workers stay unknown rather than being
declared healthy from configuration alone.

External Herdr providers are not freshly queried by this endpoint. Actual
preflight/readiness events now stamp their in-memory last-observed time. Ping
reports that historical observation, not current terminal health. It never
starts a Herdr server, sends a prompt, answers a question, acknowledges a reply,
repairs a fault, kills a process or replays a task.

## EOF fixture diagnosis

The first output-closure fixture closed the interpreter's fd 1 but the native
reader did not receive EOF. Following the diagnosis loop, changing only the
worker executable from the Windows venv redirector to the base interpreter made
EOF observable. A paired control observed one additional process for the venv
launch stack (three versus two active scope processes), with `transport_open`
remaining true only in that stack. Both fixtures had executed their close
marker. This supports inherited writer handles masking EOF; the native reader
was not changed to force that test green.

The committed regression fixture therefore uses the base interpreter to test
actual pipe EOF while the process remains alive. Ping then reports a nonhealthy
transport without restarting it. This does not establish that every silent
provider/launcher failure is detectable without an active probe, nor does it
finish autonomous idle-EOF recovery handling.

Tests cover default and all-agent responses, disabled agents, owned Pi/Codex/
Claude readiness, active task identity, pending input, live faulted processes,
observable EOF, external Herdr readiness, unknown targets and unchanged durable
records/provider input. Broader fault, interactive-request, Herdr and CLI tests
are also selected. Exact counts, control results and hashes are in the adjacent
JSON evidence. No real model request or production installation change was
performed for this checkpoint.
