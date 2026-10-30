// @earendil-works/pi-durable/storage/sqlite/cloudflare: SqliteStorage over a Durable Object's
// SQL storage, through Chord's DurableObjectSqliteDatabase driver.
import { DurableObjectSqliteDatabase } from "@earendil-works/chord/sqlite/cloudflare";
import { SqliteStorage } from "@earendil-works/pi-durable/storage/sqlite";

export { DurableObjectSqliteDatabase };

export function openDurableObjectSqliteStorage(storage, options = {}) {
  return SqliteStorage.open(new DurableObjectSqliteDatabase(storage), options);
}
