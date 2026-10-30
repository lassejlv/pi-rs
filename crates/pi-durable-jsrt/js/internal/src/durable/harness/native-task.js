// Shells of the built-in tasks: their definitions (name, version, initial, migrate, phase names)
// as upstream declares them; every phase runs in the native scheduler, never here.
import { defineTask } from "pi-durable:internal/src/durable/tasks";
import { markNativeCompaction, markNativeGeneration, markNativeTool } from "pi-durable:internal/handles";

const native = (name) => () => {
  throw new Error(`${name} runs in the native scheduler`);
};

/** A built-in task whose `phases` (and `abort`) run natively. */
export function nativeTask({ name, version, initial, migrate, phases }, kind) {
  const definition = { name, version, initial, ...(migrate === undefined ? {} : { migrate }), phases: {}, abort: native(`${name} abort`) };
  for (const phase of phases) definition.phases[phase] = native(`${name} phase ${phase}`);
  const task = defineTask(definition);
  ({ generation: markNativeGeneration, tool: markNativeTool, compaction: markNativeCompaction })[kind](task);
  return task;
}
