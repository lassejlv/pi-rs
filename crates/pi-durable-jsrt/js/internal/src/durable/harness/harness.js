// packages/durable/src/harness/harness.ts: `Harness` (the native service,
// pi-durable:internal/harness-service) and the module's other runtime exports.
export { Harness, DEFAULT_HARNESS_IDENTITY } from "pi-durable:internal/harness-service";

/** The property of a Harness that reports its scheduler index sizes (diagnostics for tests). */
export const SCHEDULER_INDEX_SIZES = Symbol.for("pi-durable.schedulerIndexSizes");

/** A clock that falls back to `Date.now` and reports once when `now` throws. */
export function safeNow(now, report) {
  if (now === undefined) return Date.now;
  let reported = false;
  return () => {
    try {
      return now();
    } catch (error) {
      // Set first: a report handler that reads the clock again must not report again.
      if (!reported) {
        reported = true;
        report(error, { type: "host_error", callback: "now" });
      }
      return Date.now();
    }
  };
}
