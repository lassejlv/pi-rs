// @earendil-works/pi-durable/tool-search: port of scripts/stock/durable-search-adapter.mjs. The
// registration describes the native `tool_search` (BM25 over deferred tool metadata, validation,
// output and persisted loads), which the native tool task executes.
import { nativeTransport } from "pi-durable:internal/transport";

export const TOOL_SEARCH_TOOL_NAME = "tool_search";
export const DEFAULT_TOOL_SEARCH_LIMIT = 8;

export function createToolSearchTool() {
  const declaration = nativeTransport.sync("builtin.describe", { tool: "tool_search" });
  return {
    ...declaration,
    callers: ["model"],
    replay: "safe",
    __piNativeBuiltin: "tool_search",
    execute() {
      throw new Error("Native tool search was bypassed");
    },
  };
}

/** The `tool_search` extension: install it and select it where the model should find deferred tools. */
export function createToolSearchExtension() {
  return { name: "tool-search", tools: [createToolSearchTool()] };
}
