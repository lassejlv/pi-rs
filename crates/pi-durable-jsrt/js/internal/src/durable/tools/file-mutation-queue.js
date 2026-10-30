// packages/durable/src/tools/file-mutation-queue.ts (not an export entry): mutations of one file in
// one environment run one at a time, in order (pi_durable::tools::file_mutation_queue,
// src/dispatch/tools.rs `builtin.mutation`).
import { callAsync, ref } from "pi-durable:bridge";
import { nativeContext } from "pi-durable:internal/chord/context";
import { describeEnv } from "pi-durable:internal/env/port";

export function withFileMutationQueue(env, path, fn, context) {
  return callAsync("builtin.mutation", describeEnv(env), path, ref(fn), nativeContext(context));
}
