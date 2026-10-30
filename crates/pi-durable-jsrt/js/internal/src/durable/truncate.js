// packages/durable/src/truncate.ts (not an export entry): complete-line output bounds over
// pi_durable::truncate (src/dispatch/tools.rs `truncate.*`). No `Buffer` needed.
import { call } from "pi-durable:bridge";
import { utf8ByteLength } from "@earendil-works/chord/output";

export const DEFAULT_MAX_LINES = 2000;
export const DEFAULT_MAX_BYTES = 50 * 1024;

export { utf8ByteLength };

export function formatSize(bytes) {
  return call("truncate.formatSize", bytes);
}

export function truncateHead(content, options = {}) {
  return call("truncate.head", content, { maxLines: options.maxLines, maxBytes: options.maxBytes });
}

export function truncateHeadOf(prefix, totals, options = {}) {
  return call(
    "truncate.headOf",
    prefix,
    { lines: totals.lines, bytes: totals.bytes },
    { maxLines: options.maxLines, maxBytes: options.maxBytes },
  );
}
