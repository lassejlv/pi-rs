// @earendil-works/chord — the durable subset (EMBEDDED-PLAN.md §2): strict-JSON helpers, the
// Chord error class and watch handles. Host/contract/link/facet/replicated-state exports are not
// embedded.

import { ChordError } from "pi-durable:bridge";

export { ChordError };
export { NativeWatch, Watch } from "pi-durable:internal/src/chord/watch";

export const isChordError = (error, code) => error instanceof ChordError && (code === undefined || error.code === code);

/** For factories and methods refusing a principal: `throw forbidden()`. */
export const forbidden = (message = "forbidden") => new ChordError("forbidden", message);

export const invalid = (reason, message, details) => new ChordError("invalid", message, { reason, details });

export { copyJson, isJsonValue } from "pi-durable:internal/chord/json";
