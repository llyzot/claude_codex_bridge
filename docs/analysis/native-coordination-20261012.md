# Native runtime coordination guidance

Native task preparation now appends a versioned coordination block before the
shared/role/private memory layers. The original task body remains first and is
not rewritten in the job journal. Context-control turns bypass this block.
The common preparation path covers Pi RPC, Codex app-server, Claude stream and
Herdr task prompts.

The rules define submission and result handling: submit once, end the current
turn after an accepted delegation, use `--chain` for a required child result,
and let the controller deliver the later result. Review continuations answer
directly rather than opening a new task back to the original caller. Accepted
receipts, provider completion and business acceptance remain distinct.

Correction guidance matches native followup's actual states. Only `injected`
confirms input acceptance; `accepted`/`sending` are pending, and `uncertain` must
not trigger automatic replay, cancellation or restart. The block does not
advertise nonexistent cancellation files or synthetic success markers.

Each task also receives a small routing descriptor: current agent/job/project,
request kind, original caller, enabled configured peers and allowed chain
targets. Its command prefix selects the current native CLI and exact controller
state root. Examples include an explicit `--from` and separately quoted
PowerShell/POSIX arguments. Tokens, token-file paths and environment values are
not included. Missing helper executables are reported as delegation problems;
the rules do not recommend guessing another global CCB installation.

This is model guidance, not a new authorization boundary. Existing callback
policy, active-job validation and provider permissions remain authoritative.
It neither disables other plugins nor proves universal coexistence with every
external orchestrator. No provider home, global instructions or project memory
files are generated or overwritten.

## Verification

The initial coordination/daemon/project-memory/role-delivery selection passed
28 cases. The final coordination/context/Herdr/followup/role-memory selection
passed 73 cases in 235.28 seconds. It includes executing the rendered PowerShell
example through a controller path containing an apostrophe and verifying that
`$()`, semicolons and ampersands remain literal message data. Rust unit tests
passed 28 cases plus the parent-invoked process-death helper. Formatting, Clippy,
diff checks and release/bin hashes passed.

A real Pi Lead and hosted Codex Worker then received an ordinary user task:
delegate a multiplication, review the answer and return a verification marker.
The user task contained no executable path, `--chain`, waiting, polling or
end-turn instruction. Pi made exactly one **PowerShell** tool call using the
runtime-provided native path, state root and sender, with `ask --chain`.

Pi ended its initial turn **1.448980 seconds before** the worker completed.
The same Pi process/session automatically received one continuation and
returned `REVIEW_OK_4ee38821` after the correct worker result `5767`. Native
history shows no Lead wait/poll tool call, one worker job and one continuation.
The original owned test process lifetimes were checked after shutdown.

This is one successful real-model acceptance run. Herdr/original terminal UI
was not opened in that run; its prompt path has controlled-provider regression
coverage. Exact task text, command, lineage, timestamps and hashes are in
[the evidence](native-coordination-20261012.json). Production/default promotion,
remaining memory bootstrap/projection compatibility and broader model acceptance
are still separate work.
