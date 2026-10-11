from __future__ import annotations

from dataclasses import dataclass
import os
from pathlib import Path
import subprocess
import tempfile

from .models import VerificationCommand


CONTROLLER_NAME = 'CCB Controller'
CONTROLLER_EMAIL = 'ccb-controller@localhost'
_CONTROLLER_ENV = {
    'GIT_AUTHOR_NAME': CONTROLLER_NAME,
    'GIT_AUTHOR_EMAIL': CONTROLLER_EMAIL,
    'GIT_COMMITTER_NAME': CONTROLLER_NAME,
    'GIT_COMMITTER_EMAIL': CONTROLLER_EMAIL,
}
_MAX_VERIFICATION_OUTPUT_BYTES = 65_536
_CONTROLLER_STATE_PATH_PREFIXES = ('.ccb/', 'docs/plantree/')
_CONTROLLER_STATE_PATHS = ('.ccb', 'docs/plantree', '.ccb-workspace.json')
_GENERATED_PATH_PREFIXES = (
    '.mypy_cache/',
    '.pytest_cache/',
    '.ruff_cache/',
    '__pycache__/',
)
_GENERATED_PATHS = ('.mypy_cache', '.pytest_cache', '.ruff_cache', '__pycache__')


@dataclass(frozen=True)
class GitCommandResult:
    argv: tuple[str, ...]
    returncode: int
    stdout: str
    stderr: str


def _is_controller_state_path(path: str) -> bool:
    normalized = str(path).replace('\\', '/')
    while normalized.startswith('./'):
        normalized = normalized[2:]
    return normalized in _CONTROLLER_STATE_PATHS or normalized.startswith(
        _CONTROLLER_STATE_PATH_PREFIXES
    )


def _is_generated_runtime_path(path: str) -> bool:
    normalized = str(path).replace('\\', '/')
    while normalized.startswith('./'):
        normalized = normalized[2:]
    if normalized in _GENERATED_PATHS or normalized.startswith(_GENERATED_PATH_PREFIXES):
        return True
    if '/__pycache__/' in normalized:
        return True
    return normalized.endswith(('.pyc', '.pyo'))


def _is_ignored_operational_path(path: str) -> bool:
    return _is_controller_state_path(path) or _is_generated_runtime_path(path)


def _status_line_is_controller_state(line: str) -> bool:
    if len(line) < 4:
        return False
    path_part = line[3:].strip()
    if not path_part:
        return False
    # Porcelain v1 reports renames as "old -> new"; ignore only if every
    # involved path is controller-owned state.
    paths = [part.strip().strip('"') for part in path_part.split(' -> ')]
    return all(_is_ignored_operational_path(path) for path in paths)


class GitOperations:
    def __init__(self, project_root: Path) -> None:
        self.project_root = Path(project_root).expanduser().resolve()
        self.object_format = self.output(self.project_root, ['rev-parse', '--show-object-format'])

    def repository_root(self) -> Path:
        return Path(self.output(self.project_root, ['rev-parse', '--show-toplevel'])).resolve()

    def repository_identity(self) -> str:
        raw = self.output(self.project_root, ['rev-parse', '--git-common-dir'])
        path = Path(raw)
        if not path.is_absolute():
            path = self.project_root / path
        return str(path.resolve())

    def resolve_commit(self, cwd: Path, ref: str = 'HEAD') -> str:
        return self.output(cwd, ['rev-parse', '--verify', f'{ref}^{{commit}}'])

    def head(self, cwd: Path) -> str:
        return self.resolve_commit(cwd, 'HEAD')

    def branch(self, cwd: Path) -> str:
        return self.output(cwd, ['branch', '--show-current'])

    def commit_tree_digest(self, cwd: Path, commit: str) -> str:
        tree = self.output(cwd, ['rev-parse', f'{commit}^{{tree}}'])
        return self.format_tree_digest(tree)

    def current_tree_digest(self, cwd: Path, *, ignore_controller_state: bool = False) -> str:
        from native_kernel import kernel_binary, rust_core_enabled
        if rust_core_enabled():
            import json
            from process_background import background_process_kwargs
            command = [str(kernel_binary()), '--inspect-tree', str(Path(cwd).resolve())]
            if not ignore_controller_state:
                command.append('--include-controller-state')
            result = subprocess.run(command, check=False, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                    text=True, encoding='utf-8', **background_process_kwargs())
            if result.returncode != 0:
                raise RuntimeError((result.stderr or 'native tree observation failed').strip())
            observed = json.loads(result.stdout)
            if observed.get('record_type') != 'git_workspace_tree':
                raise RuntimeError('invalid native tree observation response')
            return str(observed['digest'])
        cwd = Path(cwd).resolve()
        fd, raw_index = tempfile.mkstemp(prefix='ccb-git-index-')
        os.close(fd)
        index_path = Path(raw_index)
        index_path.unlink()
        env = {'GIT_INDEX_FILE': str(index_path)}
        try:
            self.run(cwd, ['read-tree', 'HEAD'], env=env)
            if ignore_controller_state:
                tracked = [
                    path
                    for path in self._nul_output(cwd, ['ls-files', '-m', '-d', '-z', '--'])
                    if not _is_ignored_operational_path(path)
                ]
                if tracked:
                    self.run(cwd, ['add', '-u', '--', *tracked], env=env)
            else:
                self.run(cwd, ['add', '-u', '--', '.'], env=env)
            untracked = list(self.untracked_paths(cwd))
            if ignore_controller_state:
                untracked = [path for path in untracked if not _is_ignored_operational_path(path)]
            if untracked:
                self.run(cwd, ['add', '--', *untracked], env=env)
            tree = self.output(cwd, ['write-tree'], env=env)
            return self.format_tree_digest(tree)
        finally:
            index_path.unlink(missing_ok=True)

    def format_tree_digest(self, oid: str) -> str:
        return f'git-tree:{self.object_format}:{str(oid).strip()}'

    def changed_paths(
        self,
        cwd: Path,
        base_commit: str,
        *,
        ignore_controller_state: bool = False,
    ) -> tuple[str, ...]:
        tracked = self._nul_output(cwd, ['diff', '--name-only', '-z', base_commit, '--'])
        untracked = self.untracked_paths(cwd)
        paths = tuple(sorted(set((*tracked, *untracked))))
        if not ignore_controller_state:
            return paths
        return tuple(path for path in paths if not _is_ignored_operational_path(path))

    def deleted_paths(
        self,
        cwd: Path,
        base_commit: str,
        *,
        ignore_controller_state: bool = False,
    ) -> tuple[str, ...]:
        paths = tuple(
            sorted(
                self._nul_output(
                    cwd,
                    ['diff', '--name-only', '--diff-filter=D', '-z', base_commit, '--'],
                )
            )
        )
        if not ignore_controller_state:
            return paths
        return tuple(path for path in paths if not _is_ignored_operational_path(path))

    def untracked_paths(self, cwd: Path) -> tuple[str, ...]:
        return tuple(
            sorted(
                self._nul_output(
                    cwd,
                    ['ls-files', '--others', '--exclude-standard', '-z', '--'],
                )
            )
        )

    def status_lines(self, cwd: Path, *, ignore_controller_state: bool = False) -> tuple[str, ...]:
        result = self.run(
            cwd,
            ['status', '--porcelain=v1', '--untracked-files=all'],
        )
        lines = tuple(line for line in result.stdout.splitlines() if line)
        if not ignore_controller_state:
            return lines
        return tuple(line for line in lines if not _status_line_is_controller_state(line))

    def is_ancestor(self, cwd: Path, ancestor: str, descendant: str) -> bool:
        result = self.run(
            cwd,
            ['merge-base', '--is-ancestor', ancestor, descendant],
            check=False,
        )
        if result.returncode not in {0, 1}:
            raise RuntimeError(result.stderr or result.stdout or 'git merge-base failed')
        return result.returncode == 0

    def commit_parents(self, cwd: Path, commit: str) -> tuple[str, ...]:
        line = self.output(cwd, ['rev-list', '--parents', '-n', '1', commit])
        parts = line.split()
        return tuple(parts[1:])

    def commit_message(self, cwd: Path, commit: str) -> str:
        raw = self.run(cwd, ['cat-file', '-p', commit]).stdout
        separator = raw.find('\n\n')
        if separator < 0:
            raise RuntimeError(f'cannot parse Git commit message for {commit}')
        message = raw[separator + 2 :]
        return message[:-1] if message.endswith('\n') else message

    def commit_identity(self, cwd: Path, commit: str) -> dict[str, str]:
        raw = self.run(
            cwd,
            ['show', '-s', '--format=%an%x00%ae%x00%cn%x00%ce', commit],
        ).stdout.rstrip('\n')
        parts = raw.split('\0')
        if len(parts) != 4:
            raise RuntimeError(f'cannot parse Git identity for commit {commit}')
        return {
            'author_name': parts[0],
            'author_email': parts[1],
            'committer_name': parts[2],
            'committer_email': parts[3],
        }

    def controller_identity(self) -> dict[str, str]:
        return {
            'author_name': CONTROLLER_NAME,
            'author_email': CONTROLLER_EMAIL,
            'committer_name': CONTROLLER_NAME,
            'committer_email': CONTROLLER_EMAIL,
        }

    def commit_all(self, cwd: Path, message: str, *, ignore_controller_state: bool = False) -> str:
        if ignore_controller_state:
            tracked = [
                path
                for path in self._nul_output(cwd, ['ls-files', '-m', '-d', '-z', '--'])
                if not _is_ignored_operational_path(path)
            ]
            untracked = [
                path
                for path in self.untracked_paths(cwd)
                if not _is_ignored_operational_path(path)
            ]
            if tracked:
                self.run(cwd, ['add', '-u', '--', *tracked])
            if untracked:
                self.run(cwd, ['add', '--', *untracked])
        else:
            self.run(cwd, ['add', '-A', '--', '.'])
        self.run(
            cwd,
            [
                '-c',
                'core.hooksPath=/dev/null',
                'commit',
                '--no-verify',
                '--no-gpg-sign',
                '-m',
                message,
            ],
            env=_CONTROLLER_ENV,
        )
        return self.head(cwd)

    def merge_no_ff(self, cwd: Path, commit: str, message: str) -> GitCommandResult:
        return self.run(
            cwd,
            [
                '-c',
                'core.hooksPath=/dev/null',
                'merge',
                '--no-ff',
                '--no-edit',
                '--no-verify',
                '--no-gpg-sign',
                '-m',
                message,
                commit,
            ],
            check=False,
            env=_CONTROLLER_ENV,
        )

    def merge_abort(self, cwd: Path) -> None:
        result = self.run(cwd, ['merge', '--abort'], check=False)
        if result.returncode != 0:
            raise RuntimeError(result.stderr or result.stdout or 'git merge --abort failed')

    def conflict_paths(self, cwd: Path) -> tuple[str, ...]:
        return tuple(
            sorted(
                self._nul_output(
                    cwd,
                    ['diff', '--name-only', '--diff-filter=U', '-z', '--'],
                )
            )
        )

    def merge_ff_only(self, cwd: Path, commit: str) -> GitCommandResult:
        return self.run(
            cwd,
            [
                '-c',
                'core.hooksPath=/dev/null',
                'merge',
                '--ff-only',
                '--no-verify',
                '--no-gpg-sign',
                commit,
            ],
            check=False,
            env=_CONTROLLER_ENV,
        )

    def reset_hard(self, cwd: Path, commit: str) -> None:
        self.run(cwd, ['reset', '--hard', commit])

    def run_verification(self, cwd: Path, command: VerificationCommand) -> dict[str, object]:
        try:
            completed = subprocess.run(
                list(command.argv),
                cwd=Path(cwd),
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=command.timeout_seconds,
            )
            stdout, stdout_truncated = _bounded_output(completed.stdout or '')
            stderr, stderr_truncated = _bounded_output(completed.stderr or '')
            return {
                **command.to_record(),
                'exit_code': completed.returncode,
                'stdout': stdout,
                'stderr': stderr,
                'stdout_truncated': stdout_truncated,
                'stderr_truncated': stderr_truncated,
                'timed_out': False,
                'result': 'pass' if completed.returncode == 0 else 'failed',
            }
        except subprocess.TimeoutExpired as exc:
            stdout, stdout_truncated = _bounded_output(_timeout_text(exc.stdout))
            stderr, stderr_truncated = _bounded_output(_timeout_text(exc.stderr))
            return {
                **command.to_record(),
                'exit_code': None,
                'stdout': stdout,
                'stderr': stderr,
                'stdout_truncated': stdout_truncated,
                'stderr_truncated': stderr_truncated,
                'timed_out': True,
                'result': 'failed',
            }
        except OSError as exc:
            stderr, stderr_truncated = _bounded_output(f'{type(exc).__name__}: {exc}')
            return {
                **command.to_record(),
                'exit_code': None,
                'stdout': '',
                'stderr': stderr,
                'stdout_truncated': False,
                'stderr_truncated': stderr_truncated,
                'timed_out': False,
                'result': 'failed',
            }

    def output(
        self,
        cwd: Path,
        args: list[str],
        *,
        env: dict[str, str] | None = None,
    ) -> str:
        result = self.run(cwd, args, env=env)
        return result.stdout.strip()

    def run(
        self,
        cwd: Path,
        args: list[str],
        *,
        check: bool = True,
        env: dict[str, str] | None = None,
    ) -> GitCommandResult:
        command = ('git', '-C', str(Path(cwd)), *tuple(str(item) for item in args))
        command_env = os.environ.copy()
        command_env.update(env or {})
        completed = subprocess.run(
            list(command),
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=command_env,
        )
        result = GitCommandResult(
            argv=command,
            returncode=completed.returncode,
            stdout=completed.stdout or '',
            stderr=completed.stderr or '',
        )
        if check and result.returncode != 0:
            detail = (result.stderr or result.stdout or 'git command failed').strip()
            raise RuntimeError(f'{" ".join(command)}: {detail}')
        return result

    def _nul_output(self, cwd: Path, args: list[str]) -> tuple[str, ...]:
        raw = self.run(cwd, args).stdout
        return tuple(item for item in raw.split('\0') if item)


def _timeout_text(value: str | bytes | None) -> str:
    if value is None:
        return ''
    if isinstance(value, bytes):
        return value.decode('utf-8', errors='replace')
    return value


def _bounded_output(value: str) -> tuple[str, bool]:
    encoded = value.encode('utf-8')
    if len(encoded) <= _MAX_VERIFICATION_OUTPUT_BYTES:
        return value, False
    return encoded[:_MAX_VERIFICATION_OUTPUT_BYTES].decode('utf-8', errors='replace'), True


__all__ = [
    'CONTROLLER_EMAIL',
    'CONTROLLER_NAME',
    'GitCommandResult',
    'GitOperations',
]
