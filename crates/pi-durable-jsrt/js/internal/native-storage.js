// pi-durable:internal/native-storage — storages the native service can use directly. Each native
// storage module registers how to recognize its instances (an id, or `undefined` for a foreign or
// customized storage, which the service then calls through its JS methods).
const lookups = new Map();

/** Register `lookup(storage) -> id | undefined` for one native storage kind (`memory`, `sqlite`). */
export function registerNativeStorage(kind, lookup) {
  lookups.set(kind, lookup);
}

/** The native id of `storage` for `kind`, if it is an unmodified native storage of that kind. */
export function nativeStorageHandle(kind, storage) {
  return lookups.get(kind)?.(storage);
}
