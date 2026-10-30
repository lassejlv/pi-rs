// packages/durable/src/harness/telemetry.ts: `DURABLE_TELEMETRY_SCHEMA` (the native
// pi_durable::harness::telemetry schema).
import { call } from "pi-durable:bridge";

/** Durable's content-free span, event, and metric vocabulary. IDs address durable records, never content. */
export const DURABLE_TELEMETRY_SCHEMA = call("harness.telemetrySchema");
