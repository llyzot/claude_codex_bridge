# Native Claude structured questions

The Rust Claude stream driver now accepts explicit responses to supported
`AskUserQuestion` requests through the existing `pending` / `respond` commands
and administrative API. Previously every such request remained
`native_ui_required`, including ordinary structured questions.

The existing current-job/tool anchor and runtime/execution/request identity
checks still apply. An ordinary permission allow without answers cannot answer a
question. The response must preserve the original questions and metadata; only
`answers` or a general `response` may change. Per-question answers cover every
question exactly once. Single selections and free text use strings; multiple
selections use distinct offered labels or the documented string form. General
replies cannot conflict with structured answers. Denials retain the existing
one-time deny contract. No persistent permission rules are accepted.

Malformed or duplicate-question forms remain held for another interaction
surface. Other tools' `requires_user_interaction` flag remains enforced.
Answering does not complete a CCB job: the driver continues waiting for Claude's
correlated terminal result. Duplicate identical submissions send only one reply,
and cancelled/stale handles cannot be answered. Controller audit records omit
the answer content; the provider's own conversation may retain it normally.

## Interface example

Read the exact pending request, copy its `params.input`, then add an answer map
and place it in an answer file:

```json
{
  "behavior": "allow",
  "updatedInput": {
    "questions": [
      {
        "question": "Which format?",
        "header": "Format",
        "multiSelect": false,
        "options": [
          { "label": "Summary", "description": "Short" },
          { "label": "Detailed", "description": "Long" }
        ]
      }
    ],
    "answers": { "Which format?": "Detailed" }
  }
}
```

Use `ccb-native --project <project> respond <agent> <request-id> <answer-file>`.
The real question and all original metadata must be preserved; the example is
not a template for replacing an agent's question. Graphical question cards and
stronger caller/human-consent provenance remain separate work.

The wire contract follows the [official user-input documentation](https://code.claude.com/docs/en/agent-sdk/user-input).
Selections are returned through `updatedInput.answers`; a general reply uses
`updatedInput.response`. The implementation was checked against the installed
Claude Code 2.1.296, rather than inferred only from that documentation.

## Validation

The structured-answer test failed before implementation. The final selected
request/stream/control suite passed 76 cases in 135.58 seconds, including four
actual-CLI round trips against a loopback-only Messages API fixture: single
selection, multiple selection, general reply and denial. Additional actual-CLI
cancellation and canonical-Windows-cwd cases passed separately (2.52 and 2.49
seconds). Cancellation invalidated the old request handle. Rust unit tests passed
(25 plus the parent-invoked subprocess helper), as did formatting, Clippy with
warnings denied and diff checks.

The first actual-CLI test used `--bare`; that installed version omitted
`AskUserQuestion` from its tool inventory and returned a tool-not-available error.
Changing only the test mode to `--safe-mode` made the tool available. The test
keeps a new empty Claude configuration directory, no user/project setting sources,
no MCP servers, only the question tool, and normal permission handling. A dummy
key is accepted only by the loopback fixture; it never forwards requests. No
credentials are copied or permission-bypass flags used.

This proves CLI/control-protocol behavior, not authenticated Claude inference or
Claude Lead reasoning. The default local account reported `loggedIn: false` and
`authMethod: none` in `C:\Users\user\.claude`. A question about the user's
alternate configuration/launcher is pending. Original Claude TUI integration,
MCP elicitation, other native dialogs, authenticated Lead callbacks, broader
interface parity and default/production promotion remain unfinished.
