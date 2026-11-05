// @earendil-works/chord/delta — operations, the wire codec, `diffRevisions` and `track`.
export {
  apply,
  applyImmutable,
  applyImmutableBatches,
  assertSafePath,
  assertValidOp,
  assertValidWireOp,
  decoder,
  encoder,
  isBase,
  isReplace,
  overlap,
  PathError,
  RESERVED_SEGMENTS,
  UnsafePathError,
} from "pi-durable:internal/chord/delta/core";
export { diffRevisions } from "pi-durable:internal/chord/delta/diff";
export { track } from "pi-durable:internal/chord/delta/tracker";
