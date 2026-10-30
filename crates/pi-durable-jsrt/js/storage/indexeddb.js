// @earendil-works/pi-durable/storage/indexeddb: the native IndexedDbStorage
// (pi_durable::storage::indexeddb, src/dispatch/indexeddb.rs) over the caller's IndexedDB
// (`options.indexedDB`, else `globalThis.indexedDB`). This module only opens the database (with
// upstream's schema on first creation) and issues the requests the native transaction logic asks
// for; each request reports back from its own event handler, while its transaction is active.
import { call, ref } from "pi-durable:bridge";
import { storageMethods } from "pi-durable:internal/storage";

const schema = call("idb.schema");

function failure(error, fallback) {
  if (error === null || error === undefined) return { name: "Error", message: fallback };
  return { name: String(error.name ?? "Error"), message: String(error.message ?? fallback) };
}

function watch(request, slot) {
  request.onsuccess = () => call("idb.settle", slot, true, request.result);
  request.onerror = () => call("idb.settle", slot, false, failure(request.error, "IndexedDB request failed"));
}

function keyRange(range) {
  const KeyRange = globalThis.IDBKeyRange;
  const hasLower = range.lower !== undefined;
  const hasUpper = range.upper !== undefined;
  if (hasLower && hasUpper) return KeyRange.bound(range.lower, range.upper, range.lowerOpen, range.upperOpen);
  if (hasLower) return KeyRange.lowerBound(range.lower, range.lowerOpen);
  if (hasUpper) return KeyRange.upperBound(range.upper, range.upperOpen);
  return null;
}

function source(transaction, store, index) {
  const objectStore = transaction.objectStore(store);
  return index === undefined ? objectStore : objectStore.index(index);
}

/** The requests the native IdbBackend makes (src/dispatch/indexeddb.rs `JsIdbBackend`). */
const port = {
  transaction(database, stores, mode, done) {
    const transaction = database.transaction(stores, mode);
    let settled = false;
    const settle = (ok, value) => {
      if (settled) return;
      settled = true;
      call("idb.settle", done, ok, value);
    };
    transaction.oncomplete = () => settle(true);
    transaction.onabort = () => settle(false, failure(transaction.error, "IndexedDB transaction was aborted"));
    transaction.onerror = () => settle(false, failure(transaction.error, "IndexedDB transaction failed"));
    return transaction;
  },
  get(transaction, store, index, key, slot) {
    watch(source(transaction, store, index).get(key), slot);
  },
  add(transaction, store, row, slot) {
    watch(transaction.objectStore(store).add(row), slot);
  },
  put(transaction, store, row, slot) {
    watch(transaction.objectStore(store).put(row), slot);
  },
  delete(transaction, store, range, slot) {
    watch(transaction.objectStore(store).delete(keyRange(range)), slot);
  },
  values(transaction, store, index, range, direction, limit, filter, slot) {
    const request = source(transaction, store, index).openCursor(range === undefined ? null : keyRange(range), direction);
    const rows = [];
    request.onsuccess = () => {
      const cursor = request.result;
      if (cursor === null) {
        call("idb.settle", slot, true, rows);
        return;
      }
      const value = cursor.value;
      if (filter === undefined || call("idb.accept", filter, value)) rows.push(value);
      if (rows.length >= limit) call("idb.settle", slot, true, rows);
      else cursor.continue();
    };
    request.onerror = () => call("idb.settle", slot, false, failure(request.error, "IndexedDB cursor failed"));
  },
  abort(transaction) {
    try {
      transaction.abort();
    } catch {
      // Already committed or aborted: its outcome settles the waiter.
    }
  },
  close(database) {
    database.close();
  },
};

function factoryOf(options) {
  const factory = options.indexedDB ?? globalThis.indexedDB;
  if (factory === undefined) throw new Error("IndexedDB is not available");
  return factory;
}

function requestResult(request) {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

/** `createSchema()`: the upstream stores and indexes, with the initial metadata row. */
function createSchema(database) {
  for (const store of schema.stores) {
    const objectStore = database.createObjectStore(store.name, { keyPath: store.keyPath });
    for (const index of store.indexes) objectStore.createIndex(index.name, index.keyPath);
    if (store.name === schema.meta) objectStore.add(schema.initialMeta);
  }
}

const stores = new WeakMap();

/** Durable Storage in one IndexedDB database. */
export class IndexedDbStorage {
  static async open(name, options = {}) {
    const request = factoryOf(options).open(name, schema.version);
    request.addEventListener("upgradeneeded", (event) => {
      if (event.oldVersion === 0) createSchema(request.result);
    });
    const database = await requestResult(request);
    // The native storage checks the metadata row and closes the database when it is missing.
    const store = await call("idb.adopt", ref(port), database);
    const storage = new IndexedDbStorage();
    stores.set(storage, { store, closing: undefined });
    return storage;
  }

  /** Delete a closed database. */
  static async delete(name, options = {}) {
    await requestResult(factoryOf(options).deleteDatabase(name));
  }
}

storageMethods(IndexedDbStorage, (storage) => stores.get(storage));
