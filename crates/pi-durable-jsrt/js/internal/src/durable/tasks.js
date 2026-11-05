// packages/durable/src/tasks.ts.

/** Define an executable task. Register it in the registry so a Harness can run tasks of its kind. */
export function defineTask(definition) {
  return { definition };
}
