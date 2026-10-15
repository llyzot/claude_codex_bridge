"""Explicit real-model Keeper acceptance; reuses existing auth, never copies it.

Run manually with CCB_RUN_REAL_MODELS=1; CCB_LIVE_LEAD selects pi (default) or
claude. All jobs, provider sessions and process controls belong to a newly
created trial directory. No foreground UI is opened. Existing account homes
are reused, so providers may write their normal session/account metadata.
"""
import hashlib
import json
import os
import re
from pathlib import Path
# Captured at import: test fixtures isolate HOME/USERPROFILE per test.
REAL_HOME = Path.home()
import subprocess
import tempfile
import time
import tomllib
import uuid

ROOT=Path(__file__).resolve().parents[1]
NATIVE=Path(os.environ.get('CCB_TEST_NATIVE_BIN') or ROOT/'rust/target/release/ccb-native.exe')
NODE=(REAL_HOME/'scoop/apps/nodejs-lts/current/node.exe')
PI=(REAL_HOME/'scoop/apps/nodejs-lts/current/bin/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js')
CODEX=(REAL_HOME/'scoop/apps/nodejs-lts/current/bin/node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe')
CLAUDE=REAL_HOME/'.local/bin/claude.exe'

def cli(*args,ok=True):
    r=subprocess.run([str(NATIVE),*map(str,args)],capture_output=True,text=True,encoding='utf-8',timeout=25)
    if ok and r.returncode:raise RuntimeError(r.stderr.strip())
    return json.loads(r.stdout) if r.returncode==0 else None

def read(path):return json.loads(path.read_text(encoding='utf-8'))

def terminate_owned(identity):
    """Bind termination to an open Windows process handle and creation time."""
    import ctypes
    from ctypes import wintypes
    kernel=ctypes.WinDLL('kernel32',use_last_error=True)
    kernel.OpenProcess.argtypes=[wintypes.DWORD,wintypes.BOOL,wintypes.DWORD]
    kernel.OpenProcess.restype=wintypes.HANDLE
    kernel.GetProcessTimes.argtypes=[wintypes.HANDLE]+[ctypes.POINTER(wintypes.FILETIME)]*4
    kernel.TerminateProcess.argtypes=[wintypes.HANDLE,wintypes.UINT]
    kernel.WaitForSingleObject.argtypes=[wintypes.HANDLE,wintypes.DWORD]
    kernel.WaitForSingleObject.restype=wintypes.DWORD
    kernel.CloseHandle.argtypes=[wintypes.HANDLE]
    handle=kernel.OpenProcess(0x1000|0x100000|0x1,False,identity['pid'])
    if not handle:raise ctypes.WinError(ctypes.get_last_error())
    try:
        times=[wintypes.FILETIME() for _ in range(4)]
        if not kernel.GetProcessTimes(handle,*(ctypes.byref(v) for v in times)):raise ctypes.WinError(ctypes.get_last_error())
        created=(times[0].dwHighDateTime<<32)|times[0].dwLowDateTime
        assert str(created)==identity['created'],'Owned process lifetime changed; refusing termination'
        if not kernel.TerminateProcess(handle,1):raise ctypes.WinError(ctypes.get_last_error())
        assert kernel.WaitForSingleObject(handle,10000)==0,'Owned process termination not observed'
    finally:kernel.CloseHandle(handle)

def until(fn,timeout=180):
    end=time.monotonic()+timeout
    while time.monotonic()<end:
        value=fn()
        if value:return value
        time.sleep(.2)
    raise TimeoutError('Owned trial observation expired; inspect retained state before any retry')

def main():
    assert os.name=='nt' and os.environ.get('CCB_RUN_REAL_MODELS')=='1','Explicit Windows real-model opt-in required'
    for path in [NATIVE,NODE,PI,CODEX]:assert path.is_file(),str(path)
    pi_home=Path(os.environ.get('CCB_LIVE_PI_HOME') or Path.home()/'.pi/agent')
    codex_home=Path(os.environ.get('CCB_LIVE_CODEX_HOME') or Path.home()/'.codex')
    assert (pi_home/'auth.json').is_file() and (codex_home/'auth.json').is_file(),'Existing authenticated homes required'
    trial=Path(tempfile.mkdtemp(prefix='ccb-native-keeper-live-',dir=Path.home()/'Desktop/work'))
    project=trial/'project';project.mkdir();state=project/'.ccb/ccbd'
    marker=uuid.uuid4().hex[:8];review1='REVIEW_ONE_'+marker;review2='REVIEW_TWO_'+marker;worker_marker='WORKER_MEMORY_'+marker
    command=[str(CODEX)]
    # Runtime-only overrides: do not connect unrelated configured MCP servers.
    config_path=codex_home/'config.toml'
    if config_path.is_file():
        for name in tomllib.loads(config_path.read_text(encoding='utf-8')).get('mcp_servers',{}):
            assert re.fullmatch(r'[A-Za-z0-9_-]+',name),'Unsupported MCP override name'
            command+=['-c',f'mcp_servers.{name}.enabled=false']
    command+=['app-server','--stdio']
    lead=dict(provider='pi',driver_kind='pi_rpc',profile_family='protocol_turn',workspace_path=str(project),
        command=[str(NODE),str(PI),'--mode','rpc','--provider','openai','--model','gpt-6-astra','--thinking','low',
                 '--no-extensions','--no-mcp','--no-skills','--no-prompt-templates','--no-themes',
                 '--session-dir',str(trial/'pi-sessions'),'--tools','powershell'],
        env={'PI_CODING_AGENT_DIR':str(pi_home),'PI_SKIP_VERSION_CHECK':'1','PI_TELEMETRY':'0'},timeout_s=180)
    worker=dict(provider='codex',driver_kind='codex_app_server',profile_family='protocol_turn',workspace_path=str(project),
        command=command,env={'CODEX_HOME':str(codex_home)},
        thread_options={'model':'gpt-6.1-sol','approvalPolicy':'never','sandbox':'read-only'},turn_options={'effort':'low'},timeout_s=180)
    lead_kind=os.environ.get('CCB_LIVE_LEAD','pi')
    assert lead_kind in ('pi','claude')
    if lead_kind=='claude':
        assert CLAUDE.is_file()
        lead=dict(provider='claude',driver_kind='claude_stream',profile_family='protocol_turn',workspace_path=str(project),
            command=[str(CLAUDE),'-p','--input-format','stream-json','--output-format','stream-json',
                     '--verbose','--replay-user-messages','--safe-mode','--strict-mcp-config',
                     '--mcp-config','{"mcpServers":{}}','--tools','Bash','--allowedTools','Bash'],
            env={'DISABLE_AUTOUPDATER':'1','CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC':'1'},timeout_s=180)
    identity=hashlib.sha256(str(project.resolve()).replace('\\','/').lower().encode()).hexdigest()
    config=dict(schema_version=2,project_id=identity,project_root=str(project),agents={'lead':lead,'worker':worker})
    launch=trial/'launch.json';launch.write_text(json.dumps(config,indent=2),encoding='utf-8')
    report={'trial_root':str(trial),'status':'starting','real_models':True,'credential_copy_performed':False,
            'existing_homes_modified_by_harness':False,'foreground_ui_opened':False,'production_install_changed':False,
            'lead_provider':lead_kind,'models':{'lead':'gpt-6-astra' if lead_kind=='pi' else 'configured Claude default','worker':'gpt-6.1-sol'},'binary_sha256':hashlib.sha256(NATIVE.read_bytes()).hexdigest()}
    def save(): (trial/'result.json').write_text(json.dumps(report,indent=2,ensure_ascii=False)+'\n',encoding='utf-8')
    def request(op,payload=None):return cli('--state-root',state,'request',op,json.dumps(payload or {}))
    def ask(agent,text):return cli('--state-root',state,'ask',agent,'--',text)['job_id']
    def complete(job,callback=False):
        def observed():
            row=cli('--state-root',state,'watch',job)
            if callback:
                if row['visible_reply_source']=='message_bureau_reply':return row
                if row['terminal'] and row['visible_reply_source']!='callback_delegated_pending':
                    raise RuntimeError('Lead did not delegate a callback: '+row['reply'])
            elif row['terminal']:return row
        return until(observed)
    def metadata():return {name:read(state/f'providers/{provider}/{name}/session.json') for name,provider in [('lead',lead_kind),('worker','codex')]}
    def session_ids(rows):return {'lead':rows['lead']['state']['sessionId' if lead_kind=='pi' else 'session_id'],'worker':rows['worker']['state']['thread_id']}
    def worker_ready():
        path=state/'providers/codex/worker/session.json'
        if not path.is_file():return False
        row=read(path);provider_state=row.get('state') or {}
        if provider_state.get('process_state')=='exited':raise RuntimeError('Codex exited before session readiness')
        return provider_state.get('thread_id')
    started=False;save();print(json.dumps({'trial_root':str(trial),'phase':'starting'}),flush=True)
    try:
        first=cli('--state-root',state,'start','--config',launch);started=True
        report['initial_daemon']=first['pid'];report['keeper_instance_id']=first['keeper_instance_id'];save()
        until(worker_ready,30)
        prompt=f'Delegate this calculation to worker: compute 73*79 without tools, explain two independent calculations, and finish with {worker_marker}. Review its answer; if correct, respond exactly {review1}. Do not do the calculation yourself before delegating or edit files.'
        parent1=ask('lead',prompt);report['first_parent']=parent1;report['status']='first_chain_running';save()
        result1=complete(parent1,True);assert review1 in result1['reply'],result1['reply']
        before=metadata();report['first_reply']=result1['reply'];report['first_session_ids']=session_ids(before)
        report['status']='first_chain_completed';save();print(json.dumps({'phase':report['status']}),flush=True)
        until(lambda:all(row['active_job_id'] is None for row in cli('--state-root',state,'queue')['queues']))
        keeper=cli('--state-root',state,'keeper-status')['keeper']
        assert cli('process-identity',first['pid'])==keeper['daemon']
        launch.unlink() # Restart must use the committed protected configuration.
        terminate_owned(keeper['daemon'])
        report['status']='awaiting_keeper_recovery';save()
        def new_owner():
            row=cli('--state-root',state,'ping',ok=False)
            return row if row and row['startup_id']!=first['startup_id'] else None
        restored=until(new_owner,45);report['restored_daemon']=restored['pid']
        assert cli('--state-root',state,'keeper-status')['keeper']['instance_id']==report['keeper_instance_id']
        recall=complete(ask('worker','Recall the WORKER_MEMORY verification marker from your previous calculation. Reply only with that exact marker. Do not call tools or edit files.'))
        assert recall['status']=='completed' and worker_marker in recall['reply'],recall['reply']
        report['worker_recall']=recall['reply'];report['status']='recovered_worker_context_verified';save()
        print(json.dumps({'phase':report['status']}),flush=True)
        prompt2=f'Recall your previous final REVIEW_ONE marker from this conversation. Delegate to worker: compute 97*89 without tools and explain two independent calculations. Review its answer. If correct, respond with {review2} followed by PREVIOUS= and the previous REVIEW_ONE marker. Do not calculate it yourself before delegating or edit files.'
        parent2=ask('lead',prompt2);report['second_parent']=parent2;save()
        result2=complete(parent2,True)
        assert review2 in result2['reply'] and review1 in result2['reply'],result2['reply']
        after=metadata();ids=session_ids(after)
        assert ids==report['first_session_ids'],ids
        report.update(status='chain_verification_completed',second_reply=result2['reply'],restored_session_ids=ids,
                      lead_state=after['lead']['state'],worker_state=after['worker']['state'])
        report['callbacks']=request('trace',{'target':parent2})
        save();print(json.dumps({'phase':'chain_verification_completed','trial_root':str(trial)}),flush=True)
    except Exception as error:
        report['status']='failed';report['error']=str(error);save();raise
    finally:
        if not started:
            owner=cli('--state-root',state,'keeper-status',ok=False)
            started=bool(owner and owner.get('alive') and owner.get('keeper',{}).get('project_id')==identity)
        if started:
            stopped=cli('--state-root',state,'stop',ok=False)
            report['stop_receipt']=stopped
            def stopped_owner():
                row=cli('--state-root',state,'keeper-status')
                return row if not row['alive'] and row['keeper']['phase']=='stopped' else None
            try:report['final_keeper']=until(stopped_owner,25)
            except Exception:
                report['status']='cleanup_unverified'
                report['cleanup_observation']='incomplete; inspect this exact trial before further controls'
                save()
                raise
            if report['status']=='chain_verification_completed':
                report['status']='passed'
                print(json.dumps({'phase':'passed','trial_root':str(trial)}),flush=True)
            save()

if __name__=='__main__':main()
