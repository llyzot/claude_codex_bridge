"""Unfinished work survives a retired namespace without replaying sent tasks."""
import json
import os
import subprocess
import sys
import hashlib
import uuid
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


@pytest.mark.parametrize('durable_result,crash,deferred', [(True,False,False),(False,False,False),(True,True,False),(True,False,True),(False,False,True),(True,False,'stale_cache'),(True,'late_daemon',False)], ids=['completed','interrupted','opener_crash','deferred_completed','deferred_interrupted','deferred_stale_cache','late_lead_daemon_crash'])
def test_reopen_delivers_child_once_and_preserves_unsent_fifo(project, durable_result, crash, deferred):
    p = project
    original = json.loads(p['pi'].args[2])
    history = Path(original['native_session_path'])
    history.write_text(json.dumps({'type':'session', 'id':original['session_id'], 'cwd':str(p['root'])})+'\n', encoding='utf-8')
    if deferred:change(p['backend'],lambda s:s['agent_sessions'].pop('codex'))
    assert p['open']().returncode == 0
    wait_for(p['ready'],timeout=30)
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
    elif deferred:
        with p['log'].open('a',encoding='utf-8') as out:
            for payload in [dict(type='task_started',turn_id='old-turn'),dict(type='user_message',message=worker['body'])]:
                out.write(json.dumps(dict(type='event_msg',payload=payload))+'\n')
    if deferred=='stale_cache':
        previous=str(uuid.uuid4());old=p['log'].parent/f'rollout-{previous}.jsonl'
        old.write_text(json.dumps(dict(type='session_meta',payload=dict(id=previous,cwd=str(p['root']))))+'\n',encoding='utf-8')
        dispatched=job(p,'worker',child_id)['provider_options']['herdr_dispatch']
        cache=p['native']/'providers/codex/worker/herdr-history.json';cache.parent.mkdir(parents=True,exist_ok=True)
        cache.write_text(json.dumps(dict(schema_version=1,identity=dispatched['identity'],discovery=dispatched['discovery'],
            history=dict(root=str(old.parent),cwd=str(p['root']),path=str(old),session_id=previous))),encoding='utf-8')
    gate = p['root']/'resume-ready'
    original['start_gate'] = str(gate)
    pi = subprocess.Popen([NODE, str(HOST), json.dumps(original)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, text=True, encoding='utf-8', creationflags=0x08000000)
    codex = subprocess.Popen([sys.executable, '-I', '-c', 'import time;time.sleep(180)'], creationflags=0x08000000)
    try:
        change(p['backend'], lambda s:s.update(provider_pids=[pi.pid, codex.pid]))
        if deferred:change(p['backend'],lambda s:s['agent_sessions'].update(codex=dict(agent='codex',kind='id',value=p['session'],source='fixture')))
        if crash is True:fault(p['backend'],['agent','start'],'hang-after')
        assert p['open'](0).returncode == 0
        wait_for(lambda:(p['native']/'herdr/herdr-recovery.json').exists() and
                 read(p['native']/'herdr/herdr-recovery.json')['phase'] == 'reset', timeout=20)
        if crash is True:
            wait_for(lambda:(p['backend']/'mutation-finished').exists())
            # Crash after both independent launches took effect, while an ACK
            # is withheld. A merely persisted but unsent launch stays uncertain
            # and must not be replayed automatically (covered by agent tests).
            wait_for(lambda:mutate_count(p['backend'],['agent','start'])==4)
            identity=read(p['native']/'herdr/herdr-open-start.json')['process']
            owned_stop(identity)
            wait_for(lambda:cli('process-identity',identity['pid']).returncode!=0)
        if crash == 'late_daemon':
            # Lead is deliberately absent. The full roster and durable reply
            # route already exist, so Worker can finish unrelated FIFO work.
            wait_for(p['controller_ready'], timeout=20)
            wait_for(lambda:job(p,'worker',child_id)['status']=='completed')
            a=wait_for(lambda:next((r for r in p['prompts']()[2:] if 'fifo A' in r['body']),None),timeout=20)
            assert not any('fifo B' in r['body'] for r in p['prompts']()[2:])
            finish_worker(p,a['body'],'A result','new-A');p['idle']('worker')
            b=wait_for(lambda:next((r for r in p['prompts']()[2:] if 'fifo B' in r['body']),None))
            finish_worker(p,b['body'],'B result','new-B');p['idle']('worker')
            wait_for(lambda:job(p,'worker',queued[-1])['status']=='completed')
            actors=json.loads(cli('--state-root',p['native'],'agents').stdout)['agents']
            assert {r['agent_name'] for r in actors}=={'lead','worker'}
            assert next(r for r in actors if r['agent_name']=='lead')['activation']['session_verified'] is False
            edge=json.loads((p['native']/'callbacks/edges.jsonl').read_text().splitlines()[-1])
            continuation_id=edge['continuation_job_id']
            assert not any(r['body'].startswith('CCB_REQ_ID: '+continuation_id+':') for r in p['prompts']()[2:])
            # Kill only the fixture daemon. Its keeper restores the full roster,
            # and pending delivery must survive without rerunning the child.
            previous=read(p['native']/'lease.json')['process']
            owned_stop(previous)
            wait_for(lambda:(lambda v:v and v['pid']!=previous['pid'])(p['controller_ready']()),timeout=20)
            assert not any(r['body'].startswith('CCB_REQ_ID: '+continuation_id+':') for r in p['prompts']()[2:])
        gate.touch()
        assert json.loads(pi.stdout.readline())['ready']
        result = p['open']()
        assert result.returncode == 0, result.stderr
        wait_for(p['ready'], timeout=20)
        expected_status = 'completed' if durable_result else 'incomplete'
        assert job(p, 'worker', child_id)['status'] == expected_status
        latest_edge = json.loads((p['native']/'callbacks/edges.jsonl').read_text(encoding='utf-8').splitlines()[-1])
        continuation = wait_for(lambda:next((row for row in p['prompts']()[2:] if row['body'].startswith('CCB_REQ_ID: '+latest_edge['continuation_job_id']+':')),None),timeout=20)
        assert job(p, 'worker', child_id)['terminal_decision']['diagnostics']['recovery_no_replay'] is True
        assert ('durable old worker result' if durable_result else 'without a confirmed terminal result') in continuation['body']
        pi.stdin.write(json.dumps(dict(action='prompt', prompt=continuation['body'], reply='review after recovery'))+'\n')
        pi.stdin.flush()
        assert json.loads(pi.stdout.readline())['ok']
        p['idle']('lead')
        final = cli('--state-root', p['native'], 'watch', parent_id, '--wait', '--timeout-ms', '5000')
        assert final.returncode == 0 and json.loads(final.stdout)['reply'] == 'review after recovery', final.stderr
        # Worker queued jobs resume in original mailbox order, each only once.
        if crash != 'late_daemon':
            a = wait_for(lambda:next((row for row in p['prompts']()[2:] if 'fifo A' in row['body']),None))
            assert not any('fifo B' in row['body'] for row in p['prompts']()[2:])
            finish_worker(p, a['body'], 'A result', 'new-A'); p['idle']('worker')
            b = wait_for(lambda:next((row for row in p['prompts']()[2:] if 'fifo B' in row['body']), None))
            finish_worker(p, b['body'], 'B result', 'new-B'); p['idle']('worker')
            wait_for(lambda:job(p, 'worker', queued[-1])['status'] == 'completed')
        assert sum(r['body']==continuation['body'] for r in p['prompts']())==1
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


@pytest.mark.parametrize('stopped',['claude'],indirect=True)
@pytest.mark.parametrize('evidence',['recorded','absent','hook_continued','foreign_execution','rewritten_history'])
def test_stopped_claude_reuses_only_unchanged_durable_idle_evidence(stopped,evidence):
    p=stopped;submitted=cli('--state-root',p['runtime'],'ask','worker','recover Claude result')
    assert submitted.returncode==0,submitted.stderr
    identity=json.loads(submitted.stdout)['job_id']
    wait_for(lambda:mutate_count(p['backend'],['agent','prompt'])==1)
    prompt=json.loads((p['backend']/'prompts.jsonl').read_text(encoding='utf-8').splitlines()[-1])['body']
    retire(p)
    path=p['project']/'.ccb/agents/worker/jobs.jsonl'
    row=json.loads(path.read_text(encoding='utf-8').splitlines()[-1])
    permit=row['provider_options']['herdr_dispatch'];execution=row['provider_options']['native_execution_instance']
    user=str(uuid.uuid4());assistant=str(uuid.uuid4());reply='Claude settled before owner death'
    def append(records):
        with p['log'].open('a',encoding='utf-8') as out:
            for record in records:out.write(json.dumps(record)+'\n')
    scope=dict(sessionId=p['session'],cwd=str(p['project']),isSidechain=False)
    append([dict(scope,type='user',uuid=user,parentUuid='historical',message=dict(role='user',content=prompt)),
            dict(scope,type='assistant',uuid=assistant,parentUuid=user,message=dict(id='native-reply',role='assistant',content=[dict(type='text',text=reply)],stop_reason='end_turn'))])
    data=p['log'].read_bytes();offset=len(data)
    cursor=dict(schema_version=2,record_type='completion_cursor',source_kind='session_event_log',session_path=str(p['log']),
                identity=permit['baseline']['identity'],offset=offset,digest=hashlib.sha256(offset.to_bytes(8,'little')+data[-65536:]).hexdigest(),
                line_no=None,event_seq=None,opaque_cursor=None,updated_at=row['updated_at'])
    # Model the authority's committed observation immediately before its final
    # completion transaction. The ordinary dispatch tests verify actual emission.
    decision=dict(schema_version=2,record_type='completion_decision',terminal=True,status='completed',reason='claude_assistant_end_turn',
                  confidence='exact',anchor_seen=True,reply_started=True,reply_stable=True,reply=reply,provider_turn_ref=user,
                  source_cursor=cursor,finished_at=row['updated_at'],diagnostics=dict(completion_authority='herdr_idle_and_anchored_transcript',
                  session_id=p['session'],request_anchor=permit['request_anchor'],assistant_uuid=assistant,native_execution_instance=execution,
                  herdr_idle_observation=dict(schema_version=1,identity=permit['identity'],interactive_ready=True,
                  agent_session=dict(agent='claude',kind='id',value=p['session']),history_binding=permit['history_binding'],observed_at=row['updated_at'])))
    if evidence!='absent':
        row['provider_options']['herdr_terminal_observation']=dict(schema_version=1,record_type='herdr_terminal_observation',
            job_id=identity,agent_name='worker',project_id=row['request']['project_id'],execution_instance_id=execution,decision=decision)
        if evidence=='foreign_execution':decision['diagnostics']['native_execution_instance']=str(uuid.uuid4())
        with path.open('a',encoding='utf-8') as out:out.write(json.dumps(row)+'\n')
    if evidence=='hook_continued':
        append([dict(scope,type='user',isMeta=True,uuid='hook-feedback',parentUuid=assistant,message=dict(role='user',content='continue checking'))])
    elif evidence=='rewritten_history':
        p['log'].write_text(p['log'].read_text(encoding='utf-8').replace(reply,'tampered terminal result'),encoding='utf-8')
    replacement=p['helper']();change(p['backend'],lambda s:s.update(provider_pids=[replacement.pid]))
    reopened=p['open']()
    if evidence in ['foreign_execution','rewritten_history']:
        assert reopened.returncode!=0
        assert mutate_count(p['backend'],['agent','start'])==1
    else:
        assert reopened.returncode==0,reopened.stderr
        watched=cli('--state-root',p['runtime'],'watch',identity)
        result=json.loads(watched.stdout)
        assert result['status']==('completed' if evidence=='recorded' else 'incomplete')
        if evidence=='recorded':assert result['reply']==reply
    assert mutate_count(p['backend'],['agent','prompt'])==1


@pytest.mark.parametrize('invalid',['duplicate_anchor','foreign_anchor','missing_history'])
def test_deferred_recovery_refuses_unknown_or_ambiguous_session_without_launch(project,invalid):
    p=project;original=json.loads(p['pi'].args[2])
    Path(original['native_session_path']).write_text(json.dumps(dict(type='session',id=original['session_id'],cwd=str(p['root'])))+'\n',encoding='utf-8')
    change(p['backend'],lambda s:s['agent_sessions'].pop('codex'))
    assert p['open']().returncode==0
    wait_for(p['ready'],timeout=30)
    submitted=cli('--state-root',p['native'],'ask','worker','deferred identity test')
    assert submitted.returncode==0,submitted.stderr
    prompt=wait_for(lambda:p['prompts']())[-1]
    killed=cli('--state-root',p['native'],'kill',timeout=25);assert killed.returncode==0,killed.stderr
    for proc in [p['pi'],p['codex']]:proc.kill();proc.wait(timeout=5)
    if invalid=='missing_history':p['log'].unlink()
    else:
        finish_worker(p,prompt['body'] if invalid=='duplicate_anchor' else 'CCB_REQ_ID: unrelated\nforeign task','result','old-turn')
        if invalid=='duplicate_anchor':
            other=str(uuid.uuid4());duplicate=p['log'].parent/f'rollout-{other}.jsonl'
            rows=p['log'].read_text(encoding='utf-8').splitlines();header=json.loads(rows[0]);header['payload']['id']=other
            duplicate.write_text('\n'.join([json.dumps(header),*rows[1:]])+'\n',encoding='utf-8')
    gate=p['root']/'held-worker-lead-ready';original['start_gate']=str(gate)
    pi=subprocess.Popen([NODE,str(HOST),json.dumps(original)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                        text=True,encoding='utf-8',creationflags=0x08000000)
    try:
        change(p['backend'],lambda s:s.update(provider_pids=[pi.pid]))
        assert p['open'](0).returncode==0
        wait_for(lambda:(p['native']/'herdr/herdr-recovery.json').exists() and read(p['native']/'herdr/herdr-recovery.json')['phase']=='reset',timeout=20)
        gate.touch();assert json.loads(pi.stdout.readline())['ready']
        wait_for(p['controller_ready'],timeout=20)
        worker=next(a for a in json.loads(cli('--state-root',p['native'],'agents').stdout)['agents'] if a['agent_name']=='worker')
        assert worker['activation']['reason']=='recovery_evidence_required'
        calls=[json.loads(line) for line in (p['backend']/'calls.jsonl').read_text().splitlines()]
        assert sum(c[:2]==['agent','start'] and c[c.index('--kind')+1]=='codex' for c in calls if '--kind' in c)==1
        assert mutate_count(p['backend'],['agent','prompt'])==1
        assert job(p,'worker',json.loads(submitted.stdout)['job_id'])['status']=='running'
    finally:
        if pi.poll() is None:pi.kill()
        pi.wait(timeout=5)


def test_reopen_settles_a_delivery_merged_with_a_draft_from_the_native_session(project):
    # The typed result was merged with an unsent draft, so the bridge could not
    # attribute the turn and the delivery stayed running when Pi was closed.
    p = project
    original = json.loads(p['pi'].args[2])
    history = Path(original['native_session_path'])
    history.write_text(json.dumps({'type':'session', 'id':original['session_id'], 'cwd':str(p['root'])})+'\n', encoding='utf-8')
    assert p['open']().returncode == 0
    wait_for(p['ready'], timeout=30)
    asked = cli('--state-root', p['native'], 'ask', '--from', 'lead', 'worker', 'plain worker task')
    assert asked.returncode == 0, asked.stderr
    worker = wait_for(lambda:p['prompts']())[-1]
    finish_worker(p, worker['body'], 'worker plain result', 'plain-turn'); p['idle']('worker')
    delivery = wait_for(lambda:next((r for r in p['prompts']() if 'CCB_REPLY from=worker' in r['body']), None), timeout=20)
    merged = 'unsent draft ' + delivery['body']
    p['command'](action='prompt', prompt=merged, reply='read it'); p['idle']('lead')
    with history.open('a', encoding='utf-8') as out:
        out.write(json.dumps({'type':'message', 'message':{'role':'user', 'content':[{'type':'text', 'text':merged}]}})+'\n')
    rows = lambda: [json.loads(line) for line in (p['root']/'.ccb/agents/lead/jobs.jsonl').read_text(encoding='utf-8').splitlines()]
    delivery_id = next(r['job_id'] for r in rows() if r['request']['message_type'] == 'reply_delivery')
    assert job(p, 'lead', delivery_id)['status'] == 'running'
    retired = cli('--state-root', p['native'], 'kill', timeout=25)
    assert retired.returncode == 0, retired.stderr
    for proc in [p['pi'], p['codex']]:
        proc.kill(); proc.wait(timeout=5)
    gate = p['root']/'resume-ready'
    original['start_gate'] = str(gate)
    pi = subprocess.Popen([NODE, str(HOST), json.dumps(original)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, text=True, encoding='utf-8', creationflags=0x08000000)
    codex = subprocess.Popen([sys.executable, '-I', '-c', 'import time;time.sleep(180)'], creationflags=0x08000000)
    try:
        change(p['backend'], lambda s:s.update(provider_pids=[pi.pid, codex.pid]))
        assert p['open'](0).returncode == 0
        wait_for(lambda:(p['native']/'herdr/herdr-recovery.json').exists() and
                 read(p['native']/'herdr/herdr-recovery.json')['phase'] == 'reset', timeout=20)
        gate.touch()
        assert json.loads(pi.stdout.readline())['ready']
        result = p['open']()
        assert result.returncode == 0, result.stderr
        wait_for(p['ready'], timeout=20)
        settled = job(p, 'lead', delivery_id)
        assert settled['status'] == 'completed'
        assert settled['terminal_decision']['reason'] == 'reply_delivery_anchor_submitted'
        assert sum('CCB_REPLY from=worker' in r['body'] for r in p['prompts']()) == 1
    finally:
        for proc in [pi, codex]:
            if proc.poll() is None:proc.kill()
            proc.wait(timeout=5)
