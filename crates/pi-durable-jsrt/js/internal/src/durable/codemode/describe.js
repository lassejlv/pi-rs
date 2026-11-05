// packages/durable/src/codemode/describe.ts: port of the description/section part of
// scripts/stock/durable-codemode-adapter.mjs. Both render natively in the hosted stock service
// (`codemode.description`, `codemode.section`).

import { nativeTransport } from "pi-durable:internal/transport";
import { renderToolSample } from "@earendil-works/pi-codemode/declarations";

/** Default token budget of the tool declarations in the `codemode` section, as in the coding agent. */
export const DEFAULT_CODEMODE_INLINE_BUDGET = 3000;

const TEXT_OUTPUT_SCHEMA = { type: "string" };

/** The code mode tool's description: the script environment and its globals, the same for every conversation. */
export function codemodeDescription(models) {
  return nativeTransport.sync("codemode.description", { models });
}

/** What a script sees of a tool: its declaration. Tools without an output schema resolve to their text output. */
export function declarationOf(tool) {
  return {
    name: tool.name,
    description: tool.description,
    inputSchema: tool.parameters,
    outputSchema: tool.structuredOutputSchema ?? TEXT_OUTPUT_SCHEMA,
  };
}

/** `toolSample` of a tool: its description and TypeScript declaration. */
export function sampleOf(tool) {
  return renderToolSample(declarationOf(tool));
}

/** The namespace of a tool: the one with the longest prefix its name starts with. */
export function namespaceOf(name, namespaces) {
  let found;
  for (const namespace of namespaces) {
    if (name.startsWith(namespace.prefix) && (found === undefined || namespace.prefix.length > found.prefix.length)) {
      found = namespace;
    }
  }
  return found;
}

/** The `codemode` prompt section: the callable tools as declarations, by namespace, within `budget` tokens. */
export function codemodeSection(tools, namespaces, budget) {
  return nativeTransport.sync("codemode.section", {
    tools: tools.map((tool) => ({
      declaration: {
        name: tool.name,
        description: tool.description,
        parameters: tool.parameters,
        constrainedSampling: tool.constrainedSampling,
      },
      schema: tool.structuredOutputSchema,
    })),
    namespaces,
    budget,
  });
}
