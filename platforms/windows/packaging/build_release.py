#!/usr/bin/env python3
"""Build the isolated native Windows x64 CCB beta artifact."""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
from datetime import datetime, timezone
import zipfile


REPO_ROOT = Path(__file__).resolve().parents[3]
ARTIFACT_BASENAME = "ccb-windows-x86_64"
ARTIFACT_NAME = f"{ARTIFACT_BASENAME}.zip"
CHECKSUM_NAME = f"{ARTIFACT_NAME}.sha256"
WINDOWS_PLATFORM_DIR = Path("platforms/windows")
WINDOWS_RUNTIME_DIR = Path("lib/platforms/windows")

PAYLOAD_DIRS = (
    "assets",
    "bin",
    "config",
    "inherit_skills",
    "lib",
    "mcp",
    "useful_tools",
)
PAYLOAD_FILES = (
    "LICENSE",
    "README.md",
    "VERSION",
    "ccb.py",
    "install.cmd",
    "install.ps1",
    "package.json",
)
WINDOWS_PAYLOAD_DIRS = (
    "docs",
    "installer",
    "tools",
)
WINDOWS_PAYLOAD_FILES = ("ccb.py", "start.ps1")
LAUNCHER_NAMES = ("ccb", "ask", "autonew", "ctx-transfer")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build the native Windows x64 CCB beta release")
    parser.add_argument("--output-dir", type=Path, default=REPO_ROOT / "dist" / "windows" / "x86_64")
    parser.add_argument("--git-ref", default="HEAD")
    parser.add_argument("--allow-dirty", action="store_true")
    parser.add_argument("--channel", default="beta")
    return parser.parse_args(argv)


def normalize_arch(value: str) -> str:
    text = str(value or "").strip().lower()
    return {"amd64": "x86_64", "x64": "x86_64", "x86_64": "x86_64"}.get(text, text)


def require_windows_x64_host() -> None:
    if platform.system() != "Windows" or normalize_arch(platform.machine()) != "x86_64":
        raise RuntimeError(
            "the Windows release must be built on a native Windows x64 runner; "
            f"got {platform.system()}/{platform.machine()}"
        )


def run_git(*args: str) -> str:
    completed = subprocess.run(
        ["git", "-C", str(REPO_ROOT), *args],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    return completed.stdout.strip()


def release_identity(git_ref: str) -> tuple[str, str, str]:
    version = run_git("show", f"{git_ref}:VERSION").strip()
    package = json.loads(run_git("show", f"{git_ref}:package.json"))
    package_version = str(package.get("version") or "").strip()
    if not version or package_version != version:
        raise RuntimeError(
            f"release identity mismatch: VERSION={version or 'missing'} package.json={package_version or 'missing'}"
        )
    commit = run_git("rev-parse", f"{git_ref}^{{commit}}")
    commit_date = run_git("show", "-s", "--format=%cs", commit)
    return version, commit, commit_date


def ensure_clean_worktree() -> None:
    dirty = run_git("status", "--porcelain", "--untracked-files=all")
    ignored_prefixes = (
        "?? dist/",
        "?? platforms/windows/launcher/target/",
    )
    entries = [line for line in dirty.splitlines() if line and not line.startswith(ignored_prefixes)]
    if entries:
        raise RuntimeError("Windows release build requires a clean worktree: " + "; ".join(entries[:10]))


def export_git_ref(git_ref: str, destination: Path) -> None:
    completed = subprocess.run(
        ["git", "-C", str(REPO_ROOT), "archive", "--format=tar", git_ref],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    destination.mkdir(parents=True, exist_ok=True)
    with tarfile.open(fileobj=io.BytesIO(completed.stdout), mode="r:") as archive:
        archive.extractall(destination)


def copy_payload(export_root: Path, artifact_root: Path) -> None:
    artifact_root.mkdir(parents=True, exist_ok=True)
    for name in PAYLOAD_FILES:
        source = export_root / name
        if not source.is_file():
            raise RuntimeError(f"Windows payload file is missing: {name}")
        shutil.copy2(source, artifact_root / name)
    for name in PAYLOAD_DIRS:
        source = export_root / name
        if not source.is_dir():
            raise RuntimeError(f"Windows payload directory is missing: {name}")
        shutil.copytree(source, artifact_root / name)

    windows_source = export_root / WINDOWS_PLATFORM_DIR
    windows_target = artifact_root / WINDOWS_PLATFORM_DIR
    windows_target.mkdir(parents=True, exist_ok=True)
    for name in WINDOWS_PAYLOAD_FILES:
        shutil.copy2(windows_source / name, windows_target / name)
    for name in WINDOWS_PAYLOAD_DIRS:
        source = windows_source / name
        if not source.is_dir():
            raise RuntimeError(f"Windows platform payload directory is missing: {source}")
        shutil.copytree(source, windows_target / name)


def _cargo_build(manifest: Path, target_dir: Path, binary_name: str) -> Path:
    subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            "--release",
            "--bin",
            binary_name,
            "--manifest-path",
            str(manifest),
            "--target-dir",
            str(target_dir),
        ],
        cwd=manifest.parent,
        check=True,
    )
    binary = target_dir / "release" / f"{binary_name}.exe"
    if not binary.is_file():
        raise RuntimeError(f"cargo did not produce expected Windows binary: {binary}")
    if not binary.read_bytes().startswith(b"MZ"):
        raise RuntimeError(f"Windows binary does not contain a PE MZ header: {binary}")
    return binary


def build_native_binaries(artifact_root: Path, build_root: Path, source_root: Path) -> dict[str, str]:
    bin_dir = artifact_root / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)

    launcher = _cargo_build(
        source_root / "platforms" / "windows" / "launcher" / "Cargo.toml",
        build_root / "launcher",
        "ccb-windows-launcher",
    )
    entries: dict[str, str] = {}
    for name in LAUNCHER_NAMES:
        destination = bin_dir / f"{name}.exe"
        shutil.copy2(launcher, destination)
        entries[name] = destination.relative_to(artifact_root).as_posix()

    helpers = (
        (source_root / "rust" / "crates" / "ccb-kernel" / "Cargo.toml", "ccb-kernel"),
        (source_root / "rust" / "crates" / "ccb-kernel" / "Cargo.toml", "ccb-native"),
        (source_root / "tools" / "ccb-agent-sidebar" / "Cargo.toml", "ccb-agent-sidebar"),
        (source_root / "tools" / "ccb-rs-helper" / "Cargo.toml", "ccb-rs-helper"),
    )
    for manifest, binary_name in helpers:
        target_dir = build_root / ("core" if binary_name in {"ccb-kernel", "ccb-native"} else binary_name)
        binary = _cargo_build(manifest, target_dir, binary_name)
        destination = bin_dir / f"{binary_name}.exe"
        shutil.copy2(binary, destination)
        entries[binary_name] = destination.relative_to(artifact_root).as_posix()
    return entries


def write_metadata(
    artifact_root: Path,
    *,
    version: str,
    commit: str,
    commit_date: str,
    channel: str,
    bin_entries: dict[str, str],
    native_installation: bool = False,
) -> None:
    build_info = {
        "version": version,
        "commit": commit,
        "date": commit_date,
        "build_time": datetime.now(timezone.utc).replace(microsecond=0).isoformat(),
        "platform": "windows",
        "arch": "x86_64",
        "channel": channel,
        "source_kind": "release",
        "install_mode": "release",
    }
    manifest = {
        "schema_version": 1,
        "version": version,
        "commit": commit,
        "platform": "windows",
        "arch": "x86_64",
        "support_tier": "beta",
        "artifact": ARTIFACT_NAME,
        "installer_entry": "install.ps1",
        "executable_entry": "bin/ccb.exe",
        "bin_entries": bin_entries,
        "prerequisites": {
            "python": ">=3.10",
            "wezterm": "required",
            "git_bash": "required",
            "herdr": ">=0.8.0",
        },
        "default_runtime": "python",
        "runtime_options": {
            "rust": {
                "installer_args": ["install", "-Runtime", "rust", "-InstallPrefix", "<new-directory>"],
                "core_entry": "bin/ccb-native.exe",
                "installed_commands": ["ccb.exe", "ask.exe", "ccb-native.exe"],
                "python_required": False,
                "terminal_required": False,
                "provider_prerequisites": "Provider-specific runtimes and credentials remain required; Git features require Git.",
                "support_tier": "experimental",
                "herdr_integrated": False,
                "updates_account_settings": False,
            },
        } if native_installation else {},
    }
    (artifact_root / "BUILD_INFO.json").write_text(
        json.dumps(build_info, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    (artifact_root / "WINDOWS_MANIFEST.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def probe_native_installation(artifact_root: Path, version: str) -> bool:
    completed = subprocess.run(
        [str(artifact_root / 'bin/ccb-native.exe'), '--capabilities'],
        cwd=artifact_root, capture_output=True, text=True, encoding='utf-8',
        timeout=10, creationflags=0x08000000 if os.name == 'nt' else 0,
    )
    if completed.returncode:
        # Older exported refs have a core CLI but not the installation handshake.
        return False
    try:
        capabilities = json.loads(completed.stdout)
    except ValueError as error:
        raise RuntimeError('native capability probe returned invalid JSON') from error
    if capabilities.get('schema_version') != 1 or capabilities.get('service') != 'ccb-native':
        raise RuntimeError('native capability probe returned an unknown contract')
    if capabilities.get('version') != version:
        raise RuntimeError('native binary version does not match the exported release')
    installation = capabilities.get('native_prefix_installation') or {}
    return installation.get('schema_version') == 1 and installation.get('supported') is True


def create_zip(stage_root: Path, artifact_root: Path, artifact_path: Path) -> None:
    with zipfile.ZipFile(artifact_path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for path in sorted(artifact_root.rglob("*")):
            if path.is_file():
                archive.write(path, path.relative_to(stage_root).as_posix())


def write_checksum(artifact_path: Path, checksum_path: Path) -> str:
    digest = hashlib.sha256(artifact_path.read_bytes()).hexdigest()
    checksum_path.write_text(f"{digest}  {artifact_path.name}\n", encoding="utf-8")
    return digest


def verify_archive(artifact_path: Path, *, version: str) -> None:
    required = {
        f"{ARTIFACT_BASENAME}/VERSION",
        f"{ARTIFACT_BASENAME}/BUILD_INFO.json",
        f"{ARTIFACT_BASENAME}/WINDOWS_MANIFEST.json",
        f"{ARTIFACT_BASENAME}/install.ps1",
        f"{ARTIFACT_BASENAME}/bin/ccb.exe",
        f"{ARTIFACT_BASENAME}/bin/ask.exe",
        f"{ARTIFACT_BASENAME}/bin/ccb-native.exe",
        f"{ARTIFACT_BASENAME}/bin/ccb-kernel.exe",
        f"{ARTIFACT_BASENAME}/platforms/windows/ccb.py",
        f"{ARTIFACT_BASENAME}/platforms/windows/start.ps1",
        f"{ARTIFACT_BASENAME}/lib/platforms/windows/herdr/entrypoint.py",
        f"{ARTIFACT_BASENAME}/lib/platforms/windows/herdr/cli_entrypoint.py",
    }
    with zipfile.ZipFile(artifact_path) as archive:
        names = set(archive.namelist())
        missing = sorted(required - names)
        if missing:
            raise RuntimeError("Windows archive is missing required entries: " + ", ".join(missing))
        if archive.read(f"{ARTIFACT_BASENAME}/VERSION").decode("utf-8").strip() != version:
            raise RuntimeError("Windows archive VERSION does not match release identity")
        for name in ("ccb.exe", "ask.exe", "ccb-native.exe", "ccb-kernel.exe"):
            if not archive.read(f"{ARTIFACT_BASENAME}/bin/{name}").startswith(b"MZ"):
                raise RuntimeError(f"Windows archive entry is not a PE executable: {name}")


def build(args: argparse.Namespace) -> tuple[Path, Path]:
    require_windows_x64_host()
    if not args.allow_dirty:
        ensure_clean_worktree()
    version, commit, commit_date = release_identity(args.git_ref)
    output_dir = args.output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    artifact_path = output_dir / ARTIFACT_NAME
    checksum_path = output_dir / CHECKSUM_NAME

    with tempfile.TemporaryDirectory(prefix="ccb-windows-release-") as temporary:
        temporary_root = Path(temporary)
        export_root = temporary_root / "export"
        stage_root = temporary_root / "stage"
        artifact_root = stage_root / ARTIFACT_BASENAME
        export_git_ref(commit, export_root)
        copy_payload(export_root, artifact_root)
        bin_entries = build_native_binaries(artifact_root, temporary_root / "cargo", export_root)
        native_installation = probe_native_installation(artifact_root, version)
        write_metadata(
            artifact_root,
            version=version,
            commit=commit,
            commit_date=commit_date,
            channel=args.channel,
            bin_entries=bin_entries,
            native_installation=native_installation,
        )
        create_zip(stage_root, artifact_root, artifact_path)

    write_checksum(artifact_path, checksum_path)
    verify_archive(artifact_path, version=version)
    return artifact_path, checksum_path


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    artifact_path, checksum_path = build(args)
    print(f"artifact: {artifact_path}")
    print(f"sha256: {checksum_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
