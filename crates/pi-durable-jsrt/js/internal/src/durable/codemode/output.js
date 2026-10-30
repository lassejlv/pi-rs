// packages/durable/src/codemode/output.ts: port of scripts/stock/durable-codemode-output-adapter.mjs.
// Formatting, truncation, UTF-16 budgets, spills and image saving are native; `write` stays JS.
import { nativeTransport } from "pi-durable:internal/transport";
import { handle, objectFor } from "pi-durable:internal/handles";

export const DEFAULT_MAX_OUTPUT_TOKENS = 10000;

nativeTransport.register("codemode.write", ({ writer, name, data }) =>
  objectFor(writer)(name, Array.isArray(data) ? Uint8Array.from(data) : data.text),
);

/** Format a code mode result for the model, writing its files with `options.write`. */
export function formatResult(result, options) {
  const { write, ...settings } = options;
  return nativeTransport.request("codemode.format", { result, options: settings, writer: handle(write) });
}
