// pi-durable:internal/chord/delta/trusted — copy-on-write application of the tracker's own
// (trusted, already validated) operations; port of upstream apply-immutable-trusted.ts.

const DATA_DESCRIPTOR = {
  value: void 0,
  writable: true,
  enumerable: true,
  configurable: true
};
function applyImmutableTrusted(target, operations) {
  let root = target;
  const owned = new WeakSet();
  for (const operation of operations) {
    if (operation[0] === "r") {
      root = operation[1];
      continue;
    }
    if (operation[0] === "p" || operation[0] === "m") {
      const copied2 = copyPath(root, operation[1], operation[1].length, owned);
      root = copied2.root;
      if (!Array.isArray(copied2.target)) throw new TypeError("Trusted array operation target is not an array");
      if (operation[0] === "p") spliceTrusted(copied2.target, operation[2], operation[3], operation[4]);
      else permuteTrusted(copied2.target, operation[2]);
      continue;
    }
    const path = operation[1];
    const copied = copyPath(root, path, path.length - 1, owned);
    root = copied.root;
    const key = path[path.length - 1];
    switch (operation[0]) {
      case "s":
        defineData(copied.target, key, operation[2]);
        break;
      case "d":
        if (Array.isArray(copied.target)) {
          if (typeof key !== "number") throw new TypeError("Trusted array deletion key is not numeric");
          spliceTrusted(copied.target, key, 1, []);
        } else Reflect.deleteProperty(copied.target, key);
        break;
      case "a": {
        const current = read(copied.target, key);
        if (typeof current !== "string") throw new TypeError("Trusted append target is not a string");
        defineData(copied.target, key, `${current}${operation[2]}`);
        break;
      }
      case "t": {
        const current = read(copied.target, key);
        if (typeof current !== "string") throw new TypeError("Trusted truncate target is not a string");
        defineData(copied.target, key, current.slice(operation[2]));
        break;
      }
    }
  }
  return root;
}
function copyPath(root, path, length, owned) {
  if (!isContainer(root)) throw new TypeError("Trusted operation root is not a container");
  let copiedRoot = root;
  if (!owned.has(root)) {
    copiedRoot = shallowCopy(root);
    owned.add(copiedRoot);
  }
  let destination = copiedRoot;
  for (let index = 0; index < length; index++) {
    const segment = path[index];
    const child = read(destination, segment);
    if (!isContainer(child)) throw new TypeError("Trusted operation path is not a container");
    if (owned.has(child)) {
      destination = child;
      continue;
    }
    const copy = shallowCopy(child);
    defineData(destination, segment, copy);
    owned.add(copy);
    destination = copy;
  }
  return { root: copiedRoot, target: destination };
}
function shallowCopy(value) {
  if (Array.isArray(value)) return Array.from(value);
  const result = Object.create(Object.getPrototypeOf(value) === null ? null : Object.prototype);
  for (const key of Object.keys(value)) defineData(result, key, value[key]);
  return result;
}
function read(target, key) {
  return target[key];
}
function defineData(target, key, value) {
  DATA_DESCRIPTOR.value = value;
  Object.defineProperty(target, key, DATA_DESCRIPTOR);
  DATA_DESCRIPTOR.value = void 0;
}
function spliceTrusted(target, start, remove, items) {
  const oldLength = target.length;
  const delta = items.length - remove;
  if (delta > 0) {
    target.length = oldLength + delta;
    target.copyWithin(start + items.length, start + remove, oldLength);
  } else if (delta < 0) {
    target.copyWithin(start + items.length, start + remove, oldLength);
    target.length = oldLength + delta;
  }
  for (let index = 0; index < items.length; index++) defineData(target, start + index, items[index]);
}
function permuteTrusted(target, permutation) {
  if (target.length !== permutation.length) throw new TypeError("Trusted permutation length mismatch");
  const previous = target.slice();
  for (let index = 0; index < permutation.length; index++) target[index] = previous[permutation[index]];
}
function isContainer(value) {
  return value !== null && typeof value === "object";
}
export {
  applyImmutableTrusted
};
