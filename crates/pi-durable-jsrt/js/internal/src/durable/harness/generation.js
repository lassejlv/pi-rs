// packages/durable/src/harness/generation.ts: `GenerationTask` (phases run natively).
import { nativeTask } from "pi-durable:internal/src/durable/harness/native-task";

export const GenerationTask = nativeTask(
  {
    name: "pi.generation",
    version: 1,
    initial: () => ({ phase: "prepare", attempt: 1 }),
    phases: ["prepare", "request", "retry", "poll", "tools"],
  },
  "generation",
);
