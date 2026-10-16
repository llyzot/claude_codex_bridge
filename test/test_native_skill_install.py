"""CCB control skills project into provider skill roots without touching user-owned skills."""
import json
import os
from test_native_cli_security import BIN
import subprocess


def run(tmp_path, *args):
    env = dict(os.environ, PI_CODING_AGENT_DIR=str(tmp_path / 'pi'), CLAUDE_CONFIG_DIR=str(tmp_path / 'claude'),
               CODEX_HOME=str(tmp_path / 'codex'))
    result = subprocess.run([str(BIN), 'skills', *args], capture_output=True, text=True, encoding='utf-8',
                            env=env, stdin=subprocess.DEVNULL, timeout=30)
    assert result.returncode == 0, result.stderr
    return {(r['provider'], r['skill']): r for r in json.loads(result.stdout)['skills']}


def test_install_status_and_idempotence(tmp_path):
    before = run(tmp_path, 'status')
    assert set(before) == {(p, s) for p in ('pi', 'claude', 'codex') for s in ('ask', 'ccb-clear', 'ccb-compact')}
    assert {r['state'] for r in before.values()} == {'absent'}
    planned = run(tmp_path, 'install', '--dry-run')
    assert {r['action'] for r in planned.values()} == {'install'} and not (tmp_path / 'pi').exists()
    run(tmp_path, 'install')
    pi_ask = (tmp_path / 'pi/skills/ask/SKILL.md').read_text(encoding='utf-8')
    assert 'name: ask' in pi_ask and 'ccb_interactive_coordination' in pi_ask
    assert (tmp_path / 'codex/skills/ccb-clear/agents/openai.yaml').is_file()
    claude_ask = (tmp_path / 'claude/skills/ask/SKILL.md').read_text(encoding='utf-8')
    assert ('PowerShell equivalent' in claude_ask) == (os.name == 'nt')
    assert not (tmp_path / 'claude/skills/ask/SKILL.md.powershell').exists()
    again = run(tmp_path, 'install')
    assert {r['state'] for r in again.values()} == {'current'} and {r['action'] for r in again.values()} == {'none'}
    for root in ('pi', 'claude', 'codex'):
        assert sorted(p.name for p in (tmp_path / root / 'skills').iterdir()) == ['ask', 'ccb-clear', 'ccb-compact']


def test_user_edits_and_foreign_skills_are_preserved_unless_replaced(tmp_path):
    run(tmp_path, 'install', '--provider', 'pi')
    edited = tmp_path / 'pi/skills/ask/SKILL.md'
    edited.write_text('my own notes', encoding='utf-8')
    foreign = tmp_path / 'codex/skills/ask'
    foreign.mkdir(parents=True)
    (foreign / 'SKILL.md').write_text('foreign ask', encoding='utf-8')
    rows = run(tmp_path, 'install', '--provider', 'pi', '--provider', 'codex')
    assert rows[('pi', 'ask')]['action'] == 'skipped_modified' and edited.read_text(encoding='utf-8') == 'my own notes'
    assert rows[('codex', 'ask')]['action'] == 'skipped_unmanaged'
    assert (foreign / 'SKILL.md').read_text(encoding='utf-8') == 'foreign ask'
    rows = run(tmp_path, 'install', '--provider', 'codex', '--replace-unmanaged')
    assert rows[('codex', 'ask')]['action'] == 'backup_and_install'
    backups = list((tmp_path / 'codex/.ccb-skill-backups').iterdir())
    assert len(backups) == 1 and (backups[0] / 'SKILL.md').read_text(encoding='utf-8') == 'foreign ask'
    assert 'name: ask' in (foreign / 'SKILL.md').read_text(encoding='utf-8')
    assert not any(p.name.startswith('.ccb-skill-stage') for p in (tmp_path / 'codex').iterdir())
