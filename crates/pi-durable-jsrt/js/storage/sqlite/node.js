// @earendil-works/pi-durable/storage/sqlite/node: Chord's Node driver (re-exported unwrapped, as
// upstream) and `openNodeSqliteStorage` over the in-process service's native SQLite store
// (pi-durable:internal/sqlite-store).
export { NodeSqliteDatabase, openNodeSqliteDatabase } from "@earendil-works/chord/sqlite/node";
export { openNodeSqliteStorage } from "pi-durable:internal/sqlite-store";
import { nativeSqliteHandle } from "pi-durable:internal/sqlite-store";
import { registerNativeStorage } from "pi-durable:internal/native-storage";

// The Harness hands an unmodified native store to the native service directly.
registerNativeStorage("sqlite", nativeSqliteHandle);
