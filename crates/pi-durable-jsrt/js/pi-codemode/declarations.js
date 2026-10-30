// @earendil-works/pi-codemode/declarations over pi_codemode::declarations::json
// (src/dispatch/codemode.rs `codemode.sdk.*`). Strings keep every UTF-16 code unit.

import { call } from "pi-durable:bridge";

const constants = call("codemode.sdk.constants");

export const DEFAULT_INPUT_SCHEMA_MAX_CHARS = constants.DEFAULT_INPUT_SCHEMA_MAX_CHARS;

/** The declaration fields of a `CodemodeTool` (its `execute` stays here). */
function declaration(tool) {
  return {
    name: tool.name,
    description: tool.description,
    inputSchema: tool.inputSchema,
    outputSchema: tool.outputSchema,
    signature: tool.signature,
  };
}

/** The identifier a script uses for a tool: invalid identifier characters become `_`. */
export function toCodemodeIdentifier(name) {
  return call("codemode.sdk.identifier", String(name));
}

/** TypeScript declarations for `tools` and the globals, one section each. */
export function renderDeclarations(options) {
  return call("codemode.sdk.renderDeclarations", {
    tools: options.tools?.map(declaration),
    globals: options.globals?.map(declaration),
  });
}

/** `name(args: Input): Promise<Output>;` for one tool. */
export function renderToolSignature(tool, options = {}) {
  return call("codemode.sdk.renderToolSignature", declaration(tool), options.inputMaxChars);
}

/** A tool's description followed by its code-mode declaration. */
export function renderToolSample(tool, options = {}) {
  return call("codemode.sdk.renderToolSample", declaration(tool), options.inputMaxChars);
}

/** The TypeScript type of a tool's output schema (`unknown` without one). */
export function renderToolOutputType(schema) {
  return call("codemode.sdk.renderToolOutputType", schema);
}

/** A JSON schema as a TypeScript type, bounded by `maxChars`. */
export function schemaToType(schema, options = {}) {
  return call("codemode.sdk.schemaToType", schema, options.maxChars);
}
