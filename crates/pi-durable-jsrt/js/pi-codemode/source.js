// @earendil-works/pi-codemode/source over pi_codemode::parse_codemode_source_utf16
// (src/dispatch/codemode.rs `codemode.sdk.parseSource`).

import { call } from "pi-durable:bridge";

const constants = call("codemode.sdk.constants");

export const CODEMODE_OPTIONS_PREFIX = constants.CODEMODE_OPTIONS_PREFIX;
export const CODEMODE_SOURCE_GRAMMAR = constants.CODEMODE_SOURCE_GRAMMAR;

export class CodemodeSourceError extends Error {
  constructor(message) {
    super(message);
    this.name = "CodemodeSourceError";
  }
}

/** Split an optional first-line `// @options: {...}` from the script. */
export function parseCodemodeSource(input) {
  const parsed = call("codemode.sdk.parseSource", input);
  if (parsed.error !== undefined) throw new CodemodeSourceError(parsed.error);
  return parsed;
}
