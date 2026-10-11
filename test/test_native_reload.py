"""Configuration cutover against disposable native services and real Git fixtures."""
import copy
import json
import os
import shlex
import sys
import time
from pathlib import Path
from types import SimpleNamespace

import pytest
from test_native_daemon import Server, spec, submit, await_watch
from test_native_cli_security import cli
from test_native_pi_rpc import pi_spec
from test_native_codex_rpc import codex_spec
from test_native_claude_stream import claude_spec
from test_native_workspaces import repo, compiled, manifest, git, start, stop, commit


def config(agents):
    return dict(schema_version=2, project_id='native-test', agents=agents)


def plan(server, desired):
    return server.rpc('reload-plan', {'config': desired})['plan']


def reload(server, desired):
    current = server.rpc('ping')['config_fingerprint']
    return server.rpc('reload', dict(config=desired, expected_fingerprint=current))['reload']


def settled(server, operation, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        row = server.rpc('reload-status')['reload']
        assert row['operation_id'] == operation['operation_id']
        if row['status'] not in {'preparing', 'retiring'}:
            return row
        time.sleep(.02)
    pytest.fail(f'reload did not settle: {row}')


def applied(server, desired):
    receipt = reload(server, desired)
    assert receipt['status'] in {'preparing', 'retiring'}, receipt
    final = settled(server, receipt)
    assert final['status'] == 'applied', final
    return final


def test_plan_is_read_only_and_noop_is_idempotent_with_busy_worker(tmp_path):
    agents = {'worker': spec()}
    server = Server(tmp_path/'state', agents)
    try:
        job = submit(server)['job_id']
        before = server.rpc('ping')
        desired = config(dict(agents, extra=spec()))
        observed = plan(server, desired)
        assert observed['added'] == ['extra'] and observed['can_apply']
        assert not (server.root/'reload.json').exists()
        assert server.rpc('ping')['config_fingerprint'] == before['config_fingerprint']
        noop = server.rpc('reload', dict(config=config(agents), expected_fingerprint='stale'))['reload']
        assert noop['status'] == 'unchanged'
        assert server.rpc('watch', {'target': job})['status'] == 'accepted'
    finally: server.close()


def test_addition_preserves_active_and_queued_fifo_and_daemon_identity(tmp_path):
    agents = {'worker': spec()}
    server = Server(tmp_path/'state', agents)
    try:
        first = submit(server, body='first')['job_id']
        second = submit(server, body='second')['job_id']
        active = server.rpc('work', {'agent_name':'worker'})['job']
        before = server.rpc('ping')
        applied(server, config(dict(agents, extra=spec())))
        assert server.rpc('ping')['pid'] == before['pid']
        assert server.rpc('watch', {'target': first})['status'] == 'running'
        assert server.rpc('work', {'agent_name':'worker'})['job'] is None
        assert server.rpc('queue', {'target':'worker'})['queues'][0]['queued_job_ids'] == [second]
        latest = server.rpc('records.find_last', dict(stream='jobs',agent_name='worker',fields={'job_id':first}),version=1)['result']
        assert latest['provider_options']['native_execution_instance'] == active['provider_options']['native_execution_instance']
        assert submit(server,'extra')['status'] == 'accepted'
    finally: server.close()


@pytest.mark.parametrize('remove', [False, True])
def test_busy_agent_change_or_removal_is_blocked_without_mutation(tmp_path, remove):
    agents = {'worker':spec(), 'other':spec()}
    server = Server(tmp_path/'state', agents)
    try:
        job = submit(server)['job_id']
        desired = config(copy.deepcopy(agents))
        if remove: del desired['agents']['worker']
        else: desired['agents']['worker']['timeout_s'] = 31
        before = server.rpc('ping')['config_fingerprint']
        status = reload(server, desired)
        assert status['status'] == 'blocked'
        assert {'unfinished_job', 'pending_mailbox'} <= {b['reason'] for b in status['plan']['blockers']}
        assert server.rpc('ping')['config_fingerprint'] == before
        assert server.rpc('watch', {'target':job})['status'] == 'accepted'
        assert not (server.root/'reload.json').exists()
    finally: server.close()


def test_idle_lead_cannot_be_removed_while_waiting_for_worker_result(tmp_path):
    agents = {'worker':spec(), 'lead':spec()}
    server = Server(tmp_path/'state', agents)
    try:
        submit(server, actor='lead')
        assert server.rpc('mailbox-head', {'agent_name':'lead'})['head'] is None
        status = reload(server, config({'worker':agents['worker']}))
        assert status['status'] == 'blocked'
        assert any(b['reason']=='outstanding_reply' and b['agent_name']=='lead' for b in status['plan']['blockers'])
    finally: server.close()


def test_pending_result_delivery_prevents_lead_removal(tmp_path):
    agents = {'worker':spec('worker'), 'lead':spec()}
    server = Server(tmp_path/'state', agents)
    try:
        job = submit(server, actor='lead')['job_id']
        await_watch(server, job, lambda r:r['terminal'])
        status = reload(server, config({'worker':agents['worker']}))
        assert status['status'] == 'blocked'
        assert any(b['reason']=='pending_mailbox' for b in status['plan']['blockers'])
        assert server.rpc('mailbox-head', {'agent_name':'lead'})['head'] is not None
    finally: server.close()


def test_addition_during_chain_keeps_automatic_lead_continuation(tmp_path):
    agents = {'worker':spec('slow-input'), 'lead':spec('lead')}
    server = Server(tmp_path/'state', agents)
    try:
        job = submit(server, 'lead')['job_id']
        deadline = time.monotonic()+5
        while time.monotonic()<deadline:
            edges = server.rpc('records.latest',dict(stream='callbacks',identity='edge_id'),version=1)['result']
            if edges: break
            time.sleep(.02)
        assert edges
        blocked = plan(server, config({'worker':agents['worker']}))
        assert not blocked['can_apply'] and any(b['reason']=='open_callback' for b in blocked['blockers'])
        applied(server, config(dict(agents, extra=spec())))
        done = await_watch(server, job, lambda r:r['visible_reply_source']=='message_bureau_reply')
        assert done['reply'] == 'reviewed: worker result'
    finally: server.close()


@pytest.mark.parametrize('provider', ['pi', 'codex', 'claude'])
@pytest.mark.skipif(os.name!='nt', reason='owned process tree retirement currently requires Windows')
def test_idle_provider_replacement_keeps_unaffected_session_and_queryable_history(tmp_path, provider):
    factories = {'pi':lambda:pi_spec('worker'), 'codex':codex_spec, 'claude':claude_spec}
    agents = {'worker':factories[provider](), 'keep':pi_spec('worker')}
    server = Server(tmp_path/'state', agents)
    try:
        old_job = submit(server)['job_id']
        await_watch(server, old_job, lambda r:r['terminal'])
        kept_job = submit(server,'keep')['job_id']
        await_watch(server, kept_job, lambda r:r['terminal'])
        old = server.rpc('provider-status', {'agent_name':'worker'})['owned']
        kept = server.rpc('provider-status', {'agent_name':'keep'})['owned']
        desired = config(copy.deepcopy(agents))
        desired['agents']['worker']['timeout_s'] = 77
        applied(server, desired)
        from native_kernel import _owner_stopped
        assert _owner_stopped(old['pid'])
        replacement = server.rpc('provider-status', {'agent_name':'worker'})['owned']
        assert replacement is None or replacement['runtime_instance_id'] != old['runtime_instance_id']
        assert server.rpc('provider-status', {'agent_name':'keep'})['owned']['runtime_instance_id'] == kept['runtime_instance_id']
        next_job = submit(server)['job_id']
        await_watch(server, next_job, lambda r:r['terminal'])
        assert server.rpc('provider-status', {'agent_name':'worker'})['owned']['runtime_instance_id'] != old['runtime_instance_id']
        applied(server, config({'keep':agents['keep']}))
        assert server.rpc('watch', {'target':old_job})['terminal']
        assert server.rpc('watch', {'target':next_job})['terminal']
        assert [row['agent_name'] for row in server.rpc('agents')['agents']] == ['keep']
    finally: server.close()
    restarted = Server(server.root, {'keep':agents['keep']})
    try: assert restarted.rpc('watch', {'target':old_job})['terminal']
    finally: restarted.close()


def test_stale_fingerprint_and_foreign_namespace_are_rejected(tmp_path):
    server = Server(tmp_path/'state', {'worker':spec()})
    try:
        desired = config({'worker':spec(), 'extra':spec()})
        with pytest.raises(AssertionError, match='fingerprint mismatch'):
            server.rpc('reload', dict(config=desired, expected_fingerprint='stale'))
        desired['project_id'] = 'foreign'
        with pytest.raises(AssertionError, match='namespace'):
            plan(server, desired)
        assert not (server.root/'reload.json').exists()
    finally: server.close()


@pytest.mark.parametrize('remove', [False, True])
def test_faulted_provider_cannot_be_hidden_by_reload(tmp_path, remove):
    from test_native_provider_faults import fault
    worker=pi_spec('worker');worker['command']=[str(tmp_path/'missing-provider.exe')]
    agents={'worker':worker,'keep':spec()}
    server=Server(tmp_path/'state',agents)
    try:
        observed=fault(server)
        desired=config(copy.deepcopy(agents))
        if remove:del desired['agents']['worker']
        else:desired['agents']['worker']['timeout_s']=11
        result=reload(server,desired)
        assert result['status']=='blocked'
        assert any(b['reason']=='provider_fault' and b['fault_id']==observed['fault_id'] for b in result['plan']['blockers'])
        assert server.rpc('provider-status',{'agent_name':'worker'})['fault']['fault_id']==observed['fault_id']
    finally:server.close()


def test_retry_does_not_recreate_a_reply_route_to_removed_caller(tmp_path):
    agents={'worker':spec(),'lead':spec()}
    server=Server(tmp_path/'state',agents)
    try:
        job=submit(server,actor='lead')['job_id']
        server.rpc('cancel',dict(job_id=job,record_reply=False))
        applied(server,config({'worker':agents['worker']}))
        with pytest.raises(AssertionError,match='caller agent was removed'):
            server.rpc('retry',{'target':job})
        assert server.rpc('watch',{'target':job})['status']=='cancelled'
    finally:server.close()


@pytest.mark.parametrize('remove', [False, True])
def test_explicit_restart_cannot_hide_unfinished_changed_agent(tmp_path, remove):
    agents = {'worker':spec(), 'keep':spec()}
    first = Server(tmp_path/'state', agents)
    job = submit(first)['job_id']
    first.close()
    different = copy.deepcopy(agents)
    if remove: del different['worker']
    else: different['worker']['timeout_s'] = 7
    with pytest.raises(AssertionError, match='strand unfinished work'):
        Server(first.root, different)
    second = Server(first.root, agents)
    try: assert second.rpc('work', {'agent_name':'worker'})['job']['job_id'] == job
    finally: second.close()


def test_cli_dry_run_wait_status_and_config_secrets_not_persisted(tmp_path):
    server = Server(tmp_path/'state', {'worker':spec()})
    try:
        desired = config({'worker':dict(spec(), env={'SECRET':'never-journal-this-value'})})
        file = tmp_path/'desired.json'
        file.write_text(json.dumps(desired), encoding='utf-8')
        args = ['--state-root',server.root,'reload','--config',file]
        dry = cli(*args, '--dry-run')
        assert dry.returncode == 0, dry.stderr
        assert json.loads(dry.stdout)['plan']['changed'] == ['worker']
        assert not (server.root/'reload.json').exists()
        changed = cli(*args, '--wait', '--timeout-ms','5000')
        assert changed.returncode == 0, changed.stderr
        assert json.loads(changed.stdout)['reload']['status'] == 'applied'
        status = cli('--state-root',server.root,'reload-status')
        assert status.returncode == 0 and json.loads(status.stdout)['reload']['status']=='applied'
        for name in ['configuration.json','reload.json','lease.json']:
            assert 'never-journal-this-value' not in (server.root/name).read_text(encoding='utf-8')
        noop = cli(*args)
        assert noop.returncode == 0 and json.loads(noop.stdout)['reload']['status']=='unchanged'
    finally: server.close()


@pytest.mark.parametrize('change_home', [False, True])
@pytest.mark.skipif(os.name!='nt', reason='owned process retirement is Windows-specific')
def test_resume_pointer_is_preserved_only_with_same_provider_workspace_environment(tmp_path, change_home):
    old_file=tmp_path/'old-session.json';new_file=tmp_path/'new-session.json'
    worker=pi_spec('persist');worker['command'].append(str(old_file))
    server=Server(tmp_path/'state',{'worker':worker})
    try:
        job=submit(server)['job_id'];await_watch(server,job,lambda r:r['terminal'])
        previous=json.loads(old_file.read_text());old_bytes=old_file.read_bytes()
        desired=config({'worker':copy.deepcopy(worker)})
        desired['agents']['worker']['timeout_s']=88
        if change_home:
            desired['agents']['worker']['env']={'PI_CODING_AGENT_DIR':str(tmp_path/'new-home')}
            desired['agents']['worker']['command'][-1]=str(new_file)
        applied(server,desired)
        job=submit(server)['job_id'];await_watch(server,job,lambda r:r['terminal'])
        current=json.loads((new_file if change_home else old_file).read_text())
        if change_home:
            assert current['sessionId']!=previous['sessionId'] and current['messageCount']==2
            assert old_file.read_bytes()==old_bytes
        else:
            assert current['sessionId']==previous['sessionId'] and current['messageCount']==4
    finally:server.close()


def test_worktree_addition_is_prepared_without_restarting_service(tmp_path):
    project = repo(tmp_path)
    started = start(project)
    state = project/'.ccb/ccbd'
    try:
        (project/'.ccb/ccb.config').write_text('worker:claude(worktree),extra:claude(worktree)')
        data = compiled(project)
        extra = data['agents']['extra']['workspace_path']
        result = cli('reload','--executables',manifest(project),'--wait',cwd=project)
        assert result.returncode == 0, result.stderr
        assert json.loads(result.stdout)['reload']['status'] == 'applied'
        ping=json.loads(cli('--state-root',state,'ping').stdout)
        assert ping['pid'] == started['pid'] and ping['startup_id']==started['startup_id']
        assert start(project)['pid']==started['pid']
        assert git(project,'worktree','list','--porcelain').stdout.count('worktree ') == 3
        assert git(extra,'rev-parse','HEAD').returncode == 0
    finally: stop(state)


def test_failed_workspace_prepare_retains_old_configuration(tmp_path):
    project = repo(tmp_path)
    start(project)
    state = project/'.ccb/ccbd'
    try:
        before = json.loads(cli('--state-root',state,'ping').stdout)['config_fingerprint']
        (project/'.ccb/ccb.config').write_text('worker:claude(worktree),extra:claude(worktree)')
        desired = compiled(project)
        from pathlib import Path
        target = Path(desired['agents']['extra']['workspace_path'])
        target.mkdir(parents=True)
        (target/'unrelated.txt').write_text('preserve')
        desired_file=tmp_path/'desired.json';desired_file.write_text(json.dumps(desired),encoding='utf-8')
        result = cli('reload','--config',desired_file,'--wait',cwd=project)
        assert result.returncode != 0
        status = json.loads(result.stdout)['reload']
        assert status['status']=='failed' and status['reason']=='workspace_preparation_failed'
        assert json.loads(cli('--state-root',state,'ping').stdout)['config_fingerprint'] == before
        assert (target/'unrelated.txt').read_text()=='preserve'
        assert [a['agent_name'] for a in json.loads(cli('--state-root',state,'agents').stdout)['agents']] == ['worker']
    finally: stop(state)


def connect(state):
    # Borrow the existing fixture transport without starting a second daemon.
    server = object.__new__(Server)
    server.root = state
    server.endpoint = json.loads((state/'kernel/control-plane.json').read_text())
    server.token = SimpleNamespace(token=json.loads(Path(server.endpoint['token_ref']).read_text())['token'])
    return server


def gated_worktree(tmp_path, fault_provider=False):
    project = repo(tmp_path)
    (project/'.ccb/ccb.config').write_text('worker:claude,keep:claude')
    (project/'.gitattributes').write_text('source.txt filter=fixture_gate\n')
    git(project,'add','.gitattributes'); commit(project)
    entered = tmp_path/'filter-entered'
    release = tmp_path/'filter-release'
    script = tmp_path/'filter.py'
    script.write_text(
        'import sys,time\nfrom pathlib import Path\n'
        'data=sys.stdin.read()\n'
        f'Path({str(entered)!r}).write_text("entered")\n'
        'deadline=time.monotonic()+20\n'
        f'while not Path({str(release)!r}).exists() and time.monotonic()<deadline: time.sleep(.02)\n'
        'sys.stdout.write(data)\n', encoding='utf-8')
    git(project,'config','filter.fixture_gate.smudge',
        f'{shlex.quote(Path(sys.executable).as_posix())} -I {shlex.quote(script.as_posix())}')
    state = project/'.ccb/ccbd'
    if fault_provider:
        from test_native_provider_faults import provider_spec
        initial=compiled(project)
        initial['agents']['worker']=provider_spec('pi','idle-late-fault',tmp_path)
        initial_file=tmp_path/'startup-config.json';initial_file.write_text(json.dumps(initial),encoding='utf-8')
        result=cli('--state-root',state,'start','--config',initial_file)
        assert result.returncode==0,result.stderr
        started=json.loads(result.stdout)
    else:started = start(project)
    server = connect(state)
    deadline = time.monotonic()+5
    while time.monotonic()<deadline:
        if server.rpc('provider-status',{'agent_name':'worker'})['owned']['ready']: break
        time.sleep(.02)
    (project/'.ccb/ccb.config').write_text('worker:claude(worktree),keep:claude')
    desired = compiled(project)
    return project, state, server, started, desired, entered, release


def test_slow_preparation_is_nonblocking_fenced_idempotent_and_observation_timeout_is_not_restart(tmp_path):
    project,state,server,started,desired,entered,release = gated_worktree(tmp_path)
    try:
        original = server.rpc('provider-status',{'agent_name':'worker'})['owned']
        operation = reload(server,desired)
        assert operation['status']=='preparing'
        deadline = time.monotonic()+5
        while not entered.exists() and time.monotonic()<deadline: time.sleep(.02)
        assert entered.exists()
        begin = time.monotonic()
        assert server.rpc('ping')['pid']==started['pid']
        assert time.monotonic()-begin<.8
        assert server.rpc('provider-status',{'agent_name':'worker'})['owned']['runtime_instance_id']==original['runtime_instance_id']
        assert reload(server,desired)['operation_id']==operation['operation_id']
        other = copy.deepcopy(desired);other['agents']['keep']['timeout_s']=123
        with pytest.raises(AssertionError,match='another configuration reload'):
            reload(server,other)
        for target,actor in [('worker','user'),('all','user'),('keep','worker'),('keep',' WORKER ')]:
            with pytest.raises(AssertionError,match='reload in progress'):
                server.rpc('submit',dict(project_id=desired['project_id'],to_agent=target,from_actor=actor,body='fenced',
                                        delivery_scope='broadcast' if target=='all' else 'single'))
        with pytest.raises(AssertionError,match='reload in progress'): server.rpc('shutdown')
        desired_file=tmp_path/'desired.json';desired_file.write_text(json.dumps(desired),encoding='utf-8')
        timeout = cli('reload','--config',desired_file,'--wait','--timeout-ms','0',cwd=project)
        assert timeout.returncode==0,timeout.stderr
        observed=json.loads(timeout.stdout)
        assert observed['observation_timeout'] and observed['reload']['operation_id']==operation['operation_id']
        receipt=server.rpc('submit',dict(project_id=desired['project_id'],to_agent='keep',from_actor='user',body='unaffected'))
        deadline=time.monotonic()+5
        while time.monotonic()<deadline:
            watched=server.rpc('watch',{'target':receipt['job_id']})
            if watched['terminal']:break
            time.sleep(.02)
        assert watched['status']=='completed'
        release.write_text('continue')
        assert settled(server,operation)['status']=='applied'
        assert server.rpc('ping')['pid']==started['pid']
    finally:
        release.write_text('continue')
        deadline=time.monotonic()+5
        while time.monotonic()<deadline:
            if server.rpc('reload-status')['reload']['status'] not in {'preparing','retiring'}:break
            time.sleep(.02)
        stop(state)


@pytest.mark.skipif(os.name!='nt', reason='owner-death process tree proof is Windows-specific')
def test_crash_during_prepare_is_not_replayed_and_requires_explicit_configuration(tmp_path):
    import signal
    from native_kernel import _owner_stopped
    project,state,server,started,desired,entered,release = gated_worktree(tmp_path)
    try:
        before=server.rpc('ping')['config_fingerprint']
        operation=reload(server,desired)
        deadline=time.monotonic()+5
        while not entered.exists() and time.monotonic()<deadline:time.sleep(.02)
        assert entered.exists()
        os.kill(started['pid'],signal.SIGTERM)
        deadline=time.monotonic()+5
        while not _owner_stopped(started['pid']) and time.monotonic()<deadline:time.sleep(.02)
        assert _owner_stopped(started['pid'])
        release.write_text('continue')
        (project/'.ccb/ccb.config').write_text('worker:claude,keep:claude')
        restarted=start(project)
        server=connect(state)
        assert restarted['pid']!=started['pid']
        assert server.rpc('ping')['config_fingerprint']==before
        status=server.rpc('reload-status')['reload']
        assert status['status']=='interrupted' and status['operation_id']==operation['operation_id']
        saved=json.loads((state/'configuration.json').read_text())
        assert saved['config_fingerprint']==before
    finally:
        release.write_text('continue')
        stop(state)


@pytest.mark.skipif(os.name!='nt', reason='owned process retirement is Windows-specific')
def test_protocol_fault_buffered_during_prepare_remains_recoverable_after_failed_cutover(tmp_path):
    project,state,server,started,desired,entered,release = gated_worktree(tmp_path, fault_provider=True)
    try:
        operation=reload(server,desired)
        deadline=time.monotonic()+5
        while not entered.exists() and time.monotonic()<deadline:time.sleep(.02)
        assert entered.exists()
        (tmp_path/'release-error').touch()
        deadline=time.monotonic()+5
        while not (tmp_path/'fault-emitted').exists() and time.monotonic()<deadline:time.sleep(.02)
        assert (tmp_path/'fault-emitted').exists()
        release.write_text('continue')
        assert settled(server,operation)['reason']=='provider_retirement_protocol_error'
        failure=server.rpc('provider-status',{'agent_name':'worker'})['fault']
        assert failure['phase']=='poll' and failure['reason']=='provider_protocol_error'
        assert 'do-not-persist-protocol-error-secret' not in (state/'providers/pi/worker/fault.json').read_text()
        stop(state)
        from native_kernel import _owner_stopped
        deadline=time.monotonic()+5
        while not _owner_stopped(started['pid']) and time.monotonic()<deadline:time.sleep(.02)
        assert _owner_stopped(started['pid'])
        restarted=cli('--state-root',state,'start','--config',tmp_path/'startup-config.json')
        assert restarted.returncode==0,restarted.stderr
        server=connect(state)
        assert server.rpc('provider-status',{'agent_name':'worker'})['fault']['fault_id']==failure['fault_id']
        assert server.rpc('provider-recover',{'agent_name':'worker','fault_id':failure['fault_id']})['status']=='recovered'
    finally:
        release.write_text('continue')
        (tmp_path/'release-fault').touch()
        if (state/'kernel/control-plane.json').exists():stop(state)
