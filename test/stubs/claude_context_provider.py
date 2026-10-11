"""UUID lifecycle fixture following the installed Claude 2.1.296 wire contract."""
import json, sys, uuid, time, threading
from pathlib import Path

mode=sys.argv[1];root=Path(sys.argv[2]);root.mkdir(exist_ok=True)
flag='--resume' if '--resume' in sys.argv else '--session-id'
session=sys.argv[sys.argv.index(flag)+1]
if mode=='flush-on-eof' and flag=='--resume':
    saved=root/'flushed-session'
    if not saved.exists() or saved.read_text()!=session:sys.exit(2)
lock=threading.Lock();abort=threading.Event();active_user=None

def uid():return str(uuid.uuid4())
def emit(value):
    with lock:print(json.dumps(value),flush=True)
def initialized():emit(dict(type='system',subtype='init',session_id=session,uuid=uid()))
def lifecycle(user,state):emit(dict(type='command_lifecycle',command_uuid=user['uuid'],state=state,session_id=session,uuid=uid()))
def result(user,action=None,error=False,interrupted=False):
    row=dict(type='result',subtype='error_during_execution' if error else 'success',is_error=error,
             session_id=session,uuid=uid(),user_message_uuid=user['uuid'],user_message_uuids=[user['uuid']],
             queued_turn_count=0,result='PRIVATE_PROVIDER_DETAIL' if error else 'normal reply',parent_tool_use_id=None)
    if action and mode!='missing-local':row['local_command']=action
    if interrupted:row['terminal_reason']='aborted_streaming'
    return row
def hold():
    until=time.monotonic()+15
    while not (root/'release').exists() and not abort.is_set() and time.monotonic()<until:time.sleep(.01)
def initialize(command):
    if mode=='delay-preflight':hold()
    if mode=='unowned-reset':
        emit(dict(type='conversation_reset',trigger='clear',session_id=session,uuid=uid(),new_conversation_id=uid(),user_message_uuid=uid()))
    commands=[dict(name=name,builtin=True) for name in ['clear','compact']]
    if mode=='shadow':commands[0]['builtin']=False
    if mode=='duplicate':commands.append(commands[0])
    p=dict(claude_code_version='2.1.200' if mode=='old-version' else '2.1.296',session_state='idle',commands=commands)
    response=dict(subtype='success',request_id=command['request_id'],response=p,
                  pending_permission_requests=[{}] if mode=='pending-permission' else [],pending_user_dialog_requests=[])
    emit(dict(type='control_response',response=response))
def control(user):
    global session
    action=user['message']['content'][1:]
    lifecycle(user,'queued');lifecycle(user,'started')
    if mode=='delay':hold()
    if action=='clear' and mode not in {'reject','false-success'} and not abort.is_set():
        display=uid()
        emit(dict(type='conversation_reset',trigger='clear',uuid=display,new_conversation_id=display,
                  session_id=session,user_message_uuid='foreign' if mode=='foreign-reset' else user['uuid']))
        (root/'previous-session').write_text(session)
        session=uid();(root/'display-id').write_text(display);(root/'actual-session').write_text(session)
    initialized()
    if mode=='foreign-start':
        emit(dict(type='command_lifecycle',command_uuid=uid(),state='started',session_id=session,uuid=uid()))
    if mode=='effect-then-cancel':
        (root/'effect-present').touch();hold()
    if action=='compact' and mode not in {'no-boundary','reject'} and not abort.is_set():
        emit(dict(type='system',subtype='compact_boundary',session_id=session,uuid=uid(),
                  compact_metadata={'trigger':'auto' if mode=='auto-boundary' else 'manual','pre_tokens':200}))
    replay=dict(user,session_id=session,message={'role':'user','content':f'<command-name>/{action}</command-name>\n<command-message>{action}</command-message>\n<command-args></command-args>'})
    emit(replay)
    row=result(user,action,error=mode=='reject',interrupted=abort.is_set())
    if mode=='foreign-result':
        foreign=dict(row,user_message_uuid=uid(),user_message_uuids=[]);emit(foreign);hold()
    terminal='cancelled' if abort.is_set() else 'failed' if mode=='reject' else 'completed'
    if mode=='lifecycle-first':lifecycle(user,terminal)
    emit(row)
    if mode!='lifecycle-first':lifecycle(user,terminal)

initialized()
for line in sys.stdin:
    command=json.loads(line)
    with (root/'wire.jsonl').open('a',encoding='utf-8') as f:f.write(json.dumps(command)+'\n')
    if command['type']=='control_request':
        kind=command['request']['subtype']
        if kind=='initialize':threading.Thread(target=initialize,args=(command,),daemon=True).start()
        elif kind=='interrupt':
            abort.set()
            emit(dict(type='control_response',response=dict(subtype='success',request_id=command['request_id'],response={'cancelled':[active_user] if mode=='effect-then-cancel' else [],'still_queued':[]})))
        else:emit(dict(type='control_response',response=dict(subtype='error',request_id=command['request_id'],error='unsupported')))
    elif command['message']['content'] in ['/clear','/compact']:
        abort.clear();active_user=command['uuid']
        threading.Thread(target=control,args=(command,),daemon=True).start()
    else:
        if mode=='flush-busy':
            emit(command);(root/'busy-prompt').touch();continue
        if mode=='unowned-followup':
            emit(dict(type='conversation_reset',trigger='plan_mode_exit',session_id=session,uuid=uid(),new_conversation_id=uid()))
        emit(command)
        emit(dict(type='assistant',session_id=session,parent_tool_use_id=None,message={'content':[{'type':'text','text':'normal reply'}]}))
        emit(result(command))

if mode in {'flush-on-eof','flush-busy'}:
    # Emulate a provider whose completed local command is persisted at clean EOF.
    time.sleep(.05)
    (root/'flushed-session').write_text(session)
if mode=='eof-hang':time.sleep(10)
