"""Profile/config and wire-model adapters around Rust completion state."""
from __future__ import annotations

from tempfile import TemporaryDirectory
from pathlib import Path
from uuid import uuid4
from weakref import finalize

from completion.registry import CompletionRegistry
from completion.snapshot_store import _decision_from_record, _state_from_record
from native_kernel import KernelClient, kernel_for_layout


def _release(client, group, temporary):
    try:
        client.call('completion.drop_group',group_id=group)
    except Exception:
        pass  # Cleanup never restarts a closed or unavailable kernel.
    if temporary is not None:
        client.close()
        temporary.cleanup()


class NativeCompletionTracker:
    def __init__(self, config, provider_catalog, registry=None, *, request_timeout_s=3600.0, layout=None):
        if registry is not None and type(registry) is not CompletionRegistry:
            raise ValueError('Custom Python completion detectors need an explicit native adapter')
        self._config=config
        self._provider_catalog=provider_catalog
        self._registry=registry or CompletionRegistry()
        self._request_timeout_s=request_timeout_s
        temporary=TemporaryDirectory(prefix='ccb-completion-') if layout is None else None
        self._native=KernelClient(Path(temporary.name)) if temporary else kernel_for_layout(layout)
        self._group=uuid4().hex
        try:
            self._native.call('completion.create',group_id=self._group)
        except Exception:
            if temporary:
                self._native.close()
                temporary.cleanup()
            raise
        self._cleanup=finalize(self,_release,self._native,self._group,temporary)

    def _call(self, method, **args):
        try:
            return self._native.call('completion.'+method,group_id=self._group,**args)
        except ValueError as error:
            if str(error).startswith('unknown completion tracker:'):
                raise KeyError(str(error)) from error
            raise

    @staticmethod
    def _view(record):
        from completion.tracker import CompletionTrackerView
        return None if record is None else CompletionTrackerView(
            job_id=record['job_id'],agent_name=record['agent_name'],
            state=_state_from_record(record['state']),decision=_decision_from_record(record['decision']))

    def start(self, job, *, started_at):
        spec=self._config.agents[job.agent_name]
        manifest=self._provider_catalog.resolve_completion_manifest(spec.provider,spec.runtime_mode)
        profile=self._registry.build_profile(spec,None,manifest)
        return self._view(self._call('start',job_id=job.job_id,agent_name=job.agent_name,provider=job.provider,
                                     profile=profile.to_record(),started_at=started_at,timeout_s=self._request_timeout_s))

    def current(self, job_id):
        return self._view(self._call('current',job_id=job_id))

    def ingest(self, job_id, item):
        return self._view(self._call('ingest',job_id=job_id,item=item.to_record()))

    def tick(self, job_id, *, now):
        return self._view(self._call('tick',job_id=job_id,now=now))

    def tick_all(self, *, now):
        return tuple(self._view(record) for record in self._call('tick_all',now=now))

    def finish(self, job_id):
        self._call('finish',job_id=job_id)

    def close(self):
        self._cleanup()
