// Probe event order only; does not submit prompts or control Herdr.
const log = row => globalThis.__CCB_PI_PROFILE_EVENT?.(row);
log({event:'probe_module'});
export default function(pi) {
  log({event:'probe_factory'});
  pi.on('session_start', () => log({event:'session_start'}));
}
