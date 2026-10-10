import hashlib
import os
from pathlib import Path
import stat
import threading
import time

from provider_core import projected_assets as assets


def legacy_fingerprint(root):
    digest = hashlib.sha256()
    digest.update(b'root\0')
    digest.update(str(stat.S_IMODE(root.stat().st_mode)).encode('ascii'))
    digest.update(b'\0')
    for entry in sorted(root.rglob('*')):
        kind = 'l' if entry.is_symlink() else 'd' if entry.is_dir() else 'f' if entry.is_file() else 'o'
        digest.update(kind.encode()); digest.update(b'\0')
        digest.update(str(entry.relative_to(root)).encode('utf-8', errors='ignore')); digest.update(b'\0')
        if entry.is_symlink():
            digest.update(str(entry.readlink()).encode('utf-8', errors='ignore'))
        elif entry.is_file():
            digest.update(str(stat.S_IMODE(entry.stat().st_mode)).encode('ascii')); digest.update(b'\0')
            with entry.open('rb') as handle:
                for chunk in iter(lambda: handle.read(65536), b''):
                    digest.update(chunk)
        elif entry.is_dir():
            digest.update(str(stat.S_IMODE(entry.stat().st_mode)).encode('ascii'))
        digest.update(b'\0')
    return digest.hexdigest()


def test_parallel_fingerprint_preserves_legacy_digest_and_bounds_workers(tmp_path, monkeypatch):
    source = tmp_path / 'source'
    (source / 'nested').mkdir(parents=True)
    for index in range(24):
        (source / 'nested' / f'{index}.txt').write_bytes(bytes([index]) * 100)
    (source / 'empty').write_bytes(b'')
    (source / 'large').write_bytes(b'x' * (3 * 1024 * 1024))
    expected = legacy_fingerprint(source)
    lock = threading.Lock()
    active = peak = 0
    original = assets._read_small_fingerprint_file

    def delayed(*args):
        nonlocal active, peak
        with lock:
            active += 1
            peak = max(peak, active)
        try:
            time.sleep(0.005)
            return original(*args)
        finally:
            with lock:
                active -= 1
    monkeypatch.setattr(assets, '_read_small_fingerprint_file', delayed)
    assert assets.tree_content_fingerprint(source) == expected
    assert peak <= (8 if os.name == 'nt' else 1)
    if os.name == 'nt':
        assert peak > 1


def test_parallel_copy_failure_cannot_publish_unverified_tree(tmp_path, monkeypatch):
    source = tmp_path / 'source'
    source.mkdir()
    (source / 'file').write_text('content')
    def fail(*args, **kwargs):
        raise OSError('copy failed')
    monkeypatch.setattr(assets.shutil, 'copy2', fail)
    target = tmp_path / 'cache' / 'bundle'
    assert not assets.copy_projected_tree_to_cache(source, target)
    assert not target.exists()
    assert not list(target.parent.glob('*.ccb-cache-*'))


def test_warm_parallel_cache_is_reused_and_tampering_rejected(tmp_path, monkeypatch):
    source = tmp_path / 'source'
    source.mkdir()
    (source / 'file').write_text('content')
    target = tmp_path / 'cache' / 'bundle'
    assert assets.copy_projected_tree_to_cache(source, target)
    def fail(*args, **kwargs):
        raise AssertionError('warm cache must not copy')
    monkeypatch.setattr(assets.shutil, 'copytree', fail)
    assert assets.copy_projected_tree_to_cache(source, target)
    (target / 'file').write_text('tampered')
    assert not assets.copy_projected_tree_to_cache(source, target)
