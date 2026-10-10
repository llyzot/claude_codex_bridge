# Native real-model checkpoint

The code remains experimental and the default installed runtime is unchanged. Existing Herdr Workspace, `codex_impl`, provider user settings and system PATH were not altered.

## Real Pi/Codex evidence

Two opt-in tests in `test/test_native_real_models.py` exercised real inference through the Rust daemon:

1. A Codex Worker in a new private `CODEX_HOME` and workspace completed a turn and returned `CCB_WORKER_OK`. Model/sandbox settings were scoped to that new thread; no tools/file changes were requested.
2. A real Pi Lead submitted one native CLI `ask --chain` to the Codex Worker and ended its initial response. Rust recorded the child result, admitted the continuation and started the Lead's next turn in the same Pi process/session. The Lead returned `CCB_LEAD_REVIEW_OK`; the callback edge reached `done`.

The tests use the locally configured model families (`gpt-6-astra` for Pi, `gpt-6.1-sol` for Codex). Auth/config files are read from the user source and copied as small protected identity overlays into new private homes. Native copying never overwrites a destination and does not print secrets. This is not a plugin/resource directory snapshot.

The first Pi run was a verified terminal failure to delegate: Pi could not locate Git Bash. Existing `C:/Users/user/scoop/apps/git/current/usr/bin/bash.exe` was then selected in only the temporary Pi `settings.json`, and a fresh test passed. The old failed test was allowed to finish; no live process was restarted merely because observation timed out.

Codex lifecycle behavior was cross-checked against [official App Server documentation](https://learn.chatgpt.com/docs/app-server) and installed CLI 0.162.1 generated schemas. The initialized connection, accepted turn ID and terminal notification are distinct evidence; this implementation additionally verifies the completed turn through `thread/read`.

## Claude stream checkpoint

The native `claude_stream` driver controls an owned persistent CLI process with `--print --input-format stream-json --output-format stream-json --verbose --replay-user-messages`. It supplies a new UUID session or resumes the previously recorded exact session. Completion requires a current-session result after the matching replayed user UUID and `CCB_REQ_ID` anchor. Subagent messages and consumed result UUIDs are rejected.

Protocol fixtures verify same-process turns, Claude Lead + native Codex Worker callback and clean restart binding. Installed Claude Code 2.1.296 was exercised in a private configuration/workspace with safe mode and no tools: it emitted init, echoed the requested user UUID and produced a `result` with `subtype=success` but `is_error=true`, then exited 1. Native handling preserves that as failure, classifies the login error as nonretryable and keeps the daemon available. A success subtype alone is insufficient.

CLI flag/output behavior was checked against its local help and [official programmatic CLI documentation](https://code.claude.com/docs/en/headless). Existing Claude authentication status is logged out. Successful Claude model execution requires user-provided login/credentials and remains unfinished; no authentication settings were changed.

## Remaining scope

Full CLI/config/API-2 parity, Herdr lifecycle/attachment, interrupted-running-task restoration, remaining provider controls, Keeper, performance/rollback evidence, default Rust selection and removal of transitional Python core entrypoints are still incomplete. A successful native model pipeline is not completion of the overall migration.
