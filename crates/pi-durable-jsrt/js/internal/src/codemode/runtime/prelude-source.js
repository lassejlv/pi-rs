// packages/codemode/src/runtime/prelude-source.ts: the VM prelude and its limits, from the native
// pi-codemode crate (the prelude its Boa VM runs).
import { call } from "pi-durable:bridge";

const constants = call("codemode.sdk.constants");

export const MAX_STORE_VALUE_CHARS = constants.MAX_STORE_VALUE_CHARS;
export const MAX_STORE_TOTAL_CHARS = constants.MAX_STORE_TOTAL_CHARS;
export const MAX_OUTPUT_CHARS = constants.MAX_OUTPUT_CHARS;
export const MAX_OUTPUT_ITEMS = constants.MAX_OUTPUT_ITEMS;
export const PRELUDE_SOURCE = call("codemode.sdk.prelude");
