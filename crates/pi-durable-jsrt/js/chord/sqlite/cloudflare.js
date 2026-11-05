// @earendil-works/chord/sqlite/cloudflare — the `SqliteDatabase` adapter over a Durable Object's
// `ctx.storage` (a caller's JS object). Port of upstream src/sqlite/cloudflare.ts (types stripped).

class SerialQueue {
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
  hold(operation) {
    return this.enqueue(operation);
  }
  enqueue(operation) {
    this.pending++;
    const result = this.tail.then(operation).finally(() => {
      this.pending--;
    });
    this.tail = result.then(
      () => {
      },
      () => {
      }
    );
    return result;
  }
}
const storageQueues = new WeakMap();
function queueFor(storage) {
  let queue = storageQueues.get(storage);
  if (queue === void 0) {
    queue = new SerialQueue();
    storageQueues.set(storage, queue);
  }
  return queue;
}
function bindValue(value) {
  if (value instanceof Uint8Array) return value.slice().buffer;
  if (typeof value !== "bigint") return value;
  if (value < BigInt(Number.MIN_SAFE_INTEGER) || value > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new RangeError(`Durable Object SQL cannot bind ${value} without losing precision`);
  }
  return Number(value);
}
const bind = (params) => params.map(bindValue);
function row(row2) {
  for (const key in row2) {
    const value = row2[key];
    if (value instanceof ArrayBuffer) row2[key] = new Uint8Array(value);
  }
  return row2;
}
class DurableObjectSqliteExecutor {
  storage;
  constructor(storage) {
    this.storage = storage;
  }
  async exec(sql) {
    this.check();
    this.storage.sql.exec(sql);
  }
  async run(sql, ...params) {
    this.check();
    this.storage.sql.exec(sql, ...bind(params));
  }
  async get(sql, ...params) {
    this.check();
    const result = this.storage.sql.exec(sql, ...bind(params)).next();
    return result.done ? void 0 : row(result.value);
  }
  async all(sql, ...params) {
    this.check();
    return this.storage.sql.exec(sql, ...bind(params)).toArray().map((value) => row(value));
  }
  /** Throws when this handle may no longer run SQL. */
  check() {
  }
}
class DurableObjectSqliteTransaction extends DurableObjectSqliteExecutor {
  active = true;
  check() {
    if (!this.active) throw new Error("SQLite transaction handle is no longer active");
  }
}
class DurableObjectSqliteDatabase extends DurableObjectSqliteExecutor {
  queue;
  constructor(storage) {
    super(storage);
    this.queue = queueFor(storage);
  }
  exec(sql) {
    return this.queue.run(() => super.exec(sql));
  }
  run(sql, ...params) {
    return this.queue.run(() => super.run(sql, ...params));
  }
  get(sql, ...params) {
    return this.queue.run(() => super.get(sql, ...params));
  }
  all(sql, ...params) {
    return this.queue.run(() => super.all(sql, ...params));
  }
  /** `ctx.storage.transaction()` commits when the callback resolves and rolls back when it rejects. */
  transaction(callback) {
    return this.queue.hold(
      () => this.storage.transaction(async () => {
        const transaction = new DurableObjectSqliteTransaction(this.storage);
        try {
          return await callback(transaction);
        } finally {
          transaction.active = false;
        }
      })
    );
  }
  /** Waits for queued work. The Durable Object owns its storage, so nothing else closes. */
  close() {
    return this.queue.run(() => {
    });
  }
}
export {
  DurableObjectSqliteDatabase
};
