// packages/durable/src/session/transaction.ts as seen by tests: prototype interception only. The
// native Session performs adoption and discard after these observers (port of
// scripts/stock/durable-transaction-probes.mjs); an observer that throws injects that fault.
// The probes are on when the build has pi-durable's `stock-test-probes` feature
// (`transactionProbes`); a probed Session then calls even a plain MemoryStorage through its JS
// methods, so later instance spies on them apply.
import { call } from "pi-durable:bridge";
import { nativeTransport } from "pi-durable:internal/transport";

/** Whether Sessions report adoption and discard to `Transaction.prototype`. */
export const transactionProbes = call("session.transactionProbes");

export class Transaction {
  adopt(_seq) {}
  discard() {}
}

nativeTransport.register("session.transactionProbe", ({ method, seq }) => {
  const transaction = new Transaction();
  transaction[method](seq);
  return null;
});
