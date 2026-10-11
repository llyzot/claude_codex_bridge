"""Local-only deterministic Messages API fixture for real CLI protocol tests.

Never forwards requests or accepts real credentials. This is not a Claude model.
"""
import json
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Backend:
    def __init__(self,question=False):
        owner=self
        self.requests=[]
        self.responses=[]
        self.paths=[]
        class Handler(BaseHTTPRequestHandler):
            def log_message(self,*args):pass
            def send_bytes(self,status,body,kind='application/json'):
                self.send_response(status);self.send_header('Content-Type',kind)
                self.send_header('Content-Length',str(len(body)));self.send_header('Connection','close')
                self.end_headers();self.wfile.write(body)
            def do_POST(self):
                owner.paths.append(self.path.split('?')[0])
                if self.headers.get('x-api-key')!='local-test-key':
                    self.send_bytes(401,b'{"error":{"type":"authentication_error","message":"Fixture key required"}}');return
                length=int(self.headers.get('Content-Length','0'))
                if length>8*1024*1024:self.send_bytes(413,b'{}');return
                data=json.loads(self.rfile.read(length))
                if self.path.split('?')[0]=='/v1/messages/count_tokens':
                    self.send_bytes(200,b'{"input_tokens":4000}');return
                if self.path.split('?')[0]!='/v1/messages':
                    self.send_bytes(400,b'{"error":{"type":"invalid_request_error","message":"Unsupported fixture endpoint"}}');return
                text='LOCAL_CONTEXT_RESPONSE_'+uuid.uuid4().hex
                owner.requests.append(data);owner.responses.append(text)
                usage={'input_tokens':4000,'output_tokens':40,'cache_creation_input_tokens':0,'cache_read_input_tokens':0}
                message={'id':'msg_'+uuid.uuid4().hex,'type':'message','role':'assistant','model':data['model'],
                         'content':[{'type':'text','text':text}],'stop_reason':'end_turn','stop_sequence':None,'usage':usage}
                asking=question and len(owner.requests)==1
                if asking:
                    message['content']=[{'type':'tool_use','id':'question-fixture-1','name':'AskUserQuestion','input':{'questions':[
                        {'question':'Which format?','header':'Format','multiSelect':False,'options':[{'label':'Summary','description':'Short'},{'label':'Detailed','description':'Long'}]}]}}]
                    message['stop_reason']='tool_use'
                    if question=='multi':message['content'][0]['input']['questions'].append(
                        {'question':'Which sections?','header':'Sections','multiSelect':True,'options':[{'label':'Intro','description':'Opening'},{'label':'Summary','description':'Wrap-up'}]})
                if not data.get('stream'):
                    self.send_bytes(200,json.dumps(message).encode());return
                initial=dict(message,content=[],stop_reason=None,usage=dict(usage,output_tokens=0))
                events=[('message_start',{'type':'message_start','message':initial}),
                        ('content_block_start',{'type':'content_block_start','index':0,'content_block':{'type':'text','text':''}}),
                        ('content_block_delta',{'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':text}}),
                        ('content_block_stop',{'type':'content_block_stop','index':0}),
                        ('message_delta',{'type':'message_delta','delta':{'stop_reason':'end_turn','stop_sequence':None},'usage':{'output_tokens':40}}),
                        ('message_stop',{'type':'message_stop'})]
                if asking:
                    tool=message['content'][0]
                    events[1]=('content_block_start',{'type':'content_block_start','index':0,'content_block':{**tool,'input':{}}})
                    events[2]=('content_block_delta',{'type':'content_block_delta','index':0,'delta':{'type':'input_json_delta','partial_json':json.dumps(tool['input'])}})
                    events[4]=('message_delta',{'type':'message_delta','delta':{'stop_reason':'tool_use','stop_sequence':None},'usage':{'output_tokens':40}})
                body=''.join(f'event: {name}\ndata: {json.dumps(value)}\n\n' for name,value in events).encode()
                self.send_bytes(200,body,'text/event-stream')
        self.server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
        self.thread=threading.Thread(target=self.server.serve_forever,daemon=True)
        self.thread.start()
        self.url=f'http://127.0.0.1:{self.server.server_port}'
    def close(self):
        self.server.shutdown();self.server.server_close();self.thread.join(timeout=3)
