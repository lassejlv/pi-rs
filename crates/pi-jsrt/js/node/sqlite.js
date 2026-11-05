// node:sqlite — DatabaseSync and StatementSync (Node 22) over rusqlite (ops/sqlite.rs).

function codedError(Ctor, code, message) {
  const err = new Ctor(message);
  err.code = code;
  return err;
}

const kId = Symbol("sqlite.token");
// Open connection ids by DatabaseSync (absent once closed).
const ids = new WeakMap();

function isNamedArg(value) {
  return value !== null && typeof value === "object" && !ArrayBuffer.isView(value) && !(value instanceof ArrayBuffer);
}

function splitArgs(args) {
  if (args.length > 0 && isNamedArg(args[0])) return [args.slice(1), args[0]];
  return [args, undefined];
}

class StatementSync {
  #db;
  #sql;
  #names;
  #columns;
  #readBigInts = false;
  #bare = true;
  constructor(token, db, sql, info) {
    if (token !== kId) throw codedError(TypeError, "ERR_ILLEGAL_CONSTRUCTOR", "Illegal constructor");
    this.#db = db;
    this.#sql = sql;
    this.#names = info[0];
    this.#columns = info[1];
  }
  #id() {
    const id = ids.get(this.#db);
    if (id === undefined) throw codedError(Error, "ERR_INVALID_STATE", "database is not open");
    return id;
  }
  run(...args) {
    const [positional, named] = splitArgs(args);
    const [changes, lastInsertRowid] = ops.sqliteRun(this.#id(), this.#sql, positional, named, this.#bare);
    return { changes, lastInsertRowid };
  }
  get(...args) {
    const [positional, named] = splitArgs(args);
    return ops.sqliteQuery(this.#id(), this.#sql, positional, named, this.#bare, false, this.#readBigInts);
  }
  all(...args) {
    const [positional, named] = splitArgs(args);
    return ops.sqliteQuery(this.#id(), this.#sql, positional, named, this.#bare, true, this.#readBigInts);
  }
  *iterate(...args) {
    yield* this.all(...args);
  }
  columns() {
    return this.#columns.map(([name, column, table, database, type]) => ({ column, database, name, table, type }));
  }
  setReadBigInts(enabled) {
    if (typeof enabled !== "boolean") throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", 'The "readBigInts" argument must be a boolean.');
    this.#readBigInts = enabled;
  }
  setAllowBareNamedParameters(enabled) {
    if (typeof enabled !== "boolean") throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", 'The "allowBareNamedParameters" argument must be a boolean.');
    this.#bare = enabled;
  }
  get sourceSQL() {
    return this.#sql;
  }
  get expandedSQL() {
    return this.#sql;
  }
}

class DatabaseSync {
  #path;
  #options;
  constructor(path, options = {}) {
    if (typeof path !== "string" && !(path instanceof URL) && !ArrayBuffer.isView(path)) {
      throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", 'The "path" argument must be a string, Uint8Array, or URL without null bytes.');
    }
    if (options === null || typeof options !== "object") {
      throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", 'The "options" argument must be an object.');
    }
    this.#path = path instanceof URL ? (path.protocol === "file:" ? decodeURIComponent(path.pathname) : String(path)) : ArrayBuffer.isView(path) ? new TextDecoder().decode(path) : path;
    this.#options = {
      readOnly: options.readOnly === true,
      timeout: typeof options.timeout === "number" ? options.timeout : 0,
      enableForeignKeyConstraints: options.enableForeignKeyConstraints !== false,
      enableDoubleQuotedStringLiterals: options.enableDoubleQuotedStringLiterals === true,
    };
    if (options.open !== false) this.open();
  }
  open() {
    if (ids.has(this)) throw codedError(Error, "ERR_INVALID_STATE", "database is already open");
    const o = this.#options;
    ids.set(this, ops.sqliteOpen(this.#path, o.readOnly, o.timeout, o.enableForeignKeyConstraints, o.enableDoubleQuotedStringLiterals));
  }
  close() {
    if (!ids.has(this)) throw codedError(Error, "ERR_INVALID_STATE", "database is not open");
    const id = ids.get(this);
    ids.delete(this);
    ops.sqliteClose(id);
  }
  get isOpen() {
    return ids.has(this);
  }
  get isTransaction() {
    if (!ids.has(this)) throw codedError(Error, "ERR_INVALID_STATE", "database is not open");
    return ops.sqliteInTransaction(ids.get(this));
  }
  exec(sql) {
    if (!ids.has(this)) throw codedError(Error, "ERR_INVALID_STATE", "database is not open");
    if (typeof sql !== "string") throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", 'The "sql" argument must be a string.');
    ops.sqliteExec(ids.get(this), sql);
  }
  prepare(sql) {
    if (!ids.has(this)) throw codedError(Error, "ERR_INVALID_STATE", "database is not open");
    if (typeof sql !== "string") throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", 'The "sql" argument must be a string.');
    return new StatementSync(kId, this, sql, ops.sqlitePrepare(ids.get(this), sql));
  }
  [Symbol.dispose]() {
    if (ids.has(this)) this.close();
  }
}

internal.defineBuiltin("sqlite", () => ({
  DatabaseSync,
  StatementSync,
  constants: { SQLITE_CHANGESET_OMIT: 0, SQLITE_CHANGESET_REPLACE: 1, SQLITE_CHANGESET_ABORT: 2 },
}));
