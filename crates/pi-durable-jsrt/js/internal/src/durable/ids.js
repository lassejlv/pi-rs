// packages/durable/src/ids.ts: erased ID brands.

/** Apply an erased ID brand at a trusted numeric allocation or decoding boundary. */
export function idFromNumber(value) {
  return value;
}

/** Apply the erased commit-sequence brand at a trusted storage boundary. */
export function seqFromNumber(value) {
  return value;
}
