# Agent journal parent authority

CCB keeps controller records under a directory such as `runtime/ccbd` and Agent
Job/Event journals beside it under `runtime/agents`. A lock confined to `ccbd`
alone did not protect those sibling journals when a caller chose another
controller directory in the same runtime parent.

Rust now claims the shared Agent-journal parent before any Job/Event read,
append, save, removal or replay. The claim is lazy, so a kernel used only for
root-local mailbox/namespace data does not unnecessarily own sibling journals.
Two private records live directly inside `agents`:

- `.ccb-journal-owner.lock`: an OS lock held for the claiming store's lifetime.
- `.ccb-journal-owner.json`: schema 1, record type `ccb_agent_journal_owner`, and
  the owning `controller_dir` name.

The existing controller-local writer lock remains in place. The parent binding
persists after process exit, so another controller cannot assume ownership merely
because the previous process stopped. Reopening the same controller works, and
moving its complete runtime parent preserves the relative binding name. Renaming
or reassigning the controller itself needs an explicit migration; this code does
not perform that migration or infer it from timestamps/PIDs.

On first binding, a bounded shallow inspection checks for conflicting controller
state and live legacy writer locks. Existing journals without known controller
state, or ambiguous sibling controller/journal layouts, are rejected without
guessing an owner. A leftover writer-lock file alone is not proof of a legacy
controller, so retrying a failed claim cannot turn it into a valid claim.
Ownership is created without replacing another marker; corrupt/foreign markers
and linked authority paths are not silently repaired.

Redo recovery validates every target and claims its journal ownership before
applying the first write. A transaction containing an earlier root-local write
and a later foreign Agent write therefore does not partially modify the earlier
file before rejecting the foreign target.

The transitional Python `legacy_writer` uses the same parent marker and OS-lock
protocol, in addition to its existing controller lock. Its reentrant same-root
behavior is retained, mismatched Agent/controller path layouts are rejected,
and errors raised by the guarded operation retain their original type. Python
and Rust can use the same correctly bound runtime after the other writer exits.

Independent runtime roots must have independent parents, for example
`one/ccbd` and `two/ccbd`, rather than `runtime/a` and `runtime/b` sharing one
`agents` directory. The generic multi-service test fixture now follows this
layout. Tests cover live/cold/abrupt-exit claims, Python/Rust interoperability,
separate parents, unknown legacy data, immutable markers, recovery preflight,
relative-parent movement and preservation of application exceptions.

This is an advisory ownership protocol for cooperating current writers, not a
filesystem security boundary against arbitrary direct edits or unmodified old
programs that ignore it. A legacy controller already holding a known writer
lock is detected during initial migration; mixing older alternate-root writers
after binding remains unsupported. Operator-directed repair/reassignment of an
ambiguous legacy parent and complete cross-platform/runtime relocation still
need their own verification. No user journals were relocated or deleted.
