// packages/durable/src/harness/context.ts: the exported helpers, native
// (pi_durable::harness::context).
import { call } from "pi-durable:bridge";

/** Tool results moved to follow the assistant message that called them. */
export function orderToolResults(messages) {
  return call("harness.orderToolResults", messages);
}
