// Offline diagnostic: load existing resources, without starting an agent or emitting session_start.
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { performance } from 'node:perf_hooks';
const started = performance.now();
process.env.PI_OFFLINE = '1';
process.env.PI_TIMING = '1';
delete process.env.HERDR_ENV;
delete process.env.HERDR_SESSION;
const output = value => process.stdout.write(JSON.stringify(value) + '\n');
console.log = () => {};
console.warn = () => {};
const root = 'C:/Users/user/scoop/apps/nodejs-lts/current/bin/node_modules/@earendil-works/pi-coding-agent';
const { DefaultResourceLoader, SettingsManager } = await import(pathToFileURL(root + '/dist/index.js'));
output({ stage: 'sdk_import', ms: Math.round(performance.now() - started) });
const cwd = 'C:/Users/user/Desktop/work/1.code/5.Tool/sample_d9_hmi';
const agentDir = 'C:/Users/user/.pi/agent';
const global = JSON.parse(readFileSync(agentDir + '/settings.json', 'utf8'));
const project = JSON.parse(readFileSync(cwd + '/.pi/settings.json', 'utf8'));
const settingsManager = SettingsManager.inMemory({
  packages: global.packages,
  extensions: [...(global.extensions || []), ...(project.extensions || []).map(p => p.startsWith('.') ? cwd + '/.pi/' + p : p)],
});
const loader = new DefaultResourceLoader({ cwd, agentDir, settingsManager });
const label = path => path.replaceAll('\\', '/').replace(cwd, '<project>').replace(agentDir, '<pi-agent>').replace(root, '<pi-runtime>');
for (const [object, methods] of [[loader, ['loadFinalExtensionSet', 'loadExtensionPaths', 'loadCurrentExtensionSet']], [loader.packageManager, ['resolve', 'resolvePackageSources', 'collectPackageResources']]]) {
  for (const name of methods) {
    const original = object[name];
    if (typeof original !== 'function') continue;
    object[name] = function(...args) {
      const start = performance.now();
      const finish = () => output({ stage: name, ms: Math.round(performance.now() - start) });
      const result = original.apply(this, args);
      if (result?.then) return result.finally(finish);
      finish(); return result;
    };
  }
}
const reload = performance.now();
try {
  await loader.reload();
  const result = loader.getExtensions();
  output({ stage: 'resource_reload', ms: Math.round(performance.now() - reload), extensions: result.extensions.map(e => ({path:label(e.path), session_start_handlers:e.handlers.get('session_start')?.length || 0})), errors: result.errors.map(e => ({path:label(e.path), error:e.error})) });
  const { printTimings } = await import(pathToFileURL(root + '/dist/core/timings.js'));
  printTimings();
  process.exit(0);
} catch (error) { output({ stage: 'failure', name: error.name }); process.exit(1); }
