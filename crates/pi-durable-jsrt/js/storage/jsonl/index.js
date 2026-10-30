// @earendil-works/pi-durable/storage/jsonl: the native JsonlStorage (pi_durable::storage::jsonl)
// over the caller's own ExecutionEnv filesystem object (src/dispatch/storage.rs).
import { call, ref, JsonlCorruptionError, JsonlStoragePoisonedError } from "pi-durable:bridge";
import { storageMethods } from "pi-durable:internal/storage";

export { JsonlCorruptionError, JsonlStoragePoisonedError };

const stores = new WeakMap();

const fsMethods = [
  "absolutePath",
  "joinPath",
  "readBinaryFile",
  "writeFile",
  "appendFile",
  "truncateFile",
  "flushFile",
  "renameFile",
  "listDir",
  "createDir",
  "remove",
];

/** The caller's filesystem with each `Result` as plain data for Rust (src/dispatch/storage.rs). */
function port(fs) {
  const plain = {};
  for (const method of fsMethods) {
    plain[method] = async (...args) => {
      const result = await fs[method](...args);
      if (result.ok) {
        const value = method === "listDir" ? result.value.map((info) => ({ ...info })) : result.value;
        return { ok: true, value };
      }
      const error = result.error;
      return { ok: false, error: { code: error?.code, message: error?.message, path: error?.path } };
    };
  }
  return plain;
}

/** Durable Storage as append-only JSON Lines files in one directory. */
export class JsonlStorage {
  static async open(directory, fs, context, options = {}) {
    const store = await call(
      "storage.jsonlOpen",
      directory,
      ref(port(fs)),
      context,
      fs.cwd,
      fs.id,
      options.fsync === true,
    );
    const storage = new JsonlStorage();
    stores.set(storage, { store, closing: undefined });
    return storage;
  }
}

storageMethods(JsonlStorage, (storage) => stores.get(storage));
