// Pi extension API adapter only. Rust owns manifests, admission and completion.
import { readFileSync } from 'node:fs';

export default function (pi) {
  const readPlan = () => {
    const plan = JSON.parse(readFileSync(process.env.CCB_PI_ROLE_PLAN, 'utf8'));
    if (plan.schema_version !== 1 || plan.owner !== process.env.CCB_PI_ROLE_OWNER ||
        typeof plan.command !== 'string' || typeof plan.generation !== 'string' ||
        !Array.isArray(plan.roots) || !plan.roots.every(path => typeof path === 'string')) {
      throw new Error('Invalid native Pi role routing plan');
    }
    return plan;
  };
  const loaded = readPlan();
  pi.on('resources_discover', () => ({ skillPaths: loaded.roots }));
  pi.registerCommand(loaded.command, {
    description: `CCB native role bridge ${loaded.owner} ${loaded.generation}`,
    handler: async (args, ctx) => {
      const next = readPlan();
      if (next.command !== loaded.command || args.trim() !== next.generation) {
        throw new Error('Native Pi role generation mismatch');
      }
      await ctx.reload();
    },
  });
}
