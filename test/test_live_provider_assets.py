import json
import os
import shutil
from pathlib import Path
from types import SimpleNamespace

from provider_backends.pi.home import materialize_pi_config
from provider_profiles import codex_home_config as codex
from provider_backends.claude.launcher_runtime import home as claude
from provider_core.projected_assets import route_live_tree, remove_projected_path, write_projected_marker
from provider_core.inherited_skills import route_inherited_skill_entries
import pytest


def test_pi_resources_reference_native_paths_without_cache(tmp_path):
    source = tmp_path / 'source'
    agent = source / '.pi' / 'agent'
    plugin = agent / 'npm' / 'node_modules' / 'plugin'
    plugin.mkdir(parents=True)
    (plugin / 'package.json').write_text('{"name":"plugin"}')
    (plugin / 'index.js').write_text('original')
    extension = agent / 'extensions' / 'example.ts'
    extension.parent.mkdir()
    extension.write_text('extension')
    (agent / 'settings.json').write_text(json.dumps({'packages': ['npm:plugin']}))
    target = tmp_path / 'managed'
    cache = tmp_path / 'cache'
    materialize_pi_config(source, target, shared_cache_root=cache)
    settings = json.loads((target / 'settings.json').read_text())
    assert Path(settings['packages'][0]) == plugin
    assert str(extension) in settings['extensions']
    assert not cache.exists()
    (plugin / 'index.js').write_text('updated')
    assert (Path(settings['packages'][0]) / 'index.js').read_text() == 'updated'


def test_codex_plugins_reference_native_tree_without_bundle(tmp_path):
    source = tmp_path / 'source'
    plugin = source / '.tmp' / 'plugins'
    plugin.mkdir(parents=True)
    (plugin / 'example').write_text('original')
    target = tmp_path / 'managed'
    cache = tmp_path / 'cache'
    codex._sync_codex_plugin_projection(source, target, enabled=True,
                                        project_root=tmp_path, shared_cache_root=cache)
    projected = target / '.tmp' / 'plugins'
    assert projected.resolve() == plugin.resolve()
    assert not cache.exists()
    (plugin / 'example').write_text('updated')
    assert (projected / 'example').read_text() == 'updated'


def test_claude_uses_native_plugin_manager_root(tmp_path):
    source = tmp_path / 'source'
    plugins = source / '.claude' / 'plugins'
    (plugins / 'cache').mkdir(parents=True)
    private = tmp_path / 'managed' / '.claude'
    env = claude._claude_plugin_environment(source, SimpleNamespace(claude_dir=private),
                                            profile=None, command_policy=None)
    assert Path(env['CLAUDE_CODE_PLUGIN_CACHE_DIR']) == plugins
    assert not (private / 'plugins').exists()


def test_live_link_migrates_only_owned_copy_and_cleanup_preserves_source(tmp_path):
    source = tmp_path / 'source with spaces'
    source.mkdir()
    (source / 'file').write_text('native')
    target = tmp_path / 'managed' / 'entry'
    shutil.copytree(source, target)
    assert write_projected_marker(target, source=source, mode='copy', label='owned')
    assert route_live_tree(source, target, label='owned')
    assert target.resolve() == source.resolve()
    remove_projected_path(target, label='owned')
    assert not target.exists()
    assert (source / 'file').read_text() == 'native'


def test_live_link_preserves_unmarked_target(tmp_path):
    source = tmp_path / 'source'
    source.mkdir()
    target = tmp_path / 'target'
    target.mkdir()
    (target / 'user-file').write_text('keep')
    assert not route_live_tree(source, target, label='owned')
    assert (target / 'user-file').read_text() == 'keep'


def test_live_skills_keep_control_overlay_separate(tmp_path):
    source = tmp_path / 'source'
    skill = source / 'example'
    skill.mkdir(parents=True)
    (skill / 'SKILL.md').write_text('original')
    target = tmp_path / 'managed'
    (target / 'ask').mkdir(parents=True)
    (target / 'ask' / 'SKILL.md').write_text('CCB-owned control')
    assert route_inherited_skill_entries(source, target, enabled=True, label='skills',
                                         exclude=('ask',), live=True) == ('example',)
    (skill / 'SKILL.md').write_text('updated')
    assert (target / 'example' / 'SKILL.md').read_text() == 'updated'
    assert (target / 'ask' / 'SKILL.md').read_text() == 'CCB-owned control'


@pytest.mark.skipif(os.name != 'nt', reason='Windows junction fallback')
def test_windows_live_link_needs_no_symlink_privilege(tmp_path, monkeypatch):
    source = tmp_path / 'source'
    source.mkdir()
    (source / 'file').write_text('native')
    target = tmp_path / 'managed' / 'entry'
    def denied(*args, **kwargs):
        raise OSError('symlink privilege unavailable')
    monkeypatch.setattr(Path, 'symlink_to', denied)
    assert route_live_tree(source, target)
    assert target.resolve() == source.resolve()
    remove_projected_path(target)
    assert (source / 'file').read_text() == 'native'
