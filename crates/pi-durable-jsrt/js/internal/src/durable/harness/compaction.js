// packages/durable/src/harness/compaction.ts: `CompactionTask` (phases run natively) and the
// native cut selection and serialization (examples/support/compaction_bridge.rs).
import { nativeTransport } from "pi-durable:internal/transport";
import { nativeTask } from "pi-durable:internal/src/durable/harness/native-task";

export const CompactionTask = nativeTask(
  {
    name: "pi.compaction",
    version: 1,
    initial: () => ({ phase: "select" }),
    phases: ["select", "summarize", "retry"],
  },
  "compaction",
);

/** The newest cut of `view` that keeps `budget` recent tokens. */
export function selectCut(view, budget) {
  return nativeTransport.sync("compaction.select", { view: { ...view, head: view.head ?? null }, budget });
}

/** The transcript text a summary request carries. */
export function serializeConversation(messages) {
  return nativeTransport.sync("compaction.serialize", { messages });
}
