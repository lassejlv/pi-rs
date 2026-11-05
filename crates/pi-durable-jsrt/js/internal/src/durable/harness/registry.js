// packages/durable/src/harness/registry.ts: `createRegistry` and `BUILTIN_TASKS` (native registry).
import { GenerationTask } from "pi-durable:internal/src/durable/harness/generation";
import { ToolTask } from "pi-durable:internal/src/durable/harness/tool";
import { CompactionTask } from "pi-durable:internal/src/durable/harness/compaction";
export { createRegistry } from "pi-durable:internal/registry";

export const BUILTIN_TASKS = [GenerationTask, ToolTask, CompactionTask];
