"""Explicit live Claude/local-MCP acceptance; no external MCP, tools or auth copy.

CCB_RUN_REAL_MODELS=1 opts into one model task and a fixture-only form answer.
"""
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import uuid

from native_keeper_live_probe import cli, CLAUDE, NATIVE


def main():
    assert os.name == 'nt' and os.environ.get('CCB_RUN_REAL_MODELS') == '1'
    assert CLAUDE.is_file() and NATIVE.is_file()
    trial = Path(tempfile.mkdtemp(prefix='ccb-claude-elicitation-live-', dir=Path.home()/'Desktop/work'))
    state = trial/'.ccb/ccbd'; trace = trial/'mcp-wire.jsonl'
    mcp = trial/'mcp.json'
    fixture = Path(__file__).parent/'stubs/mcp_elicitation_server.py'
    mcp.write_text(json.dumps({'mcpServers':{'ccb_fixture':{'command':sys.executable,'args':['-I',str(fixture),str(trace)]}}}),encoding='utf-8')
    worker = dict(provider='claude',driver_kind='claude_stream',profile_family='protocol_turn',workspace_path=str(trial),
                  command=[str(CLAUDE),'-p','--input-format','stream-json','--output-format','stream-json','--verbose',
                           '--replay-user-messages','--settings','{"disableAllHooks":true}','--strict-mcp-config','--mcp-config',str(mcp),
                           '--tools','','--allowedTools','mcp__ccb_fixture__choose_value'],
                  env={'DISABLE_AUTOUPDATER':'1','CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC':'1'},timeout_s=120)
    config=dict(schema_version=2,project_id=uuid.uuid4().hex,project_root=str(trial),agents={'worker':worker})
    launch=trial/'launch.json';launch.write_text(json.dumps(config,indent=2),encoding='utf-8')
    marker='CCB_ELICITATION_OK_'+uuid.uuid4().hex[:8]
    report={'trial_root':str(trial),'marker':marker,'account_settings_changed':False,'credentials_copied':False,
            'external_mcp_connected':False,'builtin_tools_enabled':False,'passed':False}
    def save(): (trial/'result.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n',encoding='utf-8')
    save();print(json.dumps({'trial_root':str(trial),'phase':'starting'}),flush=True)
    try:
        report['start']=cli('--state-root',state,'start','--config',launch);save()
        report['job_id']=cli('--state-root',state,'ask','worker','--',
            f'Call the MCP tool mcp__ccb_fixture__choose_value exactly once. Wait for its result. If it returns alpha, reply exactly "{marker} alpha". If the tool is unavailable or fails, report the actual error. Do not use other tools.')['job_id']
        save();end=time.monotonic()+150;answered=False
        while time.monotonic()<end:
            report['watch']=cli('--state-root',state,'watch',report['job_id'])
            if report['watch'].get('terminal'):break
            requests=cli('--state-root',state,'request','provider-requests',json.dumps({'agent_name':'worker'}))['requests']
            for row in requests:
                if row['status']!='awaiting_user' or answered:continue
                if row['method']!='elicitation' or row['params'].get('mcp_server_name')!='ccb_fixture':
                    raise RuntimeError('Unexpected interaction; no answer supplied')
                report['request']={key:row[key] for key in ['request_id','method','status','attribution','job_id','runtime_instance_id','execution_instance_id']}
                payload={key:row[key] for key in ['request_id','job_id','runtime_instance_id','execution_instance_id']}
                payload.update(agent_name='worker',result={'action':'accept','content':{'choice':'alpha'}})
                report['response']=cli('--state-root',state,'request','provider-respond',json.dumps(payload))
                answered=True;save()
            time.sleep(.1)
        records=[json.loads(line) for line in trace.read_text(encoding='utf-8').splitlines()] if trace.is_file() else []
        report['mcp_tool_calls']=sum(r.get('method')=='tools/call' for r in records)
        report['mcp_form_answers']=sum(r.get('result')=={'action':'accept','content':{'choice':'alpha'}} for r in records)
        report['task_passed']=answered and report['mcp_tool_calls']==1 and report['mcp_form_answers']==1 and report['watch'].get('terminal') and report['watch'].get('status')=='completed' and report['watch'].get('reply','').strip()==marker+' alpha'
    except Exception as exc:report['error']=str(exc)
    finally:
        save()
        try:
            report['stop']=cli('--state-root',state,'stop',ok=False)
            end=time.monotonic()+20
            while time.monotonic()<end:
                status=cli('--state-root',state,'keeper-status',ok=False)
                if status and status.get('alive') is False:
                    report['keeper_stopped']=status.get('keeper',{}).get('phase')=='stopped';break
                time.sleep(.2)
        except Exception as exc:report['cleanup_error']=str(exc)
        report['passed']=bool(report.get('task_passed') and report.get('keeper_stopped') and not report.get('error') and not report.get('cleanup_error'));save()
    print(json.dumps({'trial_root':str(trial),'passed':report['passed'],'reply':report.get('watch',{}).get('reply'),
                      'error':report.get('error'),'mcp_tool_calls':report.get('mcp_tool_calls'),'mcp_form_answers':report.get('mcp_form_answers'),'keeper_stopped':report.get('keeper_stopped')},ensure_ascii=False),flush=True)
    return 0 if report['passed'] else 1


if __name__=='__main__':raise SystemExit(main())
