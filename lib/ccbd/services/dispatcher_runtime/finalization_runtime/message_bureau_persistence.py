from __future__ import annotations

from dataclasses import replace

from completion.models import CompletionDecision

from ..completion import build_terminal_state
from ..records import append_job


def persist_reply_decision(
    dispatcher,
    current,
    terminal,
    reply_decision: CompletionDecision,
    *,
    prior_snapshot,
    finished_at: str,
):
    if getattr(dispatcher._state, '_native', None) is not None:
        from ..native_finalization import persist_native_terminal
        return persist_native_terminal(dispatcher, current, reply_decision, finished_at=finished_at,
                                       revise=True, prior_snapshot=prior_snapshot)[0]
    terminal = replace(terminal, terminal_decision=reply_decision.to_record())
    append_job(dispatcher, terminal)
    dispatcher._snapshot_writer.write_completion(
        job_id=current.job_id,
        agent_name=current.agent_name,
        profile_family=dispatcher._profile_family_for_job(current),
        state=build_terminal_state(reply_decision, prior_snapshot.state if prior_snapshot else None),
        decision=reply_decision,
        updated_at=finished_at,
    )
    return terminal


__all__ = ['persist_reply_decision']
