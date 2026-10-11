"""A first real request precedes Codex history creation; never a warm-up prompt."""
import json
import time
import uuid
import pytest
from test_native_herdr_worker import runtime, prepared, setup, wait_for
from test_native_herdr_agents import change
from test_native_daemon import await_watch

def defer(r):
    history=r['worker'].pop('herdr_history')
    r['worker']['herdr_discovery']={'root':history['root'],'cwd':history['cwd']}
    r['path'].unlink()
    change(r['backend'],lambda s:s['agents'][r['name']].update(agent_session=None))

def header(r,path=None,source='cli'):
    path=path or r['path'];identity=str(uuid.uuid4())
    path.write_text(json.dumps({'type':'session_meta','payload':{'id':identity,'cwd':str(r['project']),'source':source}})+'\n',encoding='utf-8')
    return identity

def test_first_request_creates_history_and_later_request_uses_persisted_binding(runtime):
    r=runtime;defer(r);s=r['start']();first=r['submit'](s)
    p=wait_for(lambda:r['prompts']())[-1];assert not r['path'].exists()
    identity=header(r);r['finish'](p)
    assert await_watch(s,first['job_id'],lambda row:row['terminal'])['reply']=='terminal worker result'
    saved=json.loads((s.root/'providers/codex/worker/herdr-history.json').read_text())
    assert saved['history']['session_id']==identity
    second=r['submit'](s,body='second');p=wait_for(lambda:r['prompts']() if len(r['prompts']())==2 else None)[-1]
    r['finish'](p,reply='next result',turn='next')
    assert await_watch(s,second['job_id'],lambda row:row['terminal'])['reply']=='next result'
    assert len(r['prompts']())==2

def test_cold_resume_observes_late_created_history_without_replaying(runtime):
    r=runtime;defer(r);s=r['start']();receipt=r['submit'](s);p=wait_for(lambda:r['prompts']())[-1]
    s.expected_exit=True;s.proc.kill();s.proc.wait(timeout=5)
    header(r);r['finish'](p)
    restored=r['start']()
    assert await_watch(restored,receipt['job_id'],lambda row:row['terminal'])['status']=='completed'
    assert len(r['prompts']())==1

def test_foreign_and_subagent_histories_do_not_complete_current_request(runtime):
    r=runtime;defer(r);s=r['start']();receipt=r['submit'](s);p=wait_for(lambda:r['prompts']())[-1]
    header(r,source={'subagent':{'thread_spawn':{}}});r['finish'](p)
    time.sleep(.4);assert not s.rpc('watch',{'target':receipt['job_id']})['terminal']
    r['path'].rename(r['path'].with_name('child.jsonl'))
    header(r);r['finish']({'body':'CCB_REQ_ID: foreign\nforeign task'},turn='foreign')
    time.sleep(.4);assert not s.rpc('watch',{'target':receipt['job_id']})['terminal']
    r['finish'](p,reply='correct',turn='correct')
    assert await_watch(s,receipt['job_id'],lambda row:row['terminal'])['reply']=='correct'

def test_duplicate_execution_anchor_in_two_histories_is_ambiguous(runtime):
    r=runtime;defer(r);s=r['start']();receipt=r['submit'](s);p=wait_for(lambda:r['prompts']())[-1]
    s.expected_exit=True;s.proc.kill();s.proc.wait(timeout=5)
    # Stage complete candidates before publishing either .jsonl name.
    one=r['path'].with_suffix('.one');two=r['path'].with_suffix('.two')
    for path in [one,two]:
        header(r,path)
        with path.open('a',encoding='utf-8') as out:
            for payload in [dict(type='task_started',turn_id='t'),dict(type='user_message',message=p['body']),dict(type='task_complete',turn_id='t',last_agent_message='ambiguous')]:
                out.write(json.dumps(dict(type='event_msg',payload=payload))+'\n')
    one.rename(r['path']);two.rename(r['path'].with_name('duplicate.jsonl'))
    s=r['start']()
    wait_for(lambda:(s.root/'providers/codex/worker/fault.json').exists())
    assert not s.rpc('watch',{'target':receipt['job_id']})['terminal'] and len(r['prompts']())==1
