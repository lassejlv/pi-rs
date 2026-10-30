// pi-durable:internal/env-signals — abort signals of host environment calls (port of
// scripts/stock/durable-env-signals.mjs).
import { BACKGROUND_CONTEXT, withAbortSignal } from "@earendil-works/chord/context";
import { nativeTransport } from "pi-durable:internal/transport";

const controllers = new Map();
nativeTransport.event("environment.signal", (args) => controllers.get(args.signal)?.abort(new Error("Operation aborted")));

/** The context for one environment call, aborted when the native caller aborts it. */
export function environmentContext(args, parent = BACKGROUND_CONTEXT) {
  const controller = new AbortController();
  controllers.set(args.signal, controller);
  if (args.aborted) controller.abort(new Error("Operation aborted"));
  const signal = parent.abortSignal ? AbortSignal.any([parent.abortSignal, controller.signal]) : controller.signal;
  return { context: withAbortSignal(signal, parent), release: () => controllers.delete(args.signal) };
}
