// pi-durable:internal/memory-storage — MemoryStorage (served publicly by storage/memory.js) backed by the native
// pi_durable::storage::MemoryStorage of the in-process service (examples/support/memory_bridge.rs).
// Port of scripts/stock/durable-memory-adapter.mjs without the upstream source class: every
// instance, subclasses included, owns one native store. A plain instance is shared with a native
// Session directly (`nativeMemoryHandle`); a subclass or an instance with own method overrides is
// called through its JS methods, which reach the native store through `super`.
import { nativeTransport } from "pi-durable:internal/transport";
import { objectFor } from "pi-durable:internal/handles";

const stores = new WeakMap();
const methods = [
  "commit",
  "mintId",
  "conversation",
  "scanConversations",
  "entry",
  "findLatestHeadMarker",
  "scanEntries",
  "task",
  "scanTasks",
  "submission",
  "scanSubmissions",
  "submissionByRequest",
  "findDocument",
  "document",
  "scanDocuments",
  "close",
];

function deepFreeze(value) {
  if (typeof value === "object" && value !== null && !Object.isFrozen(value)) {
    for (const child of Object.values(value)) deepFreeze(child);
    Object.freeze(value);
  }
  return value;
}

function store(storage) {
  const held = stores.get(storage);
  if (held === undefined) throw new TypeError("Not a MemoryStorage");
  return held;
}

/** In-memory Storage: committed state lives in the native store and ends with the process. */
export class MemoryStorage {
  constructor() {
    stores.set(this, { id: nativeTransport.sync("memory.new") });
  }

  /**
   * Validate `writes` as the commit at `seq` (default: the next) without applying it. The returned
   * writes are the store's resolved, frozen copy; `apply()` commits them once and returns `seq`.
   */
  prepareCommit(writes, seq) {
    const prepared = nativeTransport.sync("memory.prepare", { memory: store(this).id, writes, seq });
    return {
      seq: prepared.seq,
      writes: deepFreeze(prepared.writes),
      apply: () => nativeTransport.sync("memory.apply", { prepared: prepared.prepared }),
    };
  }

  /** Whether `close()` ran; tests reopen a store by setting it back to `false`. */
  get closed() {
    return nativeTransport.sync("memory.closed", { memory: store(this).id });
  }

  set closed(value) {
    nativeTransport.sync("memory.setClosed", { memory: store(this).id, closed: value === true });
  }
}

for (const method of methods) {
  Object.defineProperty(MemoryStorage.prototype, method, {
    configurable: true,
    writable: true,
    value: function (...args) {
      const held = store(this);
      args.pop(); // the call context: memory operations take none
      const adopt = method === "commit" ? args.splice(1, 1)[0] : undefined;
      // Native memory operations complete when called (as upstream's do): run them now and
      // settle in a microtask, after `adopt(seq)` for a commit.
      try {
        const { value, adopted } = nativeTransport.memoryCall({ memory: held.id, operation: method, args });
        if (adopted !== undefined) adopt(adopted);
        return Promise.resolve(value);
      } catch (error) {
        return Promise.reject(error);
      }
    },
  });
}

/** The native store a Session may use directly: a plain MemoryStorage without own method overrides. */
export function nativeMemoryHandle(storage) {
  const held = stores.get(storage);
  return held !== undefined &&
    Object.getPrototypeOf(storage) === MemoryStorage.prototype &&
    methods.every((method) => !Object.hasOwn(storage, method))
    ? held.id
    : undefined;
}

nativeTransport.register("memory.adopt", (args) => {
  objectFor(args.callback)(args.seq);
  return null;
});
