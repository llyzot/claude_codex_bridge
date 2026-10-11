from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import zipfile

import pytest


def _load_module():
    script_path = (
        Path(__file__).resolve().parents[1]
        / "platforms"
        / "windows"
        / "packaging"
        / "build_release.py"
    )
    spec = importlib.util.spec_from_file_location("build_windows_release", script_path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_windows_release_identity_is_isolated_x64_zip() -> None:
    module = _load_module()

    assert module.ARTIFACT_NAME == "ccb-windows-x86_64.zip"
    assert module.normalize_arch("AMD64") == "x86_64"
    assert module.normalize_arch("x64") == "x86_64"
    assert module.WINDOWS_PLATFORM_DIR.as_posix() == "platforms/windows"
    assert module.WINDOWS_RUNTIME_DIR.as_posix() == "lib/platforms/windows"


def test_default_payload_allowlist_only_names_tracked_source_directories() -> None:
    module = _load_module()
    repo_root = Path(__file__).resolve().parents[1]

    assert "commands" not in module.PAYLOAD_DIRS
    missing = [name for name in module.PAYLOAD_DIRS if not (repo_root / name).is_dir()]
    assert missing == []


def test_builder_rejects_non_windows_or_non_x64_host(monkeypatch) -> None:
    module = _load_module()
    monkeypatch.setattr(module.platform, "system", lambda: "Linux")
    monkeypatch.setattr(module.platform, "machine", lambda: "x86_64")

    with pytest.raises(RuntimeError, match="native Windows x64"):
        module.require_windows_x64_host()


def test_copy_payload_uses_allowlist_and_keeps_unix_builders_out(monkeypatch, tmp_path: Path) -> None:
    module = _load_module()
    export_root = tmp_path / "export"
    artifact_root = tmp_path / "artifact"
    (export_root / "shared").mkdir(parents=True)
    (export_root / "shared" / "runtime.py").write_text("shared\n", encoding="utf-8")
    (export_root / "VERSION").write_text("8.6.6\n", encoding="utf-8")
    windows_root = export_root / "platforms" / "windows"
    for name in ("docs", "installer", "tools"):
        (windows_root / name).mkdir(parents=True)
        (windows_root / name / "marker.txt").write_text(name, encoding="utf-8")
    (windows_root / 'ccb.py').write_text('native entry\n', encoding='utf-8')
    (windows_root / 'start.ps1').write_text('native wrapper\n', encoding='utf-8')
    (export_root / "install.sh").write_text("unix\n", encoding="utf-8")
    (export_root / "scripts").mkdir()
    (export_root / "scripts" / "build_linux_release.py").write_text("unix\n", encoding="utf-8")

    monkeypatch.setattr(module, "PAYLOAD_FILES", ("VERSION",))
    monkeypatch.setattr(module, "PAYLOAD_DIRS", ("shared",))
    module.copy_payload(export_root, artifact_root)

    assert (artifact_root / "VERSION").is_file()
    assert (artifact_root / "shared" / "runtime.py").is_file()
    assert (artifact_root / "platforms/windows/installer/marker.txt").is_file()
    assert (artifact_root / 'platforms/windows/ccb.py').read_text(encoding='utf-8') == 'native entry\n'
    assert (artifact_root / 'platforms/windows/start.ps1').is_file()
    assert (artifact_root / 'platforms/windows/tools/marker.txt').is_file()
    assert not (artifact_root / "install.sh").exists()
    assert not (artifact_root / "scripts").exists()


def test_native_launcher_is_projected_to_all_public_windows_commands(monkeypatch, tmp_path: Path) -> None:
    module = _load_module()
    artifact_root = tmp_path / "artifact"
    build_root = tmp_path / "build"
    source_root = tmp_path / "exported-ref"
    calls = []

    def fake_cargo_build(_manifest: Path, target_dir: Path, binary_name: str) -> Path:
        assert _manifest.is_relative_to(source_root)
        calls.append((_manifest,target_dir,binary_name))
        binary = target_dir / "release" / f"{binary_name}.exe"
        binary.parent.mkdir(parents=True, exist_ok=True)
        binary.write_bytes(b"MZfake")
        return binary

    monkeypatch.setattr(module, "_cargo_build", fake_cargo_build)
    entries = module.build_native_binaries(artifact_root, build_root, source_root)

    for name in ("ccb", "ask", "autonew", "ctx-transfer"):
        assert entries[name] == f"bin/{name}.exe"
        assert (artifact_root / entries[name]).read_bytes().startswith(b"MZ")
    assert entries["ccb-agent-sidebar"] == "bin/ccb-agent-sidebar.exe"
    assert entries["ccb-rs-helper"] == "bin/ccb-rs-helper.exe"
    assert entries['ccb-native']=='bin/ccb-native.exe'
    assert len({target for _,target,name in calls if name in {'ccb-native','ccb-kernel'}})==1


def test_metadata_archive_and_checksum_are_self_consistent(tmp_path: Path) -> None:
    module = _load_module()
    stage_root = tmp_path / "stage"
    artifact_root = stage_root / module.ARTIFACT_BASENAME
    bin_dir = artifact_root / "bin"
    bin_dir.mkdir(parents=True)
    (artifact_root / "VERSION").write_text("8.6.6\n", encoding="utf-8")
    (artifact_root / "install.ps1").write_text("Write-Host install\n", encoding="utf-8")
    for relative in ('platforms/windows/ccb.py', 'platforms/windows/start.ps1',
                     'lib/platforms/windows/herdr/entrypoint.py',
                     'lib/platforms/windows/herdr/cli_entrypoint.py'):
        path = artifact_root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('test entry\n', encoding='utf-8')
    entries = {
        "ccb": "bin/ccb.exe",
        "ask": "bin/ask.exe",
        "autonew": "bin/autonew.exe",
        "ctx-transfer": "bin/ctx-transfer.exe",
        "ccb-native": "bin/ccb-native.exe",
        "ccb-kernel": "bin/ccb-kernel.exe",
    }
    for relative in entries.values():
        (artifact_root / relative).write_bytes(b"MZfake")

    module.write_metadata(
        artifact_root,
        version="8.6.6",
        commit="a" * 40,
        commit_date="2026-08-11",
        channel="beta",
        bin_entries=entries,
        native_installation=True,
    )
    archive_path = tmp_path / module.ARTIFACT_NAME
    checksum_path = tmp_path / module.CHECKSUM_NAME
    module.create_zip(stage_root, artifact_root, archive_path)
    digest = module.write_checksum(archive_path, checksum_path)
    module.verify_archive(archive_path, version="8.6.6")

    assert checksum_path.read_text(encoding="utf-8") == f"{digest}  {archive_path.name}\n"
    with zipfile.ZipFile(archive_path) as archive:
        manifest = json.loads(archive.read(f"{module.ARTIFACT_BASENAME}/WINDOWS_MANIFEST.json"))
    assert manifest["support_tier"] == "beta"
    assert manifest["executable_entry"] == "bin/ccb.exe"
    assert manifest["prerequisites"]["python"] == ">=3.10"
    assert manifest['default_runtime']=='python'
    assert not manifest['runtime_options']['rust']['python_required']
    assert not manifest['runtime_options']['rust']['herdr_integrated']


def test_windows_release_workflow_handles_stable_and_beta_tags() -> None:
    text = Path(".github/workflows/release-windows.yml").read_text(encoding="utf-8")
    version = Path("VERSION").read_text(encoding="utf-8").strip()

    assert "name: Native Windows Release" in text
    assert f'default: "v{version}"' in text
    assert '- "v*.*.*"' in text
    assert '- "v*-beta.*"' in text
    assert "release_flags=(--latest)" in text
    assert 'if [[ "$TAG_NAME" == *-* ]]; then' in text
    assert "release_flags=(--prerelease)" in text
    assert text.count("--prerelease") == 1
    assert '--title "$title"' in text


def test_cargo_uses_exported_manifest_directory_and_explicit_binary(monkeypatch, tmp_path):
    module=_load_module()
    manifest=tmp_path/'export/rust/crates/ccb-kernel/Cargo.toml'
    manifest.parent.mkdir(parents=True);manifest.write_text('fixture')
    target=tmp_path/'target'
    calls=[]
    def run(argv,**kwargs):
        calls.append((argv,kwargs))
        binary=target/'release/ccb-native.exe';binary.parent.mkdir(parents=True);binary.write_bytes(b'MZfixture')
    monkeypatch.setattr(module.subprocess,'run',run)
    binary=module._cargo_build(manifest,target,'ccb-native')
    assert binary.read_bytes()==b'MZfixture'
    argv,options=calls[0]
    assert options['cwd']==manifest.parent
    assert argv[argv.index('--manifest-path')+1]==str(manifest)
    assert argv[argv.index('--bin')+1]=='ccb-native' and '--locked' in argv


def test_build_exports_the_resolved_commit_and_compiles_that_export(monkeypatch,tmp_path):
    from types import SimpleNamespace
    module=_load_module();resolved='a'*40;seen={}
    monkeypatch.setattr(module,'require_windows_x64_host',lambda:None)
    monkeypatch.setattr(module,'ensure_clean_worktree',lambda:None)
    monkeypatch.setattr(module,'release_identity',lambda ref:('8.7.10',resolved,'2026-10-11'))
    def export(ref,path):
        assert ref==resolved
        seen['export']=path;path.mkdir()
        (path/'identity').write_text(ref)
    def binaries(artifact,build,source):
        assert source==seen['export'] and (source/'identity').read_text()==resolved
        seen['compiled']=True
        return {}
    monkeypatch.setattr(module,'export_git_ref',export)
    monkeypatch.setattr(module,'copy_payload',lambda source,target:target.mkdir(parents=True))
    monkeypatch.setattr(module,'build_native_binaries',binaries)
    monkeypatch.setattr(module,'probe_native_installation',lambda path,version:True)
    monkeypatch.setattr(module,'write_metadata',lambda *a,**k:None)
    monkeypatch.setattr(module,'create_zip',lambda stage,artifact,path:path.write_bytes(b'fixture'))
    monkeypatch.setattr(module,'verify_archive',lambda *a,**k:None)
    module.build(SimpleNamespace(allow_dirty=False,git_ref='moving-branch',output_dir=tmp_path/'out',channel='beta'))
    assert seen['compiled']


@pytest.mark.parametrize('mode',['new','old','wrong-version','malformed'])
def test_release_metadata_native_lane_requires_binary_capability_and_matching_version(monkeypatch,tmp_path,mode):
    from types import SimpleNamespace
    module=_load_module()
    payload=dict(schema_version=1,service='ccb-native',version='8.7.10',native_prefix_installation=dict(schema_version=1,supported=True))
    if mode=='wrong-version':payload['version']='different-release'
    def probe(argv,**kwargs):
        assert argv[-1]=='--capabilities' and kwargs['cwd']==tmp_path
        return SimpleNamespace(returncode=1 if mode=='old' else 0,stdout='broken' if mode=='malformed' else json.dumps(payload))
    monkeypatch.setattr(module.subprocess,'run',probe)
    if mode in {'wrong-version','malformed'}:
        with pytest.raises(RuntimeError):module.probe_native_installation(tmp_path,'8.7.10')
    else:assert module.probe_native_installation(tmp_path,'8.7.10')==(mode=='new')
