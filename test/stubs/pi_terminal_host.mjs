// Offline Pi extension host. Runs the actual provisioned adapter, not a copy.
import { createInterface } from 'node:readline';
import { pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';
const config = JSON.parse(process.argv[2]);
const bridge = (await import(pathToFileURL(config.extension).href)).default;
let handlers;
let currentSession = config.session_id;
const ctx = { cwd: config.project, isIdle: () => true,
  sessionManager: { getSessionId: () => currentSession, getSessionFile: () => config.native_session_path } };
const emit = async (event, payload={}) => { for (const handler of handlers.get(event) || []) await handler({type:event,...payload},ctx); };
const load = async () => {
  handlers = new Map();
  bridge({on:(event,handler)=>handlers.set(event,[...(handlers.get(event)||[]),handler])});
  await emit('session_start');
};
await load();
console.log(JSON.stringify({ready:true,pid:process.pid}));
for await (const line of createInterface({input:process.stdin})) {
  const command = JSON.parse(line);
  if (command.action === 'reload') {await emit('session_shutdown');await load();}
  else if (command.action === 'switch') {currentSession=command.session_id;await emit('session_switch');}
  else if (command.action === 'event') {await emit(command.event,command.payload);}
  else if (command.action === 'prompt') {
    const prompt = command.prompt;
    await emit('input',{text:prompt,source:'interactive'});
    await emit('before_agent_start',{prompt});
    await emit('agent_start');
    await emit('message_end',{message:{role:'user',content:[{type:'text',text:prompt}]}});
    let reply = command.reply || 'Pi terminal result';
    if (command.delegate) {
      const result = spawnSync(config.native,['--state-root',config.state_root,'ask','--from','lead','--chain','worker','delegate from terminal Pi'],{encoding:'utf8',windowsHide:true});
      if (result.status !== 0) throw new Error(result.stderr);
      reply='delegated';
    }
    await emit('message_end',{message:{role:'assistant',content:[{type:'text',text:reply}],stopReason:command.stop_reason||'stop',errorMessage:command.error||''}});
    await emit('agent_end',{messages:[]});
    if (!command.hold) await emit('agent_settled',{aborted:command.aborted||false});
  }
  console.log(JSON.stringify({ok:true,action:command.action}));
}
