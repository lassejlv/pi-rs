// pi-durable:internal/chord/sqlite — `@earendil-works/chord/sqlite`: schema-prefix validation is
// native (pi_chord::sqlite, `sqlite.validateSchemaPrefix`); migrations run over any JS
// `SqliteDatabase` (a native-backed adapter or a caller's own), so they stay glue.

import { call } from "pi-durable:bridge";

/** Validate a prefix before using it as part of a SQLite identifier. */
export function validateSqliteSchemaPrefix(prefix) {
  call("sqlite.validateSchemaPrefix", String(prefix));
}

async function applySqliteMigrations(database, migrations, options) {
  const { table } = options;
  validateSqliteSchemaPrefix(table);
  for (let index = 0; index < migrations.length; index++) {
    if (migrations[index]?.version !== index + 1) {
      throw new Error("SQLite migrations must have contiguous versions starting at 1");
    }
  }
  await database.transaction(async (transaction) => {
    await transaction.exec(`CREATE TABLE IF NOT EXISTS ${table} (
			singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
			version INTEGER NOT NULL CHECK (version >= 0)
		) STRICT`);
    await transaction.run(`INSERT OR IGNORE INTO ${table} (singleton, version) VALUES (1, 0)`);
    const row = await transaction.get(`SELECT version FROM ${table} WHERE singleton = 1`);
    if (row === void 0) throw new Error("SQLite schema metadata is missing");
    const currentVersion = migrations.at(-1)?.version ?? 0;
    if (row.version > currentVersion) {
      throw new Error(`SQLite schema version ${row.version} is newer than supported version ${currentVersion}`);
    }
    for (const migration of migrations) {
      if (migration.version <= row.version) continue;
      for (const statement of migration.statements) await transaction.exec(statement);
      await transaction.run(`UPDATE ${table} SET version = ? WHERE singleton = 1`, migration.version);
    }
  });
}
export {
  applySqliteMigrations
};
