from __future__ import annotations

from dataclasses import replace

from completion.models import CompletionDecision
from storage.text_artifacts import artifact_stub, maybe_spill_text, write_text_artifact


def spill_terminal_reply_if_needed(
    dispatcher,
    current,
    decision: CompletionDecision,
    *,
    finished_at: str,
) -> CompletionDecision:
    native = getattr(dispatcher._state, '_native', None)
    if native is not None:
        from completion.snapshot_store import _decision_from_record
        return _decision_from_record(native.call('artifact.spill_completion', job=current.to_record(),
                                                  decision=decision.to_record(), timestamp=finished_at))
    diagnostics = dict(decision.diagnostics or {})
    if isinstance(diagnostics.get('reply_artifact'), dict):
        return decision
    if _force_reply_artifact(current):
        artifact = write_text_artifact(
            dispatcher._layout,
            text=decision.reply,
            kind='completion-reply',
            owner_id=current.job_id,
            now=finished_at,
        )
        if _is_kimi_no_captured_reply(current, decision, diagnostics):
            reply = artifact_stub(
                prefix=(
                    f'CCB completion reply for job {current.job_id} has no captured Kimi provider reply; '
                    '--artifact-reply stored an empty artifact for transport metadata only.'
                ),
                artifact=artifact,
                include_preview=False,
                instruction='Instruction: no provider reply was captured; do not treat this artifact as task evidence.',
            )
            diagnostics['artifact_instruction'] = 'no_provider_reply_captured'
            diagnostics['artifact_empty_no_provider_reply'] = True
        elif not str(decision.reply or '').strip():
            provider = str(getattr(current, 'provider', '') or 'provider').strip()
            reply = artifact_stub(
                prefix=(
                    f'CCB completion reply for job {current.job_id} has no captured '
                    f'{provider} provider reply; --artifact-reply stored an empty '
                    'artifact for transport metadata only.'
                ),
                artifact=artifact,
                include_preview=False,
                instruction='Instruction: no provider reply was captured; do not treat this artifact as task evidence.',
            )
            diagnostics['artifact_instruction'] = 'no_provider_reply_captured'
            diagnostics['artifact_empty_no_provider_reply'] = True
            diagnostics['no_captured_reply'] = True
        else:
            reply = artifact_stub(
                prefix=f'CCB completion reply for job {current.job_id} was stored as an artifact by --artifact-reply.',
                artifact=artifact,
                include_preview=False,
            )
        diagnostics['artifact_reply_forced'] = True
    else:
        reply, artifact = maybe_spill_text(
            dispatcher._layout,
            text=decision.reply,
            kind='completion-reply',
            owner_id=current.job_id,
            prefix=f'CCB completion reply for job {current.job_id} is larger than 4 KiB and was stored as an artifact.',
            now=finished_at,
        )
    if artifact is None:
        return decision
    diagnostics['reply_artifact'] = artifact
    return replace(decision, reply=reply, diagnostics=diagnostics)


def _force_reply_artifact(current) -> bool:
    options = dict(getattr(current.request, 'route_options', None) or {})
    return bool(options.get('artifact_reply'))


def _is_kimi_no_captured_reply(current, decision: CompletionDecision, diagnostics: dict[str, object]) -> bool:
    provider = str(getattr(current, 'provider', '') or '').strip().lower()
    if provider != 'kimi':
        return False
    if str(decision.reason or '') != 'kimi_native_turn_timeout':
        return False
    if _int_value(diagnostics.get('reply_chars')) != 0:
        return False
    return bool(
        diagnostics.get('no_captured_reply')
        or diagnostics.get('provider_no_reply')
        or diagnostics.get('receipt_class') == 'no_captured_reply'
    )


def _int_value(value: object) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


__all__ = ['spill_terminal_reply_if_needed']
