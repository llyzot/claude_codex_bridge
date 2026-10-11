"""Owned Claude control-wire fixture. No model or real tool execution."""
import json
import os
import sys
import threading

mode = sys.argv[1]
flag = '--resume' if '--resume' in sys.argv else '--session-id'
session = sys.argv[sys.argv.index(flag)+1]
current = None
count = 0
lock = threading.Lock()


def emit(value):
    with lock:
        print(json.dumps(value), flush=True)


def later(delay, action):
    timer = threading.Timer(delay, action)
    timer.daemon = True
    timer.start()


def result(user, interrupted=False, failed=False):
    value = dict(type='result', session_id=session, parent_tool_use_id=None,
                 uuid='result-'+user['uuid'], user_message_uuid=user['uuid'],
                 subtype='error_during_execution' if interrupted or failed else 'success', is_error=interrupted or failed,
                 terminal_reason='aborted_tools' if interrupted else 'api_error' if failed else 'completed')
    if interrupted or failed:
        value['errors'] = ['Request interrupted by user' if interrupted else 'Fixture provider failed']
    else:
        value['result'] = 'Claude completed normally'
    emit(value)


def ack(command, payload=None, error=False):
    value = dict(type='control_response', response=dict(subtype='error' if error else 'success', request_id=command['request_id']))
    if error:
        value['response']['error'] = 'Fixture control rejected'
    else:
        value['response']['response'] = payload or {}
    emit(value)


emit(dict(type='system', subtype='init', session_id=session, capabilities=['interrupt_receipt_v1', 'interrupt_cancel_queued_v1']))
for line in sys.stdin.buffer:
    command = json.loads(line)
    with open(os.environ['CCB_FIXTURE_TRACE'], 'a', encoding='utf-8') as trace:
        trace.write(json.dumps(command)+'\n')
    if command['type'] == 'user':
        count += 1
        current = command
        if count == 1 and mode in {'queue-cancel', 'queue-fallback'}:
            continue
        emit(command)
        emit(dict(type='assistant', session_id=session, parent_tool_use_id=None,
                  message={'content': [{'type': 'text', 'text': 'Partial Claude answer'}]}))
        if mode == 'exit-buffered-result':
            for _ in range(300):
                emit(dict(type='assistant', session_id=session, parent_tool_use_id=None,
                          message={'content': [{'type': 'text', 'text': 'Intermediate output'}]}))
            result(command)
            sys.exit(0)
        if count > 1:
            if mode in {'queue-cancel', 'queue-fallback'}:
                emit(dict(type='result', session_id=session, uuid='late-old-result', user_message_uuid='old-user-uuid',
                          subtype='success', is_error=False, result='WRONG OLD RESULT'))
            later(.3, lambda user=command: result(user))
    elif command['type'] == 'control_request':
        request = command['request']
        if request['subtype'] == 'interrupt':
            assert request['cancel_queued'] is True
            if mode == 'foreign-ack':
                emit(dict(type='control_response', response=dict(subtype='success', request_id='foreign', response={})))
            elif mode == 'denied':
                ack(command, error=True)
            elif mode == 'queue-cancel':
                ack(command, {'still_queued': [], 'cancelled': [current['uuid']]})
            elif mode == 'queue-fallback':
                ack(command, {'still_queued': [current['uuid']]})
            else:
                ack(command)
                if mode == 'foreign-result':
                    emit(dict(type='result', session_id=session, uuid='foreign-result', user_message_uuid='foreign-message',
                              subtype='success', is_error=False, terminal_reason='aborted_tools', result='foreign'))
                elif mode == 'exit':
                    sys.exit(2)
                elif mode != 'ack-only':
                    later(.3, lambda user=current: result(user, interrupted=mode != 'natural' and mode != 'failed', failed=mode == 'failed'))
        elif request['subtype'] == 'cancel_async_message':
            assert request['message_uuid'] == current['uuid']
            ack(command, {'cancelled': True})
