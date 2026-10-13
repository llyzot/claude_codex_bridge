// Diagnostic preload: timing only, never headers, credentials, bodies or full URLs.
import { appendFileSync } from 'node:fs';
import { performance } from 'node:perf_hooks';
const started = performance.now();
const log = row => appendFileSync(process.env.CCB_PI_PROFILE_LOG, JSON.stringify({pid:process.pid,elapsed_ms:Math.round(performance.now()-started), ...row})+'\n');
globalThis.__CCB_PI_PROFILE_EVENT = log;
const original = globalThis.fetch;
let sequence = 0;
globalThis.fetch = async function(input, options) {
  const id = ++sequence;
  const start = performance.now();
  let category = 'other';
  try {
    const host = new URL(typeof input === 'string' || input instanceof URL ? input : input.url).hostname;
    category = host.endsWith('github.com') ? 'github' : host.endsWith('openai.com') ? 'openai' : host.endsWith('models.dev') ? 'model_catalog' : 'other';
  } catch {}
  log({event:'fetch_start',id,category});
  try {
    const result = await original.call(this,input,options);
    log({event:'fetch_end',id,category,status:result.status,ms:Math.round(performance.now()-start)});
    return result;
  } catch(error) {
    log({event:'fetch_error',id,category,name:error.name,ms:Math.round(performance.now()-start)});
    throw error;
  }
};
log({event:'process_start'});
