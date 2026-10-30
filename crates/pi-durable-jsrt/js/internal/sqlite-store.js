// pi-durable:internal/sqlite-store — `openNodeSqliteStorage` for storage/sqlite/node.js.
// The opened storage is the native SQLite store of the in-process stock service
// (examples/support/memory_bridge.rs `sqlite.*`), shared directly with a native Harness. Port of
// scripts/stock/durable-task-sqlite-adapter.mjs: a store whose Storage methods are reassigned
// (decorated) keeps its JS methods on the Harness path, and the service routes the Harness's
// calls through them from then on.
import { nativeTransport } from "pi-durable:internal/transport";
import { temporaryHandle } from "pi-durable:internal/handles";
import "@earendil-works/pi-durable/storage/memory";
import { assertAcyclic } from "pi-durable:internal/storage";
import { SqliteStorage as PublicSqliteStorage } from "@earendil-works/pi-durable/storage/sqlite";

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

// An upstream `SqliteStorage` (instanceof and name), whose methods use the service's native store.
const NativeSqliteStorage = class SqliteStorage extends PublicSqliteStorage {};


for (const operation of methods) {
  Object.defineProperty(NativeSqliteStorage.prototype, operation, {
    configurable: true,
    writable: true,
    value: function (...args) {
      const store = stores.get(this);
      const context = args.pop();
      if (operation === "close" && store.closing) return store.closing;
      if (operation === "commit") {
        try {
          assertAcyclic(args[0]);
        } catch (error) {
          return Promise.reject(error);
        }
      }
      const adopt = operation === "commit" ? temporaryHandle(args.splice(1, 1)[0]) : undefined;
      const result = nativeTransport.request("sqlite.call", { sqlite: store.id, operation, args, adopt: adopt?.id }, context);
      const response = adopt ? result.finally(adopt.release) : result;
      response.catch(() => {});
      if (operation === "close") store.closing = response;
      return response;
    },
  });
}

/** Open (creating or migrating) durable SQLite storage in the file at `path`. */
export async function openNodeSqliteStorage(path, options = {}) {
  const id = await nativeTransport.request("sqlite.new", { path, options });
  const target = new NativeSqliteStorage();
  let decorated = false;
  const changed = (property) => {
    if (!decorated && methods.includes(property)) {
      decorated = true;
      nativeTransport.sync("sqlite.decorate", { sqlite: id });
    }
  };
  const storage = new Proxy(target, {
    set(target, property, value) {
      const result = Reflect.set(target, property, value);
      if (result) changed(property);
      return result;
    },
    defineProperty(target, property, descriptor) {
      const result = Reflect.defineProperty(target, property, descriptor);
      if (result) changed(property);
      return result;
    },
    deleteProperty(target, property) {
      const result = Reflect.deleteProperty(target, property);
      if (result) changed(property);
      return result;
    },
  });
  stores.set(storage, { id });
  return storage;
}

/** The service store a native Harness may use directly: an undecorated `openNodeSqliteStorage` result. */
export function nativeSqliteHandle(storage) {
  const store = stores.get(storage);
  return store !== undefined &&
    Object.getPrototypeOf(storage) === NativeSqliteStorage.prototype &&
    methods.every((method) => !Object.hasOwn(storage, method))
    ? store.id
    : undefined;
}
