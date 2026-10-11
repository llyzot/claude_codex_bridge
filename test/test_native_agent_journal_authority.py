"""One Agent-journal parent has one controller, across processes and runtimes."""
import json
import os
from pathlib import Path
from types import SimpleNamespace

import pytest
from native_kernel import KernelClient,KernelUnavailable,legacy_writer
from test_native_records import job

ROOT=Path(__file__).resolve().parents[1]
BIN=Path(os.environ.get('CCB_TEST_KERNEL_BIN') or ROOT/'rust/target/release'/('ccb-kernel.exe' if os.name=='nt' else 'ccb-kernel'))


@pytest.fixture(autouse=True)
def binary(monkeypatch):monkeypatch.setenv('CCB_KERNEL_BIN',str(BIN))


def client(root):return KernelClient(root,transport='stdio')
def read(kernel):return kernel.call('records.list',stream='jobs',agent_name='worker')
def append(kernel,id='job-one'):
    record=job(id=id,target='worker',caller='user').to_record()
    kernel.call('records.append',stream='jobs',agent_name='worker',record=record)
def marker(parent):return parent/'agents/.ccb-journal-owner.json'


def test_live_and_stopped_foreign_controller_cannot_read_or_write_shared_agent_journals(tmp_path):
    first=client(tmp_path/'first');second=client(tmp_path/'second')
    try:
        # Both root-local services may exist; neither shares Agent data before
        # the first explicit access binds its complete runtime parent.
        second.close()
        append(first)
        before=(tmp_path/'agents/worker/jobs.jsonl').read_bytes()
        second=client(tmp_path/'second')
        with pytest.raises(ValueError,match='Agent journal'):read(second)
        with pytest.raises(ValueError,match='Agent journal'):append(second,'foreign-job')
        first.close()
        with pytest.raises(ValueError,match='another controller'):read(second)
        assert (tmp_path/'agents/worker/jobs.jsonl').read_bytes()==before
        assert json.loads(marker(tmp_path).read_text())['controller_dir']=='first'
    finally:first.close();second.close()


def test_process_death_releases_lock_but_does_not_transfer_persistent_ownership(tmp_path):
    first=client(tmp_path/'ccbd')
    append(first);first.process.kill();first.process.wait(timeout=3);first.close()
    restored=client(tmp_path/'ccbd');foreign=client(tmp_path/'alternate')
    try:
        assert read(restored)[0]['job_id']=='job-one'
        with pytest.raises(ValueError):read(foreign)
    finally:restored.close();foreign.close()


def test_separate_runtime_parents_remain_independent(tmp_path):
    first=client(tmp_path/'one/ccbd');second=client(tmp_path/'two/ccbd')
    try:
        append(first,'one');append(second,'two')
        assert [row['job_id'] for row in read(first)]==['one']
        assert [row['job_id'] for row in read(second)]==['two']
    finally:first.close();second.close()


@pytest.mark.parametrize('first_mode',['rust','python'])
def test_python_and_rust_share_marker_and_locks(first_mode,tmp_path):
    root=tmp_path/'ccbd';layout=SimpleNamespace(ccbd_dir=root)
    if first_mode=='python':
        with legacy_writer(layout):pass
    else:
        initial=client(root);append(initial);initial.close()
    before=marker(tmp_path).read_bytes()
    kernel=client(root)
    try:
        read(kernel)
        with pytest.raises(KernelUnavailable):
            with legacy_writer(layout):pytest.fail('Python overlapped native writer')
        with pytest.raises(KernelUnavailable):
            with legacy_writer(SimpleNamespace(ccbd_dir=tmp_path/'alternate')):pytest.fail('alternate Python writer was admitted')
    finally:kernel.close()
    with legacy_writer(layout):
        with legacy_writer(layout):assert marker(tmp_path).read_bytes()==before
        other=client(tmp_path/'alternate')
        try:
            with pytest.raises(ValueError):read(other)
        finally:other.close()
    with pytest.raises(KernelUnavailable):
        with legacy_writer(SimpleNamespace(ccbd_dir=tmp_path/'alternate')):pytest.fail('ownership was transferred')


def test_python_guard_does_not_relabel_errors_from_the_guarded_operation(tmp_path):
    with pytest.raises(ValueError,match='operation validation'):
        with legacy_writer(SimpleNamespace(ccbd_dir=tmp_path/'ccbd')):
            raise ValueError('operation validation')


def test_python_layout_cannot_write_journals_outside_its_bound_parent(tmp_path):
    layout=SimpleNamespace(ccbd_dir=tmp_path/'one/ccbd',agents_dir=tmp_path/'other/agents')
    with pytest.raises(KernelUnavailable,match='layout'):
        with legacy_writer(layout):pytest.fail('mismatched layout was admitted')
    assert not (tmp_path/'one').exists() and not (tmp_path/'other').exists()


def test_journal_replay_checks_parent_ownership_before_any_write(tmp_path):
    owner=client(tmp_path/'owner');append(owner);owner.close()
    logs=tmp_path/'agents/worker/jobs.jsonl';before=logs.read_bytes()
    root=tmp_path/'other';(root/'kernel').mkdir(parents=True)
    operations=[dict(kind='save',path='messages/messages.jsonl',bytes=list(b'{"should_not":"appear"}\n')),
                dict(kind='append',path='agents/worker/jobs.jsonl',offset=len(before),bytes=list(b'{"job_id":"foreign"}\n'))]
    pending=root/'kernel/pending.json';pending.write_text(json.dumps(operations));journal=pending.read_bytes()
    with pytest.raises(KernelUnavailable):client(root)
    assert not (root/'messages/messages.jsonl').exists()
    assert pending.read_bytes()==journal and logs.read_bytes()==before


def test_ambiguous_legacy_parent_is_preserved_without_guessing_an_owner(tmp_path):
    old=tmp_path/'old';(old/'kernel').mkdir(parents=True);(old/'kernel/writer.lock').touch()
    (old/'messages').mkdir();(old/'messages/messages.jsonl').write_text('{}\n')
    logs=tmp_path/'agents/worker';logs.mkdir(parents=True);(logs/'jobs.jsonl').write_text('{}\n')
    kernel=client(tmp_path/'new')
    try:
        with pytest.raises(ValueError,match='legacy'):read(kernel)
        assert not marker(tmp_path).exists() and (logs/'jobs.jsonl').read_text()=='{}\n'
    finally:kernel.close()


def test_retrying_an_unproven_legacy_claim_does_not_turn_it_into_ownership(tmp_path):
    logs=tmp_path/'agents/worker';logs.mkdir(parents=True);(logs/'jobs.jsonl').write_text('{}\n')
    for _ in range(2):
        kernel=client(tmp_path/'new')
        try:
            with pytest.raises(ValueError,match='known controller'):read(kernel)
        finally:kernel.close()
    assert not marker(tmp_path).exists() and (logs/'jobs.jsonl').read_text()=='{}\n'


def test_unambiguous_existing_controller_can_bind_legacy_logs(tmp_path):
    root=tmp_path/'ccbd';(root/'messages').mkdir(parents=True);(root/'messages/messages.jsonl').write_text('')
    logs=tmp_path/'agents/worker';logs.mkdir(parents=True)
    (logs/'jobs.jsonl').write_text(json.dumps(job(target='worker',caller='user').to_record())+'\n')
    kernel=client(root)
    try:
        assert read(kernel)[0]['job_id']=='job-original'
        assert json.loads(marker(tmp_path).read_text())['controller_dir']=='ccbd'
    finally:kernel.close()


def test_legacy_peer_that_is_still_live_is_not_treated_as_an_unused_probe(tmp_path):
    old=client(tmp_path/'old');new=client(tmp_path/'new')
    try:
        with pytest.raises(ValueError,match='active'):read(new)
        assert old.process.poll() is None and not marker(tmp_path).exists()
    finally:old.close();new.close()


def test_whole_runtime_parent_move_preserves_relative_controller_identity(tmp_path):
    original=tmp_path/'original';kernel=client(original/'ccbd');append(kernel);kernel.close()
    moved=tmp_path/'moved'
    assert original.resolve().parent==tmp_path.resolve() and moved.absolute().parent==tmp_path.resolve() and not moved.exists()
    original.rename(moved)
    restored=client(moved/'ccbd')
    try:assert read(restored)[0]['job_id']=='job-one'
    finally:restored.close()


@pytest.mark.parametrize('content',['not-json',json.dumps({'schema_version':1,'record_type':'ccb_agent_journal_owner','controller_dir':'elsewhere'})])
def test_bad_owner_marker_is_not_replaced(tmp_path,content):
    path=marker(tmp_path);path.parent.mkdir();path.write_text(content)
    kernel=client(tmp_path/'ccbd')
    try:
        with pytest.raises(ValueError):read(kernel)
        assert path.read_text()==content
    finally:kernel.close()
