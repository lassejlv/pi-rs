// pi-durable:internal/storage — the `Storage` methods of a JS class over a native store
// (src/dispatch/storage.rs `storage.call`). Each method takes its upstream arguments and a
// trailing context; `commit` hands its `adopt` callback to Rust, which calls it before the
// commit settles. A repeated `close()` returns the first close's promise.
import { call } from "pi-durable:bridge";

const arities = {
  mintId: 0,
  conversation: 1,
  scanConversations: 3,
  findLatestHeadMarker: 2,
  scanEntries: 3,
  task: 1,
  scanTasks: 3,
  submission: 1,
  scanSubmissions: 3,
  submissionByRequest: 2,
  findDocument: 2,
  document: 2,
  scanDocuments: 3,
};

/** Upstream's SQLite and JSONL storages serialize writes with `JSON.stringify`; keep its cycle error. */
export function assertAcyclic(value, seen = []) {
  if (typeof value !== "object" || value === null) return;
  if (seen.includes(value)) throw new TypeError("Converting circular structure to JSON");
  seen.push(value);
  for (const child of Object.values(value)) assertAcyclic(child, seen);
  seen.pop();
}

function invoke(held, method, args, adopt) {
  if (held === undefined) return Promise.reject(new TypeError("Not an open native storage"));
  try {
    return call("storage.call", held.store, method, args, adopt);
  } catch (error) {
    return Promise.reject(error);
  }
}

/** Define the Storage methods on `Class.prototype`; `held(storage)` returns `{ store, closing }`. */
export function storageMethods(Class, held) {
  const define = (name, value) =>
    Object.defineProperty(Class.prototype, name, { configurable: true, writable: true, value });
  define("commit", function (writes, adopt, _context) {
    try {
      assertAcyclic(writes);
    } catch (error) {
      return Promise.reject(error);
    }
    return invoke(held(this), "commit", [writes], adopt);
  });
  define("entry", function (...args) {
    return invoke(held(this), "entry", args.length > 2 ? args.slice(0, 2) : args.slice(0, -1));
  });
  define("close", function (_context) {
    const state = held(this);
    if (state === undefined) return Promise.reject(new TypeError("Not an open native storage"));
    return (state.closing ??= invoke(state, "close", []));
  });
  for (const [method, arity] of Object.entries(arities)) {
    define(method, function (...args) {
      return invoke(held(this), method, args.slice(0, arity));
    });
  }
}
