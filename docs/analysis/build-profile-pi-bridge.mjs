import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
const target = process.argv[2];
mkdirSync(join(target, 'events'), {recursive:true});
mkdirSync(join(target, 'sessions'), {recursive:true});
const source = readFileSync('rust/crates/ccb-kernel/src/pi_terminal_bridge.mjs','utf8');
const options = {agent_name:'pi',event_root:join(target,'events'),native:'C:/Users/user/Desktop/work/ccb-rust/bin/ccb.exe',owner:randomUUID()};
writeFileSync(join(target,'terminal-bridge-impl.mjs'), source.replace('__CCB_PI_TERMINAL_OPTIONS__',JSON.stringify(options)));
writeFileSync(join(target,'terminal-bridge.mjs'), `
import { appendFileSync } from 'node:fs';
import { performance } from 'node:perf_hooks';
const start = performance.now();
const log = row => appendFileSync(process.env.CCB_PI_BRIDGE_PROFILE_LOG,JSON.stringify({elapsed_ms:Math.round(performance.now()-start),...row})+'\\n');
log({event:'bridge_module'});
export default async function(pi) {
  log({event:'bridge_factory_start'});
  const original = pi.on;
  pi.on = function(event,handler) {
    return original.call(this,event,(...args) => {
      if(event !== 'session_start') return handler(...args);
      const t = performance.now();
      log({event:'session_start_begin'});
      try { const result=handler(...args); log({event:'session_start_end',ms:Math.round(performance.now()-t)}); return result; }
      catch(error) {log({event:'session_start_error',name:error.name}); throw error;}
    });
  };
  try { const factory=(await import('./terminal-bridge-impl.mjs')).default; await factory(pi); }
  finally { pi.on=original; }
  log({event:'bridge_factory_end'});
}
`);
