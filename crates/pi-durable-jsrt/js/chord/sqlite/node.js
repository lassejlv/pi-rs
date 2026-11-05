// @earendil-works/chord/sqlite/node — the `SqliteDatabase` adapter over `node:sqlite`, which pi-jsrt
// implements natively over rusqlite. Port of upstream src/sqlite/node.ts (types stripped): the
// adapter wraps a caller's `DatabaseSync` (`new NodeSqliteDatabase(connection)`), so it is JS.

import { mkdir } from "node:fs/promises";
import { dirname } from "node:path";
import { DatabaseSync } from "node:sqlite";
const DEFAULT_WAL_AUTO_CHECKPOINT_PAGES = 1e3;
const DEFAULT_BUSY_TIMEOUT_MS = 5e3;
const ignore = () => {
};
class SerialOperationQueue {
  tail = Promise.resolve();
  pending = 0;
  run(operation) {
    if (this.pending > 0) return this.enqueue(operation);
    try {
      return Promise.resolve(operation());
    } catch (error) {
      return Promise.reject(error);
    }
  }
  runAsync(operation) {
    if (this.pending > 0) return this.enqueue(operation);
    this.pending++;
    const { promise: barrier, resolve: releaseBarrier } = Promise.withResolvers();
    this.tail = barrier;
    let started;
    try {
      started = operation();
    } catch (error) {
      started = Promise.reject(error);
    }
    return started.finally(() => {
      this.pending--;
      releaseBarrier();
    });
  }
  enqueue(operation) {
    this.pending++;
    return this.release(this.tail.then(operation));
  }
  release(operation) {
    const settled = operation.finally(() => {
      this.pending--;
    });
    this.tail = settled.then(ignore, ignore);
    return settled;
  }
}
class NodeSqliteExecutor {
  database;
  statements;
  constructor(database, statements) {
    this.database = database;
    this.statements = statements;
  }
  exec(sql) {
    return this.runOperation(() => {
      this.database.exec(sql);
    });
  }
  run(sql, ...params) {
    return this.runOperation(() => {
      this.statement(sql).run(...params);
    });
  }
  get(sql, ...params) {
    return this.runOperation(() => this.statement(sql).get(...params));
  }
  all(sql, ...params) {
    return this.runOperation(() => this.statement(sql).all(...params));
  }
  statement(sql) {
    let statement = this.statements.get(sql);
    if (statement === void 0) {
      statement = this.database.prepare(sql);
      this.statements.set(sql, statement);
    }
    return statement;
  }
}
class NodeSqliteTransaction extends NodeSqliteExecutor {
  scope;
  constructor(database, statements, scope) {
    super(database, statements);
    this.scope = scope;
  }
  async runOperation(operation) {
    if (!this.scope.active) throw new Error("SQLite transaction handle is no longer active");
    return operation();
  }
}
class NodeSqliteDatabase extends NodeSqliteExecutor {
  access = new SerialOperationQueue();
  closed = false;
  constructor(database) {
    super(database, new Map());
  }
  transaction(callback) {
    return this.access.runAsync(async () => {
      this.database.exec("BEGIN IMMEDIATE");
      const scope = { active: true };
      try {
        const result = await callback(new NodeSqliteTransaction(this.database, this.statements, scope));
        scope.active = false;
        this.database.exec("COMMIT");
        return result;
      } catch (error) {
        scope.active = false;
        try {
          this.database.exec("ROLLBACK");
        } catch (rollbackError) {
          throw new AggregateError([error, rollbackError], "SQLite transaction failed and rollback failed");
        }
        throw error;
      }
    });
  }
  close() {
    return this.access.run(() => {
      if (this.closed) return;
      this.closed = true;
      this.statements.clear();
      try {
        this.database.exec("PRAGMA wal_checkpoint(TRUNCATE)");
      } finally {
        this.database.close();
      }
    });
  }
  runOperation(operation) {
    return this.access.run(operation);
  }
}
async function openNodeSqliteDatabase(path, options = {}) {
  const checkpointPages = options.walAutoCheckpointPages ?? DEFAULT_WAL_AUTO_CHECKPOINT_PAGES;
  const timeout = options.busyTimeoutMs ?? DEFAULT_BUSY_TIMEOUT_MS;
  if (path !== ":memory:") await mkdir(dirname(path), { recursive: true });
  const database = new DatabaseSync(path, { timeout });
  const adapter = new NodeSqliteDatabase(database);
  try {
    await adapter.exec("PRAGMA journal_mode = WAL");
    await adapter.exec("PRAGMA synchronous = NORMAL");
    await adapter.exec(`PRAGMA wal_autocheckpoint = ${checkpointPages}`);
    return adapter;
  } catch (error) {
    try {
      await adapter.close();
    } catch {
    }
    throw error;
  }
}
export {
  NodeSqliteDatabase,
  openNodeSqliteDatabase
};
