"""Permission wire fixture: responses cause no filesystem or tool side effects."""
import json,os,sys,threading,time
from pathlib import Path

mode=sys.argv[1]
assert '--include-partial-messages' in sys.argv
assert sys.argv[sys.argv.index('--permission-prompt-tool')+1]=='stdio'
flag='--resume' if '--resume' in sys.argv else '--session-id'
session=sys.argv[sys.argv.index(flag)+1]
lock=threading.Lock(); current=None

def emit(row):
    with lock:print(json.dumps(row),flush=True)
def later(delay,fn):
    timer=threading.Timer(delay,fn);timer.daemon=True;timer.start()
def finish(user,cancelled=False):
    emit(dict(type='result',subtype='success',is_error=False,session_id=session,uuid='result-'+user['uuid'],
        user_message_uuid=user['uuid'],parent_tool_use_id=None,result='permission fixture completed',
        terminal_reason='aborted_tools' if cancelled else 'end_turn'))
def tool(user,tool_id,name='Read',stamp=True,parent=None,partial=False):
    row=dict(type='stream_event' if partial else 'assistant',session_id=session,parent_tool_use_id=parent)
    if stamp:row['user_message_uuid']='foreign-user' if mode=='foreign-stamp' else user['uuid']
    block=dict(type='tool_use',id=tool_id,name=name,input={})
    if partial:row['event']=dict(type='content_block_start',index=0,content_block=block)
    else:row['message']=dict(role='assistant',content=[block])
    emit(row)

emit(dict(type='system',subtype='init',session_id=session))
for line in sys.stdin:
    row=json.loads(line)
    with open(os.environ['CCB_FIXTURE_TRACE'],'a',encoding='utf-8') as trace:trace.write(json.dumps(row)+'\n')
    if row['type']=='user':
        current=row;emit(row);uid=row['uuid'];tool_id='tool-'+uid
        name='AskUserQuestion' if mode.startswith('question') else 'Read'
        if mode=='mixed-stamp':
            emit(dict(type='stream_event',session_id=session,parent_tool_use_id=None,user_message_uuid=uid,event={'type':'message_start'}))
            emit(dict(type='assistant',session_id=session,parent_tool_use_id=None,user_message_uuid='foreign',message={'content':[]}))
            tool(row,tool_id,name,False,partial=True)
        elif mode=='subagent':
            tool(row,'parent-'+uid,'Agent')
            tool(row,tool_id,name,False,'parent-'+uid)
        elif mode not in ('late-anchor','unknown','dialog'):
            tool(row,tool_id,name,mode not in ('no-stamp','question-no-anchor'),'foreign-parent' if mode=='foreign-parent' else None,mode=='partial')
        request=dict(subtype='can_use_tool',tool_name=name,tool_use_id=tool_id,
            input={'file_path':'SECRET_FIXTURE_INPUT'},decision_reason='SECRET_PERMISSION_REASON',
            default_to_no=True,suppress_always_allow_rule=True)
        if mode=='bidi':request['input']['file_path']='C:/fixture/\u202ehidden\u2069.txt'
        if mode=='native-ui':request['requires_user_interaction']=True
        if mode.startswith('question'):
            request['input']={'questions':[{'question':'Choose a value','header':'Choice','multiSelect':False,'options':[{'label':'A','description':'First'},{'label':'B','description':'Second'}]}]}
            request['requires_user_interaction']=True
            if mode=='question-multi':
                request['input']['questions'].append({'question':'Choose sections','header':'Sections','multiSelect':True,'options':[{'label':'Intro'},{'label':'Summary'}]})
            if mode=='question-duplicate':request['input']['questions']*=2
        if mode=='unknown':request={'subtype':'unregistered_hook','payload':'SECRET_UNSUPPORTED'}
        if mode=='dialog':request={'subtype':'request_user_dialog','dialog_kind':'not-advertised','payload':{}}
        packet=dict(type='control_request',request_id='permission-'+uid,request=request)
        if mode=='foreign-session':packet['session_id']='foreign-session'
        emit(packet)
        if mode=='duplicate':emit(packet)
        if mode=='reuse':emit(dict(packet,request=dict(request,input={'file_path':'changed'})))
        if mode=='late-anchor':later(.6,lambda user=row:tool(user,'tool-'+user['uuid']))
        if mode=='withdraw':later(.4,lambda rid=packet['request_id']:emit(dict(type='control_cancel_request',request_id=rid)))
        if mode in ('complete','dialog'):later(.4,lambda user=row:finish(user))
        if mode=='late-request':later(.2,lambda user=row,packet=packet:(finish(user),later(.3,lambda:emit(packet))))
    elif row['type']=='control_response':
        if row['response']['subtype']=='success':
            root=Path(os.environ['CCB_NATIVE_TOKEN_FILE']).parent.parent
            events=root.parent/'agents'/os.environ['CCB_AGENT_NAME']/'events.jsonl'
            assert any(json.loads(line)['type']=='provider_request_answer_requested' for line in events.read_text().splitlines())
            interrupted=row['response']['response'].get('interrupt') is True
            if interrupted:
                jobs=[json.loads(s) for s in events.with_name('jobs.jsonl').read_text().splitlines()]
                assert jobs[-1]['provider_options']['native_cancel_intent']['record_reply'] is True
            later(.35,lambda user=current,interrupted=interrupted:finish(user,interrupted))
        elif mode in ('unknown','foreign-session'):later(.1,lambda user=current:finish(user))
    elif row['type']=='control_request':
        kind=row['request']['subtype']
        emit(dict(type='control_response',response=dict(subtype='success',request_id=row['request_id'],response={})))
        if kind=='interrupt' and current:
            emit(dict(type='control_cancel_request',request_id='permission-'+current['uuid']))
            later(.1,lambda user=current:finish(user,True))
