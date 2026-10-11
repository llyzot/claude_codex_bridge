"""Shared Agent-journal authority for the transitional Python writer.

The marker is portable with its whole runtime parent: it stores a controller
directory name, not a PID, timeout or machine-specific absolute path.
"""
from __future__ import annotations

from contextlib import contextmanager
import json
import os
from pathlib import Path
import stat
import tempfile

OWNER='.ccb-journal-owner.json'
LOCK='.ccb-journal-owner.lock'


def _unlinked(path: Path) -> None:
    try:metadata=path.lstat()
    except FileNotFoundError:return
    if stat.S_ISLNK(metadata.st_mode) or getattr(metadata,'st_file_attributes',0)&0x400:
        raise ValueError('linked Agent journal authority path')


@contextmanager
def _lock(path: Path):
    _unlinked(path)
    with path.open('a+b') as handle:
        handle.seek(0)
        if os.name=='nt':
            import msvcrt
            acquire=lambda:msvcrt.locking(handle.fileno(),msvcrt.LK_NBLCK,1)
            release=lambda:msvcrt.locking(handle.fileno(),msvcrt.LK_UNLCK,1)
        else:
            import fcntl
            acquire=lambda:fcntl.flock(handle.fileno(),fcntl.LOCK_EX|fcntl.LOCK_NB)
            release=lambda:fcntl.flock(handle.fileno(),fcntl.LOCK_UN)
        acquire()
        try:yield handle
        finally:release()


def _check_peers(root: Path, agents: Path, known_controller: bool) -> None:
    has_logs=False
    for entry in agents.iterdir():
        if entry.is_dir():
            _unlinked(entry)
            has_logs |= (entry/'jobs.jsonl').exists() or (entry/'events.jsonl').exists()
    if has_logs and not known_controller:
        raise ValueError('legacy Agent journals lack a known controller; explicit migration is required')
    count=0
    for peer in root.parent.iterdir():
        if not peer.is_dir():continue
        count+=1
        if count>4096:raise ValueError('too many runtime-parent entries to establish journal ownership')
        _unlinked(peer)
        if peer.resolve()==root or peer==agents:continue
        writer=peer/'kernel/writer.lock'
        evidence=any((peer/name).exists() for name in [
            'lease.json','configuration.json','submissions.jsonl','messages/messages.jsonl',
            'attempts/attempts.jsonl','callbacks/edges.jsonl','kernel/pending.json'])
        if writer.exists():
            _unlinked(peer/'kernel')
            with _lock(writer):
                if has_logs or evidence:
                    raise ValueError('ambiguous legacy Agent journal parent; use its original controller directory')
        elif evidence:raise ValueError('another controller already owns data in this runtime parent')


@contextmanager
def agent_journal_authority(controller: Path, *, known_controller: bool):
    root=Path(controller).resolve()
    agents=root.parent/'agents'
    _unlinked(agents);agents.mkdir(parents=True,exist_ok=True);_unlinked(agents)
    with _lock(agents/LOCK):
        marker=agents/OWNER;_unlinked(marker)
        if marker.exists():
            with marker.open('rb') as handle:raw=handle.read(8193)
            if len(raw)>8192:raise ValueError('Agent journal authority record too large')
            record=json.loads(raw)
            if (not isinstance(record,dict) or type(record.get('schema_version')) is not int
                    or record['schema_version']!=1 or record.get('record_type')!='ccb_agent_journal_owner'
                    or record.get('controller_dir')!=root.name):
                raise ValueError('Agent journals belong to another controller directory; use a separate runtime parent')
        else:
            _check_peers(root,agents,known_controller)
            record=dict(schema_version=1,record_type='ccb_agent_journal_owner',controller_dir=root.name)
            name=None
            try:
                with tempfile.NamedTemporaryFile(mode='wb',dir=agents,prefix='.ccb-owner-',delete=False) as handle:
                    name=Path(handle.name)
                    handle.write(json.dumps(record,ensure_ascii=False).encode('utf-8'));handle.flush();os.fsync(handle.fileno())
                # Windows rename and Unix hard-link creation reject an existing
                # destination, matching Rust's persist_noclobber contract.
                if os.name=='nt':os.rename(name,marker)
                else:os.link(name,marker)
                if os.name!='nt':
                    descriptor=os.open(agents,os.O_RDONLY)
                    try:os.fsync(descriptor)
                    finally:os.close(descriptor)
            finally:
                if name is not None:name.unlink(missing_ok=True)
        yield
