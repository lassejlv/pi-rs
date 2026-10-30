// @earendil-works/pi-durable/storage/sqlite: the native SqliteStorage, schema and migrations
// (pi_durable::storage::sqlite) over a caller's own `SqliteDatabase` object (src/dispatch/storage.rs).
import { call, ref } from "pi-durable:bridge";
import { storageMethods } from "pi-durable:internal/storage";

const configuration = call("storage.sqliteConfig");

export const SQLITE_MIGRATIONS = configuration.migrations;
export const CURRENT_SQLITE_SCHEMA_VERSION = configuration.version;

/** Run one native transaction body inside the driver's own `transaction()`. */
function transact(database, state) {
  return database.transaction((executor) => call("storage.sqliteTransaction", state, ref(executor)));
}

/** Apply `migrations` (default: every durable migration) that the database has not applied yet. */
export async function applySqliteMigrations(database, migrations) {
  return call("storage.sqliteMigrate", ref(database), transact, migrations);
}

const stores = new WeakMap();

/** Durable Storage in SQLite tables, over any `SqliteDatabase` driver. */
export class SqliteStorage {
  static async open(database, options = {}) {
    const store = await call("storage.sqliteOpen", ref(database), transact, options.prefix);
    const storage = new SqliteStorage();
    stores.set(storage, { store, closing: undefined });
    return storage;
  }
}

storageMethods(SqliteStorage, (storage) => stores.get(storage));
