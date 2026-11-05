// @earendil-works/pi-durable/storage/jsonl/node: JsonlStorage over a NodeExecutionEnv.
import { NodeExecutionEnv } from "@earendil-works/pi-durable/env/node";
import { JsonlStorage } from "@earendil-works/pi-durable/storage/jsonl";

export async function openNodeJsonlStorage(directory, context, options = {}) {
  return JsonlStorage.open(directory, new NodeExecutionEnv({ cwd: process.cwd() }), context, options);
}

export { JsonlStorage };
