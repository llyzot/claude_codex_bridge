"""Unfinished work survives a retired namespace without replaying sent tasks."""
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest
from test_native_herdr_namespace import setup, mutate_count, fault
from test_native_herdr_agents import change
from test_native_herdr_open import project, cli, NODE, owned_stop
from test_native_herdr_reopen import read, retire
from test_native_herdr_recovery import stopped
from test_native_herdr_worker import wait_for
from test_native_pi_terminal import HOST

pytestmark = pytest.mark.skipif(os.name != 'nt', reason='Windows owner lifetime recovery')


def job(p, agent, identity):
    rows = [json.loads(line) for line in (p['root']/f'.ccb/agents/{agent}/jobs.jsonl').read_text(encoding='utf-8').splitlines()]
    return next(row for row in reversed(rows) if row['job_id'] == identity)


def finish_worker(p, prompt, reply, turn):
    with p['log'].open('a', encoding='utf-8') as out:
        for payload in [dict(type='task_started', turn_id=turn), dict(type='user_message', message=prompt),
                        dict(type='task_complete', turn_id=turn, last_agent_message=reply)]:
            out.write(json.dumps(dict(type='event_msg', payload=payload))+'\n')


@pytest.mark.parametrize('durable_result,crash', [(True,False),(False,False),(True,True)], ids=['completed','interrupted','opener_crash'])
def test_reopen_delivers_child_once_and_preserves_unsent_fifo(project, durable_result, crash):
    p = project
    original = json.loads(p['pi'].args[2])
    history = Path(original['native_session_path'])
    history.write_text(json.dumps({'type':'session', 'id':original['session_id'], 'cwd':str(p['root'])})+'\n', encoding='utf-8')
    assert p['open']().returncode == 0
    parent = cli('--state-root', p['native'], 'ask', 'lead', 'delegate before retirement')
    assert parent.returncode == 0, parent.stderr
    parent_id = json.loads(parent.stdout)['job_id']
    first = wait_for(lambda:p['prompts']())[-1]
    p['command'](action='prompt', prompt=first['body'], delegate=True)
    p['idle']('lead')
    worker = wait_for(lambda:p['prompts']() if len(p['prompts']()) >= 2 else None)[1]
    edges = [json.loads(s) for s in (p['native']/'callbacks/edges.jsonl').read_text(encoding='utf-8').splitlines()]
    child_id = edges[-1]['child_job_id']
    queued = []
    for text in ['fifo A', 'fifo B']:
        result = cli('--state-root', p['native'], 'ask', 'worker', text)
        assert result.returncode == 0, result.stderr
        queued.append(json.loads(result.stdout)['job_id'])
    retired = cli('--state-root', p['native'], 'kill', timeout=25)
    assert retired.returncode == 0, retired.stderr
    for proc in [p['pi'], p['codex']]:
        proc.kill(); proc.wait(timeout=5)
    if durable_result:
        finish_worker(p, worker['body'], 'durable old worker result', 'old-turn')
    gate = p['root']/'resume-ready'
    original['start_gate'] = str(gate)
    pi = subprocess.Popen([NODE, str(HOST), json.dumps(original)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, text=True, encoding='utf-8', creationflags=0x08000000)
    codex = subprocess.Popen([sys.executable, '-I', '-c', 'import time;time.sleep(180)'], creationflags=0x08000000)
    try:
        change(p['backend'], lambda s:s.update(provider_pids=[pi.pid, codex.pid]))
        if crash:fault(p['backend'],['agent','start'],'hang-after')
        assert p['open'](0).returncode == 0
        wait_for(lambda:(p['native']/'herdr/herdr-recovery.json').exists() and
                 read(p['native']/'herdr/herdr-recovery.json')['phase'] == 'reset', timeout=20)
        if crash:
            wait_for(lambda:(p['backend']/'mutation-finished').exists())
            identity=read(p['native']/'herdr/herdr-open-start.json')['process']
            owned_stop(identity)
            wait_for(lambda:cli('process-identity',identity['pid']).returncode!=0)
        gate.touch()
        assert json.loads(pi.stdout.readline())['ready']
        result = p['open']()
        assert result.returncode == 0, result.stderr
        wait_for(p['ready'], timeout=20)
        expected_status = 'completed' if durable_result else 'incomplete'
        assert job(p, 'worker', child_id)['status'] == expected_status
        prompts = wait_for(lambda:p['prompts']() if len(p['prompts']()) >= 4 else None)
        latest_edge = json.loads((p['native']/'callbacks/edges.jsonl').read_text(encoding='utf-8').splitlines()[-1])
        continuation = next(row for row in prompts[2:] if row['body'].startswith('CCB_REQ_ID: '+latest_edge['continuation_job_id']+':'))
        assert job(p, 'worker', child_id)['terminal_decision']['diagnostics']['recovery_no_replay'] is True
        assert ('durable old worker result' if durable_result else 'without a confirmed terminal result') in continuation['body']
        pi.stdin.write(json.dumps(dict(action='prompt', prompt=continuation['body'], reply='review after recovery'))+'\n')
        pi.stdin.flush()
        assert json.loads(pi.stdout.readline())['ok']
        p['idle']('lead')
        final = cli('--state-root', p['native'], 'watch', parent_id, '--wait', '--timeout-ms', '5000')
        assert final.returncode == 0 and json.loads(final.stdout)['reply'] == 'review after recovery', final.stderr
        # Worker queued jobs resume in original mailbox order, each only once.
        a = next(row for row in p['prompts']()[2:] if 'fifo A' in row['body'])
        assert not any('fifo B' in row['body'] for row in p['prompts']()[2:])
        finish_worker(p, a['body'], 'A result', 'new-A'); p['idle']('worker')
        b = wait_for(lambda:next((row for row in p['prompts']()[2:] if 'fifo B' in row['body']), None))
        finish_worker(p, b['body'], 'B result', 'new-B'); p['idle']('worker')
        wait_for(lambda:job(p, 'worker', queued[-1])['status'] == 'completed')
        assert sum(row['body'] == worker['body'] for row in p['prompts']()) == 1
        assert sum('fifo A' in row['body'] for row in p['prompts']()) == 1
        assert sum('fifo B' in row['body'] for row in p['prompts']()) == 1
        assert mutate_count(p['backend'], ['workspace', 'close']) == 1
        assert mutate_count(p['backend'], ['agent', 'start']) == 4
        assert p['open']().returncode == 0
    finally:
        for proc in [pi, codex]:
            if proc.poll() is None:proc.kill()
            proc.wait(timeout=5)


@pytest.mark.parametrize('invalid', ['model','environment','permit','cancel_intent','lease_binding'])
def test_recovery_refuses_changed_owner_configuration_or_corrupt_execution(stopped, invalid):
    p=stopped
    result=cli('--state-root',p['runtime'],'ask','worker','uncertain old side effect')
    assert result.returncode==0,result.stderr
    identity=json.loads(result.stdout)['job_id']
    wait_for(lambda:mutate_count(p['backend'],['agent','prompt'])==1)
    retire(p)
    if invalid in ['model','environment']:
        path=p['project']/'open.json';options=read(path)
        spec=options['config']['agents']['worker']
        if invalid=='model':spec['model']='different-model'
        else:spec.setdefault('env',{})['CODEX_HOME']=str(p['project']/'different-home')
        path.write_text(json.dumps(options),encoding='utf-8')
    elif invalid=='lease_binding':
        path=p['runtime']/'lease.json';lease=read(path)
        lease['config_signature']='0'*64
        path.write_text(json.dumps(lease),encoding='utf-8')
    else:
        path=p['project']/'.ccb/agents/worker/jobs.jsonl'
        row=json.loads(path.read_text(encoding='utf-8').splitlines()[-1])
        assert row['job_id']==identity
        if invalid=='permit':row['provider_options']['herdr_dispatch']['identity']['namespace_owner']='foreign'
        else:row['provider_options']['native_cancel_intent']={'record_reply':'invalid'}
        with path.open('a',encoding='utf-8') as out:out.write(json.dumps(row)+'\n')
    replacement=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[replacement.pid]))
    rejected=p['open']()
    assert rejected.returncode!=0
    assert mutate_count(p['backend'],['agent','prompt'])==1
    assert mutate_count(p['backend'],['agent','start'])==1
    assert json.loads((p['project']/'.ccb/agents/worker/jobs.jsonl').read_text(encoding='utf-8').splitlines()[-1])['status']=='running'


def test_recovery_keeps_durable_silent_cancellation_without_a_reply(stopped):
    p=stopped;result=cli('--state-root',p['runtime'],'ask','worker','old cancelled work')
    assert result.returncode==0,result.stderr
    identity=json.loads(result.stdout)['job_id']
    wait_for(lambda:mutate_count(p['backend'],['agent','prompt'])==1)
    retire(p)
    path=p['project']/'.ccb/agents/worker/jobs.jsonl'
    row=json.loads(path.read_text(encoding='utf-8').splitlines()[-1])
    # Model a committed cancellation intent whose owner died before finalization.
    row['provider_options']['native_cancel_intent']={'record_reply':False}
    with path.open('a',encoding='utf-8') as out:out.write(json.dumps(row)+'\n')
    replacement=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[replacement.pid]))
    reopened=p['open']();assert reopened.returncode==0,reopened.stderr
    watched=cli('--state-root',p['runtime'],'watch',identity)
    assert json.loads(watched.stdout)['status']=='cancelled'
    replies=p['runtime']/'replies/replies.jsonl'
    assert not replies.exists() or not replies.read_text(encoding='utf-8').strip()
    assert mutate_count(p['backend'],['agent','prompt'])==1
