// Pi API adapter only. Rust owns admission, completion decisions and delivery.
import { appendFileSync, existsSync, lstatSync, realpathSync, renameSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';

const options = __CCB_PI_TERMINAL_OPTIONS__;

export default function (pi) {
  let generation = null;
  let active = null;
  let sequence = 0;
  const digest = text => createHash('sha256').update(String(text)).digest('hex');
  const anchor = text => {
    const first = String(text).split(/\r?\n/, 1)[0];
    return /^CCB_REQ_ID: ([A-Za-z0-9_.:-]{1,256})$/.exec(first)?.[1] || '';
  };
  const text = content => typeof content === 'string' ? content :
    Array.isArray(content) ? content.filter(p => p?.type === 'text' && typeof p.text === 'string').map(p => p.text).join('\n') : '';
  const canonicalAssistant = ctx => {
    const branch = ctx.sessionManager.getBranch();
    if (!Array.isArray(branch)) throw new Error('CCB Pi requires a canonical session branch');
    let assistant = null;
    // Other extensions may transform message_end after this listener runs.
    // Read the persisted in-memory branch at settlement, scoped to the last
    // exact request anchor. Never strip provider/extension-looking text blindly.
    for (let i = branch.length - 1; i >= 0; --i) {
      const entry = branch[i];
      if (entry?.type !== 'message') continue;
      const message = entry.message;
      if (message?.role === 'user') {
        return anchor(text(message.content)) === active.anchor ? { matched: true, assistant } : { matched: false };
      }
      if (!assistant && message?.role === 'assistant') {
        assistant = { text: text(message.content), stop_reason: message.stopReason,
          error: Boolean(message.errorMessage), canonical_entry_id: entry.id };
      }
    }
    return { matched: false };
  };
  const publish = (root, state) => {
    const current = join(root, 'current.json');
    try { if (lstatSync(current).isSymbolicLink()) throw new Error('CCB Pi state link refused'); }
    catch (e) { if (e.code !== 'ENOENT') throw e; }
    const temporary = join(root, `${randomUUID()}.state.tmp`);
    writeFileSync(temporary, JSON.stringify(state) + '\n', { encoding: 'utf8', flag: 'wx', mode: 0o600 });
    renameSync(temporary, current);
  };
  const emit = (type, payload = {}) => {
    if (!generation) return;
    const row = { type: 'pi_event', bridge: generation.bridge, seq: ++sequence,
      payload: { type, turn_id: active?.turn || null, ...payload } };
    const encoded = JSON.stringify(row) + '\n';
    if (Buffer.byteLength(encoded) > 4 * 1024 * 1024) throw new Error('CCB Pi event exceeds limit');
    appendFileSync(generation.path, encoded, { encoding: 'utf8' });
  };
  const rotate = ctx => {
    if (typeof ctx.sessionManager.getBranch !== 'function') throw new Error('CCB Pi requires SessionManager.getBranch');
    const root = resolve(options.event_root);
    if (realpathSync(root) !== root || lstatSync(root).isSymbolicLink()) throw new Error('CCB Pi event root changed');
    const id = String(ctx.sessionManager.getSessionId());
    const sessionPath = ctx.sessionManager.getSessionFile();
    if (!id || typeof sessionPath !== 'string' || !sessionPath) throw new Error('CCB Pi requires a persistent session');
    const entries = ctx.sessionManager.getEntries?.();
    const nativeHeader = ctx.sessionManager.getHeader?.();
    const nativeUnwrittenEmpty = !existsSync(sessionPath) && nativeHeader?.id === id && !nativeHeader.parentSession &&
      Array.isArray(entries) && !entries.some(e => e.type === 'compaction' || e.type === 'branch_summary' ||
        (e.type === 'message' && ['user', 'assistant'].includes(e.message?.role)));
    const instance = randomUUID();
    const path = join(root, `${instance}.jsonl`);
    const identity = JSON.parse(execFileSync(options.native, ['process-identity', String(process.pid)],
      { encoding: 'utf8', windowsHide: true, timeout: 5000 }));
    if (identity.pid !== process.pid) throw new Error('CCB Pi process identity mismatch');
    const bridge = { schema_version: 1, owner: options.owner, instance, process: identity,
      session_path: resolve(sessionPath) };
    const header = { type: 'session_meta', payload: { id, cwd: resolve(ctx.cwd), source: 'ccb_native_pi', ccb_bridge: bridge } };
    writeFileSync(path, JSON.stringify(header) + '\n', { encoding: 'utf8', flag: 'wx', mode: 0o600 });
    const state = { schema_version: 1, adapter_revision: 'canonical-session-branch-v1', provider: 'pi', ready: true, agent_name: options.agent_name,
      owner: options.owner, session_id: id, cwd: resolve(ctx.cwd), path, bridge, native_unwritten_empty: nativeUnwrittenEmpty };
    publish(root, state);
    generation = { path, bridge, root, state };
    active = null;
    sequence = 0;
  };
  pi.on('session_start', (_event, ctx) => rotate(ctx));
  pi.on('session_switch', (_event, ctx) => {
    emit('session_changed');
    rotate(ctx);
  });
  pi.on('session_shutdown', () => {
    emit('bridge_shutdown');
    if (generation) publish(generation.root, { ...generation.state, ready: false });
    generation = null;
    active = null;
  });
  pi.on('input', event => {
    emit('native_input_observed');
    if (active) { emit('request_superseded'); active = null; }
    return { action: 'continue' };
  });
  pi.on('before_agent_start', event => {
    const coordination = typeof options.coordination === 'string' && options.coordination
      ? { systemPrompt: `${event.systemPrompt || ''}\n${options.coordination}` } : undefined;
    const request = anchor(event.prompt);
    if (active && request === active.anchor && digest(event.prompt) === active.digest) return coordination;
    if (active) emit('request_superseded');
    active = request ? { anchor: request, digest: digest(event.prompt), turn: randomUUID(), assistant: null } : null;
    if (active) emit('request_start', { request_anchor: request });
    return coordination;
  });
  pi.on('agent_start', () => { emit('native_agent_activity'); if (active) emit('agent_start'); });
  pi.on('message_end', event => {
    if (!active) return;
    const message = event.message;
    if (message?.role === 'user') {
      const request = anchor(text(message.content));
      if (request !== active.anchor) { emit('request_superseded'); active = null; return; }
      emit('user_message', { request_anchor: request });
    } else if (message?.role === 'assistant') {
      active.assistant = { text: text(message.content), stop_reason: message.stopReason,
        error: Boolean(message.errorMessage) };
    }
  });
  // agent_end/turn_end do not prove settlement; Pi may retry or continue them.
  pi.on('agent_settled', (event, ctx) => {
    if (active) {
      if (!event.aborted) {
        const final = canonicalAssistant(ctx);
        if (!final.matched) { emit('request_superseded'); active = null; return; }
        active.assistant = final.assistant;
      }
      emit('agent_settled', { aborted: event.aborted, idle: ctx.isIdle(), assistant: active.assistant });
    }
    active = null;
  });
}
