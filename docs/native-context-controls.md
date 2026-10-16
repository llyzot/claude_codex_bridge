# Native context controls and CCB skills

## `ccb clear` and `ccb compact`

```powershell
ccb compact                 # every configured agent
ccb compact pi_lead         # named agents only
ccb clear claude_reviewer --wait
```

- `compact` asks the provider to summarize its conversation. The session stays
  the same.
- `clear` starts a fresh provider conversation in the same running agent. The
  old conversation stays on disk. Project files, `.ccb` state, authentication
  and memory are untouched.
- Busy or queued agents are refused before any input is sent. The command runs
  once and reports its operation. With `--wait`, it also reports the result.

| Agent | compact | clear |
|---|---|---|
| Hosted Codex | protocol compaction | protocol clear |
| Claude (Herdr terminal) | `/compact` | `/clear` |
| Pi (Herdr terminal) | `/compact` | `/new` |

For terminal agents, CCB types the provider's own command only after the
operation is committed, and it never resends the command. A clear completes
once CCB observes the new session (the new Claude transcript, or Pi's new
`current.json` session). CCB then records a session rotation, so later tasks,
`ccb herdr open` and stopped-server recovery use the new session, not the old
one.

If the daemon restarts in the middle of a clear, it does not resend `/clear`.
If the new session is already observable, the clear is completed and rotated.
Otherwise the operation is reported as `incomplete`.

A Pi session started by `/new` that never received a task is recovered as a
fresh session; it is not held for missing history.

## CCB skills

The control skills (`ask`, `ccb-clear`, `ccb-compact`) teach each agent the
exact commands and their limits.

```powershell
ccb skills status
ccb skills install --dry-run
ccb skills install --provider pi
ccb skills install --replace-unmanaged     # replace older non-managed copies
```

Skills install into each provider's user skill root:

| Provider | Skill root | Override |
|---|---|---|
| Pi | `~/.pi/agent/skills` | `PI_CODING_AGENT_DIR` |
| Claude | `~/.claude/skills` | `CLAUDE_CONFIG_DIR` |
| Codex | `~/.codex/skills` | `CODEX_HOME` |

Each installed skill records a `.ccb-skill.json` manifest of file hashes.
`install` updates only skills that it installed and that you have not edited.

- Edited skills are reported as `skipped_modified` and left alone.
- Skills without a manifest, for example ones copied by the legacy installer,
  are reported as `skipped_unmanaged` and left alone.
- `--replace-unmanaged` first moves an unmanaged skill to `.ccb-skill-backups`
  next to the skill root, then installs CCB's version. Backups are kept outside
  the root because Pi scans skill roots recursively.
