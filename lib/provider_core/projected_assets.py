from __future__ import annotations

from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import tempfile
from collections import deque
from concurrent.futures import ThreadPoolExecutor

_HASH_CHUNK_SIZE = 64 * 1024


def _is_junction(path: Path) -> bool:
    if os.name != 'nt':
        return False
    try:
        return getattr(path.lstat(), 'st_reparse_tag', None) == 0xA0000003
    except OSError:
        return False


def _create_windows_junction(source: Path, target: Path) -> None:
    import ctypes
    from ctypes import wintypes
    import struct

    destination = str(source.absolute())
    substitute = '\\??\\UNC\\' + destination[2:] if destination.startswith('\\\\') else '\\??\\' + destination
    substitute_bytes = substitute.encode('utf-16-le')
    print_bytes = destination.encode('utf-16-le')
    paths = substitute_bytes + b'\0\0' + print_bytes + b'\0\0'
    data = struct.pack('<IHHHHHH', 0xA0000003, 8 + len(paths), 0,
                       0, len(substitute_bytes), len(substitute_bytes) + 2, len(print_bytes)) + paths
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                                  wintypes.LPVOID, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
    kernel.CreateFileW.restype = wintypes.HANDLE
    kernel.DeviceIoControl.argtypes = [wintypes.HANDLE, wintypes.DWORD, wintypes.LPVOID,
                                      wintypes.DWORD, wintypes.LPVOID, wintypes.DWORD,
                                      ctypes.POINTER(wintypes.DWORD), wintypes.LPVOID]
    kernel.DeviceIoControl.restype = wintypes.BOOL
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    target.mkdir()
    target_name = str(target.absolute())
    if not target_name.startswith('\\\\?\\'):
        target_name = '\\\\?\\UNC\\' + target_name[2:] if target_name.startswith('\\\\') else '\\\\?\\' + target_name
    handle = kernel.CreateFileW(target_name, 0x40000000, 7, None, 3, 0x02200000, None)
    if handle == ctypes.c_void_p(-1).value:
        target.rmdir()
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        buffer = ctypes.create_string_buffer(data)
        returned = wintypes.DWORD()
        if not kernel.DeviceIoControl(handle, 0x000900A4, buffer, len(data), None, 0,
                                      ctypes.byref(returned), None):
            raise ctypes.WinError(ctypes.get_last_error())
    except Exception:
        kernel.CloseHandle(handle)
        target.rmdir()
        raise
    else:
        kernel.CloseHandle(handle)


def route_live_tree(source: Path, target: Path, *, enabled: bool = True,
                    label: str = 'projected-tree', allow_unmarked_replace: bool = False) -> bool:
    """Reference native resources; never silently replace a link with a copy."""
    del allow_unmarked_replace
    source, target = Path(source).expanduser(), Path(target).expanduser()
    marker = _default_marker_path(target)
    if marker.exists() and not _marker_matches(marker, label=label, source=None):
        return False
    if not enabled or not source.is_dir():
        remove_projected_path(target, label=label)
        return False
    if _same_path(source, target) and not (target.is_symlink() or _is_junction(target)):
        return True
    if _projection_points_to(target, source):
        return _write_projection_marker(marker, source=source,
                                        mode='junction' if _is_junction(target) else 'symlink', label=label)
    if target.exists() or target.is_symlink() or _is_junction(target):
        if not _marker_matches(marker, label=label, source=None):
            return False
        if not (target.is_symlink() or _is_junction(target)):
            if not target.resolve().is_relative_to(target.parent.resolve()):
                raise RuntimeError('live projection target escaped its owned parent')
            if source.resolve().is_relative_to(target.resolve()):
                raise RuntimeError('live projection source is inside replacement target')
        _remove_path(target)
    target.parent.mkdir(parents=True, exist_ok=True)
    try:
        target.symlink_to(source.absolute(), target_is_directory=True)
        mode = 'symlink'
    except OSError:
        if os.name != 'nt':
            raise
        _create_windows_junction(source, target)
        mode = 'junction'
    if _write_projection_marker(marker, source=source, mode=mode, label=label):
        return True
    _remove_path(target)
    return False


def route_projected_tree(
    source: Path,
    target: Path,
    *,
    enabled: bool = True,
    label: str = 'projected-tree',
    marker_path: Path | None = None,
    allow_unmarked_replace: bool = False,
) -> bool:
    source = Path(source).expanduser()
    target = Path(target).expanduser()
    marker = marker_path or _default_marker_path(target)
    marker_present = marker.exists() or marker.is_symlink()
    marker_owned = _marker_matches(marker, label=label, source=None)

    if marker_present and not marker_owned:
        return False

    if not enabled or not source.is_dir():
        _remove_projected_target(target, marker, label=label)
        return False
    if _same_path(source, target) and not target.is_symlink():
        return True
    target.parent.mkdir(parents=True, exist_ok=True)
    if _projection_points_to(target, source):
        return _write_projection_marker(marker, source=source, mode='symlink', label=label)
    if target.exists() or target.is_symlink():
        if not _can_replace_projected_target(
            target,
            marker,
            label=label,
            allow_unmarked_replace=allow_unmarked_replace,
        ):
            return False
        _remove_path(target)
    try:
        target.symlink_to(source, target_is_directory=True)
        if _write_projection_marker(marker, source=source, mode='symlink', label=label):
            return True
    except Exception:
        pass
    _remove_path(target)
    try:
        shutil.copytree(source, target)
        if _write_projection_marker(marker, source=source, mode='copy', label=label):
            return True
    except Exception:
        pass
    _remove_path(target)
    return False


def copy_projected_tree_to_cache(
    source: Path,
    bundle_root: Path,
    *,
    label: str = 'projected-tree',
    marker_source: Path | None = None,
) -> bool:
    source = Path(source).expanduser()
    bundle_root = Path(bundle_root).expanduser()
    marker_authority = (
        Path(marker_source).expanduser()
        if marker_source is not None
        else source
    )
    if not source.is_dir():
        return False
    if bundle_root.is_symlink() or not tree_symlinks_are_self_contained(source):
        return False
    source_fingerprint = tree_content_fingerprint(source)
    if not source_fingerprint:
        return False
    if bundle_root.is_dir():
        if (
            not tree_symlinks_are_self_contained(bundle_root)
            or tree_content_fingerprint(bundle_root) != source_fingerprint
        ):
            return False
        return write_projected_marker(
            bundle_root,
            label=label,
            mode='copy',
            source=marker_authority,
        )
    bundle_root.parent.mkdir(parents=True, exist_ok=True)
    staging_root = Path(
        tempfile.mkdtemp(
            prefix=f'.{bundle_root.name}.ccb-cache-',
            dir=bundle_root.parent,
        )
    )
    candidate = staging_root / 'candidate'
    try:
        _copy_cache_tree(source, candidate)
        if (
            not tree_symlinks_are_self_contained(candidate)
            or tree_content_fingerprint(candidate) != source_fingerprint
        ):
            return False
        try:
            candidate.rename(bundle_root)
        except OSError:
            # Another materializer may have published the same content-addressed
            # bundle first. Accept only a complete winning tree.
            if (
                bundle_root.is_symlink()
                or not tree_symlinks_are_self_contained(bundle_root)
                or tree_content_fingerprint(bundle_root) != source_fingerprint
            ):
                return False
        if (
            bundle_root.is_symlink()
            or not tree_symlinks_are_self_contained(bundle_root)
            or tree_content_fingerprint(bundle_root) != source_fingerprint
        ):
            return False
        if not write_projected_marker(
            bundle_root,
            label=label,
            mode='copy',
            source=marker_authority,
        ):
            raise OSError(f'failed to write projection marker: {bundle_root}')
    except Exception:
        return False
    finally:
        _remove_path(staging_root)
    return True


def seed_projected_tree(
    source: Path,
    target: Path,
    *,
    enabled: bool = True,
    label: str = 'projected-tree',
    marker_path: Path | None = None,
) -> bool:
    """Seed a writable local tree without replacing user-owned target data."""
    source = Path(source).expanduser()
    target = Path(target).expanduser()
    marker = marker_path or _default_marker_path(target)

    if not enabled:
        remove_projected_path(target, label=label, marker_path=marker)
        return False
    if not source.is_dir():
        return False
    if _same_path(source, target) and not target.is_symlink():
        return True

    source_fingerprint = tree_metadata_fingerprint(source)
    if not source_fingerprint:
        return False
    owned = _marker_matches(marker, label=label, source=None)
    target_present = target.exists() or target.is_symlink()
    if target_present and not owned:
        return False
    if marker.exists() and not owned:
        return False

    marker_payload = _read_projection_marker(marker) if owned else {}
    if (
        target.is_dir()
        and not target.is_symlink()
        and _marker_matches(marker, label=label, source=source)
        and marker_payload.get('mode') == 'copy-seed'
        and marker_payload.get('source_fingerprint') == source_fingerprint
    ):
        return True

    target.parent.mkdir(parents=True, exist_ok=True)
    staging_root = Path(tempfile.mkdtemp(prefix=f'.{target.name}.ccb-seed-', dir=target.parent))
    staged_target = staging_root / 'candidate'
    previous_target = staging_root / 'previous'
    moved_previous = False
    installed_candidate = False
    try:
        shutil.copytree(source, staged_target)
        if target_present:
            target.rename(previous_target)
            moved_previous = True
        staged_target.rename(target)
        installed_candidate = True
        if not _write_projection_marker(
            marker,
            source=source,
            mode='copy-seed',
            label=label,
            source_fingerprint=source_fingerprint,
        ):
            raise OSError(f'failed to write projection marker: {marker}')
        _remove_path(previous_target)
        return True
    except Exception:
        if installed_candidate:
            _remove_path(target)
        if moved_previous and (previous_target.exists() or previous_target.is_symlink()):
            try:
                previous_target.rename(target)
            except Exception:
                pass
        return False
    finally:
        _remove_path(staging_root)


def seed_projected_file(
    source: Path,
    target: Path,
    *,
    enabled: bool = True,
    label: str = 'projected-file',
    marker_path: Path | None = None,
) -> bool:
    """Seed a writable local file without replacing user-owned target data."""
    source = Path(source).expanduser()
    target = Path(target).expanduser()
    marker = marker_path or _default_marker_path(target)

    if not enabled:
        remove_projected_path(target, label=label, marker_path=marker)
        return False
    if not source.is_file() or source.is_symlink():
        return False
    if _same_path(source, target) and not target.is_symlink():
        return True

    source_fingerprint = _file_metadata_fingerprint(source)
    if not source_fingerprint:
        return False
    owned = _marker_matches(marker, label=label, source=None)
    target_present = target.exists() or target.is_symlink()
    if target_present and not owned:
        return False
    if marker.exists() and not owned:
        return False

    marker_payload = _read_projection_marker(marker) if owned else {}
    if (
        target.is_file()
        and not target.is_symlink()
        and _marker_matches(marker, label=label, source=source)
        and marker_payload.get('mode') == 'copy-seed'
        and marker_payload.get('source_fingerprint') == source_fingerprint
    ):
        return True

    target.parent.mkdir(parents=True, exist_ok=True)
    staging_root = Path(tempfile.mkdtemp(prefix=f'.{target.name}.ccb-seed-', dir=target.parent))
    staged_target = staging_root / 'candidate'
    previous_target = staging_root / 'previous'
    moved_previous = False
    installed_candidate = False
    try:
        shutil.copy2(source, staged_target, follow_symlinks=False)
        if target_present:
            target.rename(previous_target)
            moved_previous = True
        staged_target.rename(target)
        installed_candidate = True
        if not _write_projection_marker(
            marker,
            source=source,
            mode='copy-seed',
            label=label,
            source_fingerprint=source_fingerprint,
        ):
            raise OSError(f'failed to write projection marker: {marker}')
        _remove_path(previous_target)
        return True
    except Exception:
        if installed_candidate:
            _remove_path(target)
        if moved_previous and (previous_target.exists() or previous_target.is_symlink()):
            try:
                previous_target.rename(target)
            except Exception:
                pass
        return False
    finally:
        _remove_path(staging_root)


def ensure_shared_tree_bundle(source: Path, bundle_root: Path) -> Path | None:
    return bundle_root if copy_projected_tree_to_cache(source, bundle_root) else None


def remove_projected_path(
    target: Path,
    *,
    label: str = 'projected-tree',
    source: Path | None = None,
    marker_path: Path | None = None,
    allow_unmarked_replace: bool = False,
) -> None:
    del allow_unmarked_replace
    target = Path(target).expanduser()
    marker = marker_path or _default_marker_path(target)
    if not _marker_matches(marker, label=label, source=source):
        return
    _remove_projected_target(target, marker, label=label)


def projected_path_is_owned(
    target: Path,
    *,
    label: str = 'projected-tree',
    source: Path | None = None,
    marker_path: Path | None = None,
) -> bool:
    target = Path(target).expanduser()
    marker = marker_path or _default_marker_path(target)
    return _marker_matches(marker, label=label, source=source)


def remove_projected_tree(
    target: Path,
    *,
    marker_path: Path | None = None,
    allow_unmarked_replace: bool = False,
) -> None:
    remove_projected_path(target, marker_path=marker_path, allow_unmarked_replace=allow_unmarked_replace)


def write_projected_marker(target: Path, *, label: str, mode: str, source: Path) -> bool:
    return _write_projection_marker(
        _default_marker_path(Path(target).expanduser()),
        source=source,
        mode=mode,
        label=label,
    )


def tree_content_fingerprint(root: Path) -> str:
    root = Path(root).expanduser()
    digest = hashlib.sha256()
    try:
        digest.update(b'root\0')
        digest.update(str(stat.S_IMODE(root.stat().st_mode)).encode('ascii'))
        digest.update(b'\0')
        records = []
        for entry in sorted(root.rglob('*')):
            metadata = entry.lstat()
            mode = metadata.st_mode
            kind = 'l' if stat.S_ISLNK(mode) else 'd' if stat.S_ISDIR(mode) else 'f' if stat.S_ISREG(mode) else 'o'
            records.append((entry, kind, metadata))
        files = iter((entry, metadata.st_size) for entry, kind, metadata in records if kind == 'f')
        workers = 8 if os.name == 'nt' else 1
        with ThreadPoolExecutor(max_workers=workers) as reads:
            pending = deque()

            def queue_next():
                item = next(files, None)
                if item is not None:
                    pending.append(reads.submit(_read_small_fingerprint_file, *item))

            for _ in range(workers):
                queue_next()
            for entry, kind, metadata in records:
                digest.update(kind.encode('utf-8'))
                digest.update(b'\0')
                digest.update(str(entry.relative_to(root)).encode('utf-8', errors='ignore'))
                digest.update(b'\0')
                if kind == 'l':
                    digest.update(str(entry.readlink()).encode('utf-8', errors='ignore'))
                elif kind == 'f':
                    digest.update(str(stat.S_IMODE(metadata.st_mode)).encode('ascii'))
                    digest.update(b'\0')
                    content = pending.popleft().result()
                    if content is not None:
                        digest.update(content)
                    else:
                        with entry.open('rb') as handle:
                            for chunk in iter(lambda: handle.read(_HASH_CHUNK_SIZE), b''):
                                digest.update(chunk)
                    queue_next()
                elif kind == 'd':
                    digest.update(str(stat.S_IMODE(metadata.st_mode)).encode('ascii'))
                digest.update(b'\0')
    except Exception:
        return ''
    return digest.hexdigest()


def _read_small_fingerprint_file(path: Path, size: int) -> bytes | None:
    # Bound buffering to at most workers * 2 MiB. Large files retain streaming.
    limit = 2 * 1024 * 1024
    if size > limit:
        return None
    with path.open('rb') as handle:
        content = handle.read(limit + 1)
    return content if len(content) <= limit else None


def _copy_cache_tree(source: Path, target: Path) -> None:
    if os.name != 'nt':
        shutil.copytree(source, target, symlinks=True)
        return
    with ThreadPoolExecutor(max_workers=8) as copies:
        pending = []

        def copy_file(src, dst):
            pending.append(copies.submit(shutil.copy2, src, dst))
            return dst

        shutil.copytree(source, target, symlinks=True, copy_function=copy_file)
        for copy in pending:
            copy.result()


def tree_metadata_fingerprint(root: Path) -> str:
    root = Path(root).expanduser()
    digest = hashlib.sha256()
    try:
        for entry in sorted(root.rglob('*')):
            relative = entry.relative_to(root)
            if entry.is_symlink():
                kind = 'l'
            elif entry.is_dir():
                kind = 'd'
            elif entry.is_file():
                kind = 'f'
            else:
                kind = 'o'
            digest.update(kind.encode('utf-8'))
            digest.update(b'\0')
            digest.update(str(relative).encode('utf-8', errors='ignore'))
            digest.update(b'\0')
            if entry.is_symlink():
                digest.update(str(entry.readlink()).encode('utf-8', errors='ignore'))
            elif entry.is_file():
                stat = entry.stat()
                digest.update(str(stat.st_size).encode('utf-8'))
                digest.update(b'\0')
                digest.update(str(stat.st_mtime_ns).encode('utf-8'))
            digest.update(b'\0')
    except Exception:
        return ''
    return digest.hexdigest()


def _file_metadata_fingerprint(path: Path) -> str:
    try:
        metadata = path.stat(follow_symlinks=False)
    except OSError:
        return ''
    digest = hashlib.sha256()
    digest.update(str(metadata.st_size).encode('utf-8'))
    digest.update(b'\0')
    digest.update(str(metadata.st_mtime_ns).encode('utf-8'))
    return digest.hexdigest()


def _remove_projected_target(target: Path, marker: Path, *, label: str) -> None:
    if _marker_matches(marker, label=label, source=None):
        _remove_path(target)
        marker.unlink(missing_ok=True)


def _projection_points_to(target: Path, source: Path) -> bool:
    if not (target.is_symlink() or _is_junction(target)):
        return False
    try:
        return target.resolve() == source.resolve()
    except Exception:
        try:
            return target.readlink() == source
        except Exception:
            return False


def _can_replace_projected_target(
    target: Path,
    marker: Path,
    *,
    label: str,
    allow_unmarked_replace: bool,
) -> bool:
    del allow_unmarked_replace
    if _marker_matches(marker, label=label, source=None):
        return True
    if marker.exists() or marker.is_symlink():
        return False
    return not target.exists() and not target.is_symlink()


def tree_symlinks_are_self_contained(
    root: Path,
    *,
    ignored_names: tuple[str, ...] = (),
) -> bool:
    try:
        if root.is_symlink() or not root.is_dir():
            return False
        resolved_root = root.resolve(strict=True)
        ignored = set(ignored_names)
        for entry in root.rglob('*'):
            if ignored.intersection(entry.relative_to(root).parts):
                continue
            if not entry.is_symlink():
                continue
            link = entry.readlink()
            if link.is_absolute():
                return False
            (entry.parent / link).resolve(strict=True).relative_to(resolved_root)
    except (OSError, RuntimeError, ValueError):
        return False
    return True


def _default_marker_path(target: Path) -> Path:
    return Path(f'{target}.ccb-projection.json')


def _marker_matches(marker: Path, *, label: str, source: Path | None) -> bool:
    if marker.is_symlink() or not marker.is_file():
        return False
    payload = _read_projection_marker(marker)
    if payload.get('schema_version') != 1:
        return False
    if payload.get('record_type') != 'ccb_projected_asset':
        return False
    if str(payload.get('label') or '') != label:
        return False
    marker_source = str(payload.get('source') or '').strip()
    if not marker_source:
        return False
    if str(payload.get('mode') or '') not in {'symlink', 'junction', 'copy', 'copy-seed'}:
        return False
    if source is None:
        return True
    try:
        return Path(marker_source).expanduser().resolve() == Path(source).expanduser().resolve()
    except Exception:
        return marker_source == str(source)


def _read_projection_marker(path: Path) -> dict[str, object]:
    try:
        payload = json.loads(path.read_text(encoding='utf-8'))
    except Exception:
        return {}
    return payload if isinstance(payload, dict) else {}


def _write_projection_marker(
    path: Path,
    *,
    source: Path,
    mode: str,
    label: str,
    source_fingerprint: str | None = None,
) -> bool:
    payload = {
        'schema_version': 1,
        'record_type': 'ccb_projected_asset',
        'label': label,
        'source': str(source),
        'mode': mode,
        'updated_at': datetime.now(timezone.utc).isoformat().replace('+00:00', 'Z'),
    }
    if source_fingerprint:
        payload['source_fingerprint'] = source_fingerprint
    descriptor: int | None = None
    tmp_path: Path | None = None
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        descriptor, tmp_name = tempfile.mkstemp(prefix=f'.{path.name}.', suffix='.tmp', dir=path.parent)
        tmp_path = Path(tmp_name)
        with os.fdopen(descriptor, 'w', encoding='utf-8') as handle:
            handle.write(json.dumps(payload, ensure_ascii=True, sort_keys=True) + '\n')
        descriptor = None
        os.replace(tmp_path, path)
        return True
    except Exception:
        if descriptor is not None:
            try:
                os.close(descriptor)
            except OSError:
                pass
        if tmp_path is not None:
            tmp_path.unlink(missing_ok=True)
        return False


def _remove_path(path: Path) -> None:
    if _is_junction(path):
        path.rmdir()
        return
    if path.is_symlink() or path.is_file():
        path.unlink(missing_ok=True)
        return
    if path.is_dir():
        shutil.rmtree(path, ignore_errors=True)


def _same_path(left: Path, right: Path) -> bool:
    try:
        return left.resolve() == right.resolve()
    except Exception:
        return left == right


__all__ = [
    'copy_projected_tree_to_cache',
    'ensure_shared_tree_bundle',
    'projected_path_is_owned',
    'remove_projected_path',
    'remove_projected_tree',
    'route_projected_tree',
    'route_live_tree',
    'seed_projected_file',
    'seed_projected_tree',
    'tree_content_fingerprint',
    'tree_symlinks_are_self_contained',
    'tree_metadata_fingerprint',
    'write_projected_marker',
]
