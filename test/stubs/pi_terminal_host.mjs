// Offline Pi extension host. Runs the actual provisioned adapter, not a copy.
import { createInterface } from 'node:readline';
import { pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';
import { appendFileSync, existsSync } from 'node:fs';
const config = JSON.parse(process.argv[2]);
if (config.start_gate) {
  const deadline = Date.now() + 30000;
  while (!existsSync(config.start_gate)) {
    if (Date.now() >= deadline) throw new Error('Pi fixture start gate expired');
    await new Promise(resolve => setTimeout(resolve, 25));
  }
}
const bridge = (await import(pathToFileURL(config.extension).href)).default;
let handlers;
let currentSession = config.session_id;
let branch = [];
// Prompts the bridge submitted through pi.sendUserMessage; the test then drives
// the turn and the host reports that input with Pi's `extension` source.
const submitted = [];
const sendUserMessage = text => {
  submitted.push(text);
  if (config.prompts_log) appendFileSync(config.prompts_log, JSON.stringify({ name: 'pi-extension', body: text }) + '\n');
};
let idle = true;
const ctx = { cwd: config.project, isIdle: () => idle,
  sessionManager: { getSessionId: () => currentSession, getSessionFile: () => config.native_session_path, getBranch: () => branch,
    getEntries: () => branch, getHeader: () => ({type:'session',id:currentSession,cwd:config.project}) } };
const emit = async (event, payload={}) => { let result; for (const handler of handlers.get(event) || []) { const value=await handler({type:event,...payload},ctx); if(value!==undefined) result=value; } return result; };
const load = async () => {
  handlers = new Map();
  const api={on:(event,handler)=>handlers.set(event,[...(handlers.get(event)||[]),handler])};
  if (!config.without_send) api.sendUserMessage=sendUserMessage;
  await bridge(api);
  await emit('session_start');
};
await load();
console.log(JSON.stringify({ready:true,pid:process.pid}));
for await (const line of createInterface({input:process.stdin})) {
  const command = JSON.parse(line);
  if (command.action === 'context') {
    const result=await emit('before_agent_start',{prompt:command.prompt||'interactive request',systemPrompt:'USER_SYSTEM_SENTINEL'});
    console.log(JSON.stringify({systemPrompt:result?.systemPrompt||'USER_SYSTEM_SENTINEL'}));continue;
  }
  if (command.action === 'busy') {idle=!command.busy;console.log(JSON.stringify({ok:true,action:command.action}));continue;}
  if (command.action === 'reload') {await emit('session_shutdown');await load();}
  else if (command.action === 'switch') {currentSession=command.session_id;if (command.session_path) {config.native_session_path=command.session_path;branch=[];}await emit('session_switch');}
  else if (command.action === 'event') {await emit(command.event,command.payload);}
  else if (command.action === 'prompt') {
    const prompt = command.prompt;
    const index = submitted.indexOf(prompt);
    if (index >= 0) submitted.splice(index, 1);
    await emit('input',{text:prompt,source:index >= 0 ? 'extension' : 'interactive'});
    await emit('before_agent_start',{prompt});
    await emit('agent_start');
    await emit('message_end',{message:{role:'user',content:[{type:'text',text:prompt}]}});
    branch.push({type:'message',id:`user-${branch.length}`,message:{role:'user',content:command.canonical_prompt ?? prompt}});
    let reply = command.reply || 'Pi terminal result';
    if (command.delegate) {
      const result = spawnSync(config.native,['--state-root',config.state_root,'ask','--from','lead','--chain','worker','delegate from terminal Pi'],{encoding:'utf8',windowsHide:true});
      if (result.status !== 0) throw new Error(result.stderr);
      reply='delegated';
    }
    await emit('message_end',{message:{role:'assistant',content:[{type:'text',text:reply}],stopReason:command.stop_reason||'stop',errorMessage:command.error||''}});
    branch.push({type:'message',id:`assistant-${branch.length}`,message:{role:'assistant',content:[{type:'text',text:command.canonical_reply ?? reply}],stopReason:command.stop_reason||'stop',errorMessage:command.error||''}});
    await emit('agent_end',{messages:[]});
    if (!command.hold) await emit('agent_settled',{aborted:command.aborted||false});
  }
  console.log(JSON.stringify({ok:true,action:command.action}));
}
