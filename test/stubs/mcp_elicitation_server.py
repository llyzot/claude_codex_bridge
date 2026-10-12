"""Local-only MCP fixture: one tool elicits a fixed choice; no filesystem tools."""
import json
from pathlib import Path
import sys

trace = Path(sys.argv[1])
pending = {}


def emit(value):
    print(json.dumps(value), flush=True)


for line in sys.stdin:
    message = json.loads(line)
    with trace.open('a', encoding='utf-8') as log:
        log.write(json.dumps(message) + '\n')
    method = message.get('method')
    if method == 'initialize':
        emit({'jsonrpc':'2.0','id':message['id'],'result':{'protocolVersion':message['params']['protocolVersion'],
              'capabilities':{'tools':{}},'serverInfo':{'name':'ccb-elicitation-fixture','version':'1.0'}}})
    elif method == 'tools/list':
        emit({'jsonrpc':'2.0','id':message['id'],'result':{'tools':[{'name':'choose_value',
              'description':'Ask the human to select a test value; return the selected value.',
              'inputSchema':{'type':'object','properties':{},'additionalProperties':False}}]}})
    elif method == 'tools/call':
        assert message['params']['name'] == 'choose_value'
        request_id = 'form-' + str(message['id'])
        pending[request_id] = message['id']
        emit({'jsonrpc':'2.0','id':request_id,'method':'elicitation/create','params':{
              'mode':'form','message':'CCB local acceptance fixture: select alpha.',
              'requestedSchema':{'type':'object','properties':{'choice':{'type':'string','enum':['alpha','beta']}},'required':['choice']}}})
    elif method == 'ping':
        emit({'jsonrpc':'2.0','id':message['id'],'result':{}})
    elif method is None and message.get('id') in pending:
        tool_id = pending.pop(message['id'])
        result = message.get('result', {})
        text = result.get('content', {}).get('choice') if result.get('action') == 'accept' else 'not accepted'
        emit({'jsonrpc':'2.0','id':tool_id,'result':{'content':[{'type':'text','text':str(text)}]}})
