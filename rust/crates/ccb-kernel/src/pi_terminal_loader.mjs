// Keep this entry stable: Pi/Jiti may retain native .mjs modules across reload.
// The factory runs again on each reload and selects the current implementation.
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';

export default async function (pi) {
  const implementation = new URL('./terminal-bridge-impl.mjs', import.meta.url);
  const source = readFileSync(implementation);
  implementation.searchParams.set('ccb_source', createHash('sha256').update(source).digest('hex'));
  const factory = (await import(implementation.href)).default;
  await factory(pi);
}
