// packages/durable/src/harness/tool.ts: the built-in tool task and its nested-call documents. The
// task's phases (resolution, validation, hooks, execution, settlement, recovery) run in the native
// scheduler of the hosted stock service.
import { defineDocFamily } from "pi-durable:internal/src/durable/documents";
import { nativeTask } from "pi-durable:internal/src/durable/harness/native-task";

/** One nested call of a running tool: its task, and once it ended, its result. */
export const NestedCallDoc = defineDocFamily({
  kind: "pi.tool.nested-call",
  version: 1,
  scope: "task",
  family: true,
  initial: () => ({}),
  // The document retires with its task.
  checkpointWhen: () => false,
});

/** Built-in tool task (`pi.tool`); its `call` and `execute` phases and `abort` run natively. */
export const ToolTask = nativeTask(
  {
    name: "pi.tool",
    version: 2,
    initial: () => ({ phase: "call" }),
    // Version 1 had only model-issued calls.
    migrate: (input, checkpoint) => ({ input: { kind: "model", ...input }, checkpoint }),
    phases: ["call", "execute"],
  },
  "tool",
);

/** An error result the Harness writes itself: no content and one `error` diagnostic with `code`. */
export function harnessError(code, message) {
  return { output: [], isError: true, diagnostics: [{ severity: "error", code, message }] };
}
