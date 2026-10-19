// Pi API adapter only. Rust owns admission, completion decisions and delivery.
import { appendFileSync, existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, realpathSync, renameSync, statSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';

const options = __CCB_PI_TERMINAL_OPTIONS__;

export default function (pi) {
  let generation = null;
  let active = null;
  let sequence = 0;
  // Prompts this bridge submitted itself; their `input` event is not a person's.
  const own = new Set();
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
    const inbox = join(root, 'inbox');
    const canSend = typeof pi.sendUserMessage === 'function' && typeof ctx.isIdle === 'function';
    if (canSend) {
      mkdirSync(inbox, { recursive: true, mode: 0o700 });
      if (lstatSync(inbox).isSymbolicLink() || realpathSync(inbox) !== inbox) throw new Error('CCB Pi inbox changed');
    }
    const state = { schema_version: 1, adapter_revision: 'canonical-session-branch-v1', provider: 'pi', ready: true, agent_name: options.agent_name,
      owner: options.owner, session_id: id, cwd: resolve(ctx.cwd), path, bridge, native_unwritten_empty: nativeUnwrittenEmpty,
      ...(canSend ? { inbox_protocol: 1 } : {}) };
    publish(root, state);
    generation = { path, bridge, root, state, ctx, inbox: canSend ? inbox : null };
    active = null;
    sequence = 0;
  };
  // CCB writes `<instance>.<execution>.request.json`; the rename to
  // `.claimed.json` is the only claim and excludes Rust's withdrawal. A request
  // is submitted only while Pi is idle, so it always starts its own turn.
  const takeRequest = () => {
    const current = generation;
    if (!current?.inbox || !current.ctx.isIdle()) return;
    const prefix = `${current.bridge.instance}.`;
    const name = readdirSync(current.inbox).filter(n => n.startsWith(prefix) && n.endsWith('.request.json')).sort()[0];
    if (!name) return;
    const path = join(current.inbox, name);
    const stem = path.slice(0, -'.request.json'.length);
    let request;
    try {
      if (statSync(path).size > 1024 * 1024) throw new Error('request exceeds limit');
      request = JSON.parse(readFileSync(path, 'utf8'));
    } catch (error) {
      if (error.code === 'ENOENT') return;
      request = null;
    }
    const prompt = request?.prompt;
    if (request?.schema_version !== 1 || request.instance !== current.bridge.instance || typeof prompt !== 'string' ||
        !request.anchor || anchor(prompt) !== request.anchor) {
      try { renameSync(path, `${stem}.rejected.json`); } catch {}
      emit('inbox_rejected');
      return;
    }
    try { renameSync(path, `${stem}.claimed.json`); } catch { return; }
    own.add(digest(prompt));
    pi.sendUserMessage(prompt);
  };
  clearInterval(globalThis.__ccbPiInboxTimer);
  globalThis.__ccbPiInboxTimer = setInterval(() => {
    try { takeRequest(); } catch (error) { emit('inbox_failed', { error: String(error?.message || error) }); }
  }, 200);
  globalThis.__ccbPiInboxTimer.unref?.();
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
    const mine = event?.source === 'extension' && own.delete(digest(event.text));
    if (mine) return { action: 'continue' };
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
