// packages/durable/src/harness/usage.ts: `UsageDoc` and the usage ledger helpers.
import { copyJson } from "@earendil-works/chord";
import { defineDoc } from "pi-durable:internal/src/durable/documents";

export const UsageDoc = defineDoc({
  kind: "pi.usage",
  version: 1,
  scope: "conversation",
  history: "latest",
  fork: "initial",
  initial: () => ({ models: {}, tools: {} }),
  checkpointWhen: () => true,
});

/** Add `usage` to one bucket of the conversation's `pi.usage`, in the commit that records the response. */
export async function recordUsage(tx, conversationId, bucket, key, usage) {
  const totals = (await tx.doc(UsageDoc, conversationId))[bucket];
  // Own keys only: a tool may be called `toString`.
  const total = Object.hasOwn(totals, key) ? totals[key] : undefined;
  // Providers may leave optional counters `undefined`; drafts take strict JSON.
  if (total === undefined) totals[key] = copyJson(usage, { omitUndefinedProperties: true });
  else addUsage(total, usage);
}

/** Add every counter of `usage` to `total`; optional counters are added once either side reports them. */
export function addUsage(total, usage) {
  total.input += usage.input;
  total.output += usage.output;
  total.cacheRead += usage.cacheRead;
  total.cacheWrite += usage.cacheWrite;
  total.totalTokens += usage.totalTokens;
  if (usage.cacheWrite1h !== undefined) total.cacheWrite1h = (total.cacheWrite1h ?? 0) + usage.cacheWrite1h;
  if (usage.reasoning !== undefined) total.reasoning = (total.reasoning ?? 0) + usage.reasoning;
  total.cost.input += usage.cost.input;
  total.cost.output += usage.cost.output;
  total.cost.cacheRead += usage.cost.cacheRead;
  total.cost.cacheWrite += usage.cost.cacheWrite;
  total.cost.total += usage.cost.total;
}

/** Add every bucket of `state` into `sum`. */
export function addUsageState(sum, state) {
  for (const bucket of ["models", "tools"]) {
    for (const [key, usage] of Object.entries(state[bucket])) {
      const total = Object.hasOwn(sum[bucket], key) ? sum[bucket][key] : undefined;
      // Define rather than assign: assigning a tool named `__proto__` would set the prototype.
      if (total !== undefined) addUsage(total, usage);
      else Object.defineProperty(sum[bucket], key, { value: structuredClone(usage), enumerable: true, writable: true });
    }
  }
}
