// pi-durable:internal/chord/json — strict-JSON helpers of @earendil-works/chord (json.ts).

import { call } from "pi-durable:bridge";

// --- strict JSON (json.ts) ---
//
// What the host conversion cannot see (prototypes, descriptors, symbols, sparse or decorated
// arrays, cycles) is checked here; the copy itself is native (`json.copy`), which also rejects
// non-finite numbers and `undefined`.

/** Check structure; returns whether a null-prototype object occurs (the native copy loses them). */
function checkStructure(value, active, omitUndefined) {
  if (value === null || typeof value !== "object") {
    if (value === undefined || typeof value === "function" || typeof value === "symbol" || typeof value === "bigint") {
      throw new TypeError(`Value contains a non-JSON ${typeof value}; expected strict JSON`);
    }
    if (typeof value === "number" && !Number.isFinite(value)) {
      throw new TypeError("Value contains a non-finite number and is not strict JSON");
    }
    return false;
  }
  if (active.has(value)) throw new TypeError("Value contains cycles and is not strict JSON");
  active.add(value);
  try {
    let nullPrototype = false;
    if (Array.isArray(value)) {
      if (Object.getPrototypeOf(value) !== Array.prototype || Reflect.ownKeys(value).length !== value.length + 1) {
        throw new TypeError("Value must contain strict JSON dense plain arrays");
      }
      for (let index = 0; index < value.length; index++) {
        const descriptor = Object.getOwnPropertyDescriptor(value, index);
        if (descriptor === undefined || !descriptor.enumerable || !("value" in descriptor)) {
          throw new TypeError("Value must contain strict JSON enumerable indexed data properties");
        }
        if (checkStructure(descriptor.value, active, omitUndefined)) nullPrototype = true;
      }
      return nullPrototype;
    }
    const prototype = Object.getPrototypeOf(value);
    if (prototype !== Object.prototype && prototype !== null) {
      throw new TypeError("Value must contain strict JSON plain objects or arrays");
    }
    if (prototype === null) nullPrototype = true;
    for (const key of Reflect.ownKeys(value)) {
      if (typeof key === "symbol") throw new TypeError("Value contains a symbol key and is not strict JSON");
      const descriptor = Object.getOwnPropertyDescriptor(value, key);
      if (!descriptor.enumerable || !("value" in descriptor)) {
        throw new TypeError("Value must contain strict JSON enumerable data properties");
      }
      if (descriptor.value === undefined && omitUndefined) continue;
      if (checkStructure(descriptor.value, active, omitUndefined)) nullPrototype = true;
    }
    return nullPrototype;
  } finally {
    active.delete(value);
  }
}

const DATA = { value: undefined, writable: true, enumerable: true, configurable: true };

function defineData(target, key, value) {
  DATA.value = value;
  Object.defineProperty(target, key, DATA);
  DATA.value = undefined;
}

/** Give the native copy the source's null prototypes back (and own `__proto__` data keys). */
function restorePrototypes(source, copy) {
  if (source === null || typeof source !== "object") return copy;
  if (Array.isArray(source)) {
    for (let index = 0; index < copy.length; index++) copy[index] = restorePrototypes(source[index], copy[index]);
    return copy;
  }
  const target = Object.getPrototypeOf(source) === null ? Object.create(null) : copy;
  for (const key of Object.keys(copy)) {
    const child = restorePrototypes(Object.getOwnPropertyDescriptor(source, key).value, copy[key]);
    if (target !== copy || child !== copy[key]) defineData(target, key, child);
  }
  return target;
}

/** Copy a value into an alias-free strict-JSON tree owned by the caller. */
export function copyJson(value, options) {
  const omit = options?.omitUndefinedProperties === true;
  const nullPrototype = checkStructure(value, new Set(), omit);
  const copy = call("json.copy", value, omit);
  return nullPrototype ? restorePrototypes(value, copy) : copy;
}

/** Return whether a value is finite strict JSON with plain objects and no cycles. */
export function isJsonValue(value) {
  try {
    checkStructure(value, new Set(), false);
    return true;
  } catch {
    return false;
  }
}
