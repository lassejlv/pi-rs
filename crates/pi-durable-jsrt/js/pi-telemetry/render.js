// @earendil-works/pi-telemetry/render over pi_telemetry::render_trace (src/dispatch/telemetry.rs).
import { call } from "pi-durable:bridge";

/** Render stored spans as a chronological forest, treating absent parents as roots. */
export function renderTrace(spans) {
  return call("telemetry.renderTrace", [...spans]);
}
