// node:fs, node:fs/promises

const path = internal.requireBuiltin("path");
const Buffer = internal.Buffer;
const EventEmitter = internal.EventEmitter;

const constants = {
  F_OK: 0,
  R_OK: 4,
  W_OK: 2,
  X_OK: 1,
  COPYFILE_EXCL: 1,
  COPYFILE_FICLONE: 2,
  COPYFILE_FICLONE_FORCE: 4,
  O_RDONLY: 0,
  O_WRONLY: 1,
  O_RDWR: 2,
  O_CREAT: 64,
  O_EXCL: 128,
  O_TRUNC: 512,
  O_APPEND: 1024,
  O_SYNC: 1052672,
  S_IFMT: 0o170000,
  S_IFREG: 0o100000,
  S_IFDIR: 0o040000,
  S_IFCHR: 0o020000,
  S_IFBLK: 0o060000,
  S_IFIFO: 0o010000,
  S_IFLNK: 0o120000,
  S_IFSOCK: 0o140000,
  S_IRWXU: 0o700,
  S_IRUSR: 0o400,
  S_IWUSR: 0o200,
  S_IXUSR: 0o100,
  UV_FS_COPYFILE_EXCL: 1,
};

function toPath(p, name = "path") {
  if (typeof p === "string") return path.resolve(p);
  if (p instanceof Uint8Array) return path.resolve(Buffer.from(p).toString());
  if (p instanceof internal.URL || (p && typeof p === "object" && typeof p.href === "string" && p.protocol)) {
    if (p.protocol !== "file:") {
      const err = new TypeError("The URL must be of scheme file");
      err.code = "ERR_INVALID_URL_SCHEME";
      throw err;
    }
    return decodeURIComponent(p.pathname);
  }
  const err = new TypeError(`The "${name}" argument must be of type string or an instance of Buffer or URL. Received ${p === null ? "null" : typeof p}`);
  err.code = "ERR_INVALID_ARG_TYPE";
  throw err;
}

const encodingOf = (options) => (typeof options === "string" ? options : options?.encoding ?? null);
const flagOf = (options, def) => (typeof options === "object" && options?.flag) || def;
const modeOf = (options, def) => (typeof options === "object" && options?.mode !== undefined ? options.mode : def);

function toBytes(data, encoding) {
  if (typeof data === "string") return Buffer.from(data, encoding ?? "utf8");
  if (data instanceof Uint8Array) return data;
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (data && typeof data === "object" && typeof data.toString === "function" && data.toString !== Object.prototype.toString) {
    return Buffer.from(String(data));
  }
  const err = new TypeError(`The "data" argument must be of type string or an instance of Buffer, TypedArray, or DataView. Received ${data === null ? "null" : typeof data}`);
  err.code = "ERR_INVALID_ARG_TYPE";
  throw err;
}

const asBuffer = (u8) => Buffer.from(u8.buffer, u8.byteOffset, u8.byteLength);
const decode = (u8, encoding) => (encoding && encoding !== "buffer" ? asBuffer(u8).toString(encoding) : asBuffer(u8));

class Stats {
  constructor(raw) {
    Object.assign(this, raw);
    this.atime = new Date(raw.atimeMs);
    this.mtime = new Date(raw.mtimeMs);
    this.ctime = new Date(raw.ctimeMs);
    this.birthtime = new Date(raw.birthtimeMs);
  }
  #t() {
    return this.mode & constants.S_IFMT;
  }
  isFile() {
    return this.#t() === constants.S_IFREG;
  }
  isDirectory() {
    return this.#t() === constants.S_IFDIR;
  }
  isSymbolicLink() {
    return this.#t() === constants.S_IFLNK;
  }
  isFIFO() {
    return this.#t() === constants.S_IFIFO;
  }
  isSocket() {
    return this.#t() === constants.S_IFSOCK;
  }
  isCharacterDevice() {
    return this.#t() === constants.S_IFCHR;
  }
  isBlockDevice() {
    return this.#t() === constants.S_IFBLK;
  }
}

class Dirent {
  #type;
  constructor(name, type, parentPath) {
    this.name = name;
    this.parentPath = parentPath;
    this.path = parentPath;
    this.#type = type;
  }
  isFile() {
    return this.#type === 1;
  }
  isDirectory() {
    return this.#type === 2;
  }
  isSymbolicLink() {
    return this.#type === 3;
  }
  isFIFO() {
    return this.#type === 4;
  }
  isSocket() {
    return this.#type === 5;
  }
  isCharacterDevice() {
    return this.#type === 6;
  }
  isBlockDevice() {
    return this.#type === 7;
  }
}

function mkdirArgs(options) {
  if (typeof options === "number" || typeof options === "string") return [false, Number(options) || 0o777];
  return [Boolean(options?.recursive), options?.mode ?? 0o777];
}

function readdirResult(dir, entries, options, recurseFn) {
  const withTypes = typeof options === "object" && options?.withFileTypes;
  const recursive = typeof options === "object" && options?.recursive;
  const out = [];
  const visit = (base, list, rel) => {
    for (const [name, type] of list) {
      const relName = rel ? `${rel}/${name}` : name;
      out.push(withTypes ? new Dirent(name, type, base) : relName);
      if (recursive && type === 2) visit(`${base}/${name}`, recurseFn(`${base}/${name}`), relName);
    }
  };
  visit(dir, entries, "");
  const enc = encodingOf(options);
  if (enc === "buffer" && !withTypes) return out.map((n) => Buffer.from(n));
  return out;
}

const sync = {
  existsSync(p) {
    try {
      return ops.fsSync("exists", toPath(p));
    } catch {
      return false;
    }
  },
  readFileSync(p, options) {
    if (typeof p === "number") {
      const chunks = [];
      for (;;) {
        const c = ops.fsSync("read", p, 65536, null);
        if (!c.length) break;
        chunks.push(c);
      }
      return decode(Buffer.concat(chunks), encodingOf(options));
    }
    return decode(ops.fsSync("readFile", toPath(p)), encodingOf(options));
  },
  writeFileSync(p, data, options) {
    const bytes = toBytes(data, encodingOf(options));
    if (typeof p === "number") {
      sync.writeSync(p, bytes);
      return;
    }
    ops.fsSync("writeFile", toPath(p), bytes, flagOf(options, "w"), modeOf(options, 0o666));
  },
  appendFileSync(p, data, options) {
    const bytes = toBytes(data, encodingOf(options));
    if (typeof p === "number") {
      sync.writeSync(p, bytes);
      return;
    }
    ops.fsSync("writeFile", toPath(p), bytes, flagOf(options, "a"), modeOf(options, 0o666));
  },
  statSync(p, options) {
    try {
      return new Stats(ops.fsSync("stat", toPath(p)));
    } catch (err) {
      if (options?.throwIfNoEntry === false && err.code === "ENOENT") return undefined;
      throw err;
    }
  },
  lstatSync(p, options) {
    try {
      return new Stats(ops.fsSync("lstat", toPath(p)));
    } catch (err) {
      if (options?.throwIfNoEntry === false && err.code === "ENOENT") return undefined;
      throw err;
    }
  },
  fstatSync(fd) {
    return new Stats(ops.fsSync("fstat", fd));
  },
  accessSync(p, mode = constants.F_OK) {
    ops.fsSync("access", toPath(p), mode);
  },
  mkdirSync(p, options) {
    const [recursive, mode] = mkdirArgs(options);
    return ops.fsSync("mkdir", toPath(p), recursive, mode);
  },
  readdirSync(p, options) {
    const dir = toPath(p);
    return readdirResult(dir, ops.fsSync("readdir", dir), options, (d) => ops.fsSync("readdir", d));
  },
  rmSync(p, options) {
    ops.fsSync("rm", toPath(p), Boolean(options?.recursive), Boolean(options?.force));
  },
  rmdirSync(p, options) {
    ops.fsSync("rmdir", toPath(p), Boolean(options?.recursive));
  },
  unlinkSync(p) {
    ops.fsSync("unlink", toPath(p));
  },
  renameSync(a, b) {
    ops.fsSync("rename", toPath(a, "oldPath"), toPath(b, "newPath"));
  },
  copyFileSync(a, b, mode = 0) {
    ops.fsSync("copyFile", toPath(a, "src"), toPath(b, "dest"), mode);
  },
  cpSync(a, b, options = {}) {
    ops.fsSync("cp", toPath(a, "src"), toPath(b, "dest"), Boolean(options.recursive), options.force !== false, Boolean(options.errorOnExist));
  },
  realpathSync: Object.assign((p, options) => {
    const r = ops.fsSync("realpath", toPath(p));
    return encodingOf(options) === "buffer" ? Buffer.from(r) : r;
  }, { native: (p) => ops.fsSync("realpath", toPath(p)) }),
  readlinkSync(p) {
    return ops.fsSync("readlink", toPath(p));
  },
  symlinkSync(target, p) {
    ops.fsSync("symlink", String(target), toPath(p));
  },
  linkSync(a, b) {
    ops.fsSync("link", toPath(a), toPath(b));
  },
  chmodSync(p, mode) {
    ops.fsSync("chmod", toPath(p), typeof mode === "string" ? parseInt(mode, 8) : mode);
  },
  chownSync() {},
  lchownSync() {},
  utimesSync(p, atime, mtime) {
    const t = (v) => (v instanceof Date ? v.getTime() / 1000 : Number(v));
    ops.fsSync("utimes", toPath(p), t(atime), t(mtime));
  },
  truncateSync(p, len = 0) {
    ops.fsSync("truncate", toPath(p), len);
  },
  mkdtempSync(prefix) {
    return ops.fsSync("mkdtemp", path.isAbsolute(prefix) ? prefix : path.resolve(prefix));
  },
  openSync(p, flags = "r", mode = 0o666) {
    return ops.fsSync("open", toPath(p), typeof flags === "number" ? numericFlags(flags) : flags, mode);
  },
  closeSync(fd) {
    ops.fsSync("close", fd);
  },
  readSync(fd, buffer, offsetOrOptions, length, position) {
    let offset = offsetOrOptions;
    if (typeof offsetOrOptions === "object" && offsetOrOptions !== null) {
      ({ offset = 0, length = buffer.byteLength - offset, position = null } = offsetOrOptions);
    }
    offset = offset ?? 0;
    length = length ?? buffer.byteLength - offset;
    const data = ops.fsSync("read", fd, length, position ?? null);
    new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength).set(data, offset);
    return data.length;
  },
  writeSync(fd, data, offsetOrPosition, lengthOrEncoding, position) {
    let bytes;
    let pos = null;
    if (typeof data === "string") {
      bytes = Buffer.from(data, typeof lengthOrEncoding === "string" ? lengthOrEncoding : "utf8");
      pos = typeof offsetOrPosition === "number" ? offsetOrPosition : null;
    } else {
      const offset = offsetOrPosition ?? 0;
      const length = typeof lengthOrEncoding === "number" ? lengthOrEncoding : data.byteLength - offset;
      bytes = new Uint8Array(data.buffer, data.byteOffset + offset, length);
      pos = position ?? null;
    }
    if (fd === 1 || fd === 2) {
      ops.write(fd, bytes);
      return bytes.length;
    }
    return ops.fsSync("write", fd, bytes, pos);
  },
  fsyncSync(fd) {
    ops.fsSync("fsync", fd);
  },
  fdatasyncSync(fd) {
    ops.fsSync("fsync", fd);
  },
  ftruncateSync(fd, len = 0) {
    ops.fsSync("ftruncate", fd, len);
  },
  futimesSync(fd, atime, mtime) {
    const t = (v) => (v instanceof Date ? v.getTime() / 1000 : Number(v));
    ops.fsSync("futimes", fd, t(atime), t(mtime));
  },
  lutimesSync(p, atime, mtime) {
    sync.utimesSync(p, atime, mtime);
  },
  fchmodSync(fd, mode) {
    ops.fsSync("fchmod", fd, typeof mode === "string" ? parseInt(mode, 8) : mode);
  },
  fchownSync() {},
  readvSync(fd, buffers, position) {
    let total = 0;
    for (const b of buffers) {
      const n = sync.readSync(fd, b, 0, b.byteLength, position === undefined || position === null ? null : position + total);
      total += n;
      if (n < b.byteLength) break;
    }
    return total;
  },
  writevSync(fd, buffers, position) {
    let total = 0;
    for (const b of buffers) total += sync.writeSync(fd, b, 0, b.byteLength, position === undefined || position === null ? null : position + total);
    return total;
  },
};

function numericFlags(n) {
  const acc = n & 3;
  const append = n & constants.O_APPEND;
  const creat = n & constants.O_CREAT;
  const excl = n & constants.O_EXCL;
  const trunc = n & constants.O_TRUNC;
  if (acc === 0) return "r";
  let f = append ? "a" : trunc || creat ? "w" : "r+";
  if (excl) f += "x";
  if (acc === 2 && f !== "r+") f += "+";
  return f;
}

class FileHandle {
  constructor(fd) {
    this.fd = fd;
  }
  async read(buffer, offset, length, position) {
    if (buffer === undefined || (typeof buffer === "object" && !ArrayBuffer.isView(buffer))) {
      const opts = buffer ?? {};
      buffer = opts.buffer ?? Buffer.alloc(16384);
      offset = opts.offset ?? 0;
      length = opts.length ?? buffer.byteLength - offset;
      position = opts.position ?? null;
    }
    offset = offset ?? 0;
    length = length ?? buffer.byteLength - offset;
    const data = await ops.fsAsync("read", this.fd, length, position ?? null);
    new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength).set(data, offset);
    return { bytesRead: data.length, buffer };
  }
  async write(data, offsetOrPosition, lengthOrEncoding, position) {
    let bytes;
    let pos = null;
    if (typeof data === "string") {
      bytes = Buffer.from(data, typeof lengthOrEncoding === "string" ? lengthOrEncoding : "utf8");
      pos = typeof offsetOrPosition === "number" ? offsetOrPosition : null;
    } else {
      const offset = offsetOrPosition ?? 0;
      const length = typeof lengthOrEncoding === "number" ? lengthOrEncoding : data.byteLength - offset;
      bytes = new Uint8Array(data.buffer, data.byteOffset + offset, length);
      pos = position ?? null;
    }
    const bytesWritten = await ops.fsAsync("write", this.fd, bytes, pos);
    return { bytesWritten, buffer: data };
  }
  async readFile(options) {
    const chunks = [];
    for (;;) {
      const c = await ops.fsAsync("read", this.fd, 65536, null);
      if (!c.length) break;
      chunks.push(c);
    }
    return decode(Buffer.concat(chunks), encodingOf(options));
  }
  async writeFile(data, options) {
    await this.write(toBytes(data, encodingOf(options)));
  }
  async appendFile(data, options) {
    await this.write(toBytes(data, encodingOf(options)));
  }
  async stat() {
    return new Stats(await ops.fsAsync("fstat", this.fd));
  }
  async truncate(len = 0) {
    await ops.fsAsync("ftruncate", this.fd, len);
  }
  async sync() {
    await ops.fsAsync("fsync", this.fd);
  }
  async datasync() {
    await ops.fsAsync("fsync", this.fd);
  }
  async close() {
    if (this.fd === -1) return;
    const fd = this.fd;
    this.fd = -1;
    await ops.fsAsync("close", fd);
  }
  async [Symbol.asyncDispose]() {
    await this.close();
  }
  async *readLines() {
    const text = await this.readFile("utf8");
    const lines = text.split(/\r?\n/);
    if (lines[lines.length - 1] === "") lines.pop();
    yield* lines;
  }
  createReadStream(options) {
    return createReadStream(null, { ...options, fd: this.fd });
  }
  createWriteStream(options) {
    return createWriteStream(null, { ...options, fd: this.fd });
  }
}

const promises = {
  async readFile(p, options) {
    if (p instanceof FileHandle) return p.readFile(options);
    const signal = options?.signal;
    signal?.throwIfAborted();
    const data = await ops.fsAsync("readFile", toPath(p));
    signal?.throwIfAborted();
    return decode(data, encodingOf(options));
  },
  async writeFile(p, data, options) {
    if (p instanceof FileHandle) return p.writeFile(data, options);
    if (data && typeof data === "object" && !ArrayBuffer.isView(data) && (data[Symbol.asyncIterator] || data[Symbol.iterator]) && typeof data !== "string") {
      const chunks = [];
      for await (const c of data) chunks.push(toBytes(c, encodingOf(options)));
      data = Buffer.concat(chunks);
    }
    await ops.fsAsync("writeFile", toPath(p), toBytes(data, encodingOf(options)), flagOf(options, "w"), modeOf(options, 0o666));
  },
  async appendFile(p, data, options) {
    if (p instanceof FileHandle) return p.appendFile(data, options);
    await ops.fsAsync("writeFile", toPath(p), toBytes(data, encodingOf(options)), flagOf(options, "a"), modeOf(options, 0o666));
  },
  async stat(p) {
    return new Stats(await ops.fsAsync("stat", toPath(p)));
  },
  async lstat(p) {
    return new Stats(await ops.fsAsync("lstat", toPath(p)));
  },
  async access(p, mode = constants.F_OK) {
    await ops.fsAsync("access", toPath(p), mode);
  },
  async mkdir(p, options) {
    const [recursive, mode] = mkdirArgs(options);
    return ops.fsAsync("mkdir", toPath(p), recursive, mode);
  },
  async readdir(p, options) {
    const dir = toPath(p);
    const entries = await ops.fsAsync("readdir", dir);
    return readdirResult(dir, entries, options, (d) => ops.fsSync("readdir", d));
  },
  async rm(p, options) {
    await ops.fsAsync("rm", toPath(p), Boolean(options?.recursive), Boolean(options?.force));
  },
  async rmdir(p, options) {
    await ops.fsAsync("rmdir", toPath(p), Boolean(options?.recursive));
  },
  async unlink(p) {
    await ops.fsAsync("unlink", toPath(p));
  },
  async rename(a, b) {
    await ops.fsAsync("rename", toPath(a, "oldPath"), toPath(b, "newPath"));
  },
  async copyFile(a, b, mode = 0) {
    await ops.fsAsync("copyFile", toPath(a, "src"), toPath(b, "dest"), mode);
  },
  async cp(a, b, options = {}) {
    await ops.fsAsync("cp", toPath(a, "src"), toPath(b, "dest"), Boolean(options.recursive), options.force !== false, Boolean(options.errorOnExist));
  },
  async realpath(p) {
    return ops.fsAsync("realpath", toPath(p));
  },
  async readlink(p) {
    return ops.fsAsync("readlink", toPath(p));
  },
  async symlink(target, p) {
    await ops.fsAsync("symlink", String(target), toPath(p));
  },
  async link(a, b) {
    await ops.fsAsync("link", toPath(a), toPath(b));
  },
  async chmod(p, mode) {
    await ops.fsAsync("chmod", toPath(p), typeof mode === "string" ? parseInt(mode, 8) : mode);
  },
  async chown() {},
  async lchown() {},
  async utimes(p, atime, mtime) {
    const t = (v) => (v instanceof Date ? v.getTime() / 1000 : Number(v));
    await ops.fsAsync("utimes", toPath(p), t(atime), t(mtime));
  },
  async truncate(p, len = 0) {
    await ops.fsAsync("truncate", toPath(p), len);
  },
  async mkdtemp(prefix) {
    return ops.fsAsync("mkdtemp", path.isAbsolute(prefix) ? prefix : path.resolve(prefix));
  },
  async open(p, flags = "r", mode = 0o666) {
    const fd = await ops.fsAsync("open", toPath(p), typeof flags === "number" ? numericFlags(flags) : flags, mode);
    return new FileHandle(fd);
  },
  async opendir(p) {
    const dir = toPath(p);
    const entries = readdirResult(dir, await ops.fsAsync("readdir", dir), { withFileTypes: true });
    let i = 0;
    return {
      path: dir,
      async read() {
        return entries[i++] ?? null;
      },
      readSync() {
        return entries[i++] ?? null;
      },
      async close() {},
      closeSync() {},
      async *[Symbol.asyncIterator]() {
        yield* entries;
      },
    };
  },
  watch(p, options = {}) {
    const watcher = watch(p, options);
    return internal.EventEmitter.on(watcher, "change", { signal: options.signal });
  },
  constants,
};

// Callback API built on promises.
const callbackApi = {};
for (const [name, fn] of Object.entries(promises)) {
  if (name === "constants" || name === "watch") continue;
  callbackApi[name] = function (...args) {
    const cb = args[args.length - 1];
    if (typeof cb !== "function") {
      const err = new TypeError('The "cb" argument must be of type function. Received undefined');
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
    args.pop();
    let p;
    try {
      p = fn(...args);
    } catch (err) {
      process.nextTick(cb, err);
      return;
    }
    p.then(
      (v) => (name === "open" ? cb(null, v.fd) : cb(null, v)),
      (err) => cb(err),
    );
  };
}
callbackApi.exists = (p, cb) => {
  promises.access(p).then(
    () => cb(true),
    () => cb(false),
  );
};
callbackApi.close = (fd, cb = () => {}) => {
  ops.fsAsync("close", fd).then(() => cb(null), cb);
};
callbackApi.read = (fd, buffer, offset, length, position, cb) => {
  if (typeof buffer === "function") {
    cb = buffer;
    buffer = Buffer.alloc(16384);
    offset = 0;
    length = buffer.length;
    position = null;
  }
  new FileHandle(fd).read(buffer, offset, length, position).then((r) => cb(null, r.bytesRead, r.buffer), cb);
};
callbackApi.write = (fd, data, ...rest) => {
  const cb = rest.pop();
  new FileHandle(fd).write(data, ...rest).then((r) => cb(null, r.bytesWritten, r.buffer), cb);
};
// fd-based calls without a promise counterpart run synchronously and report via the callback.
for (const name of ["fsync", "fdatasync", "ftruncate", "futimes", "lutimes", "fchmod", "fchown", "readv", "writev"]) {
  callbackApi[name] = (...args) => {
    const cb = typeof args[args.length - 1] === "function" ? args.pop() : () => {};
    let result;
    try {
      result = sync[`${name}Sync`](...args);
    } catch (err) {
      process.nextTick(cb, err);
      return;
    }
    if (name === "readv" || name === "writev") process.nextTick(cb, null, result, args[1]);
    else process.nextTick(cb, null);
  };
}
callbackApi.fstat = (fd, options, cb) => {
  if (typeof options === "function") cb = options;
  ops.fsAsync("fstat", fd).then((s) => cb(null, new Stats(s)), cb);
};

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

function createReadStream(p, options = {}) {
  if (typeof options === "string") options = { encoding: options };
  const { Readable } = internal.requireBuiltin("stream");
  let fd = options.fd ?? null;
  let pos = options.start ?? null;
  const end = options.end ?? Infinity;
  const hwm = options.highWaterMark ?? 65536;
  const filePath = p === null ? undefined : toPath(p);
  const stream = new Readable({
    highWaterMark: hwm,
    encoding: options.encoding,
    read() {
      const go = async () => {
        if (fd === null) {
          fd = await ops.fsAsync("open", filePath, options.flags ?? "r", options.mode ?? 0o666);
          stream.emit("open", fd);
          stream.emit("ready");
        }
        let want = hwm;
        if (pos !== null && end !== Infinity) want = Math.min(want, end - pos + 1);
        if (want <= 0) {
          stream.push(null);
          return;
        }
        const chunk = await ops.fsAsync("read", fd, want, pos);
        if (pos !== null) pos += chunk.length;
        stream.bytesRead += chunk.length;
        if (chunk.length === 0) stream.push(null);
        else stream.push(asBuffer(chunk));
      };
      go().catch((err) => stream.destroy(err));
    },
    destroy(err, cb) {
      if (fd !== null && options.autoClose !== false && options.fd === undefined) {
        const f = fd;
        fd = null;
        ops.fsAsync("close", f).then(() => cb(err), () => cb(err));
      } else cb(err);
    },
  });
  stream.path = filePath;
  stream.bytesRead = 0;
  stream.close = (cb) => stream.destroy(null, cb);
  stream.on("end", () => {
    if (options.autoClose !== false) stream.destroy();
  });
  return stream;
}

function createWriteStream(p, options = {}) {
  if (typeof options === "string") options = { encoding: options };
  const { Writable } = internal.requireBuiltin("stream");
  let fd = options.fd ?? null;
  const filePath = p === null ? undefined : toPath(p);
  let pos = options.start ?? null;
  // One open shared by the eager open and the first writes: a second "w" open would truncate
  // the file after earlier writes landed.
  let opening = null;
  const ensureOpen = () => {
    if (fd !== null) return Promise.resolve(fd);
    opening ??= ops.fsAsync("open", filePath, options.flags ?? "w", options.mode ?? 0o666).then((f) => {
      fd = f;
      stream.emit("open", fd);
      stream.emit("ready");
      return fd;
    });
    return opening;
  };
  const stream = new Writable({
    highWaterMark: options.highWaterMark,
    decodeStrings: true,
    defaultEncoding: options.encoding ?? "utf8",
    write(chunk, encoding, cb) {
      ensureOpen()
        .then((f) => ops.fsAsync("write", f, chunk, pos))
        .then((n) => {
          if (pos !== null) pos += n;
          stream.bytesWritten += n;
          cb();
        }, cb);
    },
    final(cb) {
      ensureOpen().then(() => cb(), cb);
    },
    destroy(err, cb) {
      if (fd !== null && options.autoClose !== false && options.fd === undefined) {
        const f = fd;
        fd = null;
        ops.fsAsync("close", f).then(() => cb(err), () => cb(err));
      } else cb(err);
    },
  });
  stream.path = filePath;
  stream.bytesWritten = 0;
  stream.close = (cb) => {
    stream.end();
    if (cb) stream.once("close", cb);
  };
  stream.on("finish", () => {
    if (options.autoClose !== false) stream.destroy();
  });
  if (filePath !== undefined) ensureOpen().catch((err) => stream.destroy(err));
  return stream;
}

// ---------------------------------------------------------------------------
// Watching (polling based)
// ---------------------------------------------------------------------------

const watchers = new Map();
internal.fsWatchEvent = (id, kind, name) => {
  const w = watchers.get(id);
  if (w) w.emit("change", kind, name || path.basename(w._path));
};

function watch(p, options, listener) {
  if (typeof options === "function") {
    listener = options;
    options = {};
  }
  if (typeof options === "string") options = { encoding: options };
  const target = toPath(p);
  if (!sync.existsSync(target)) {
    const err = new Error(`ENOENT: no such file or directory, watch '${target}'`);
    err.code = "ENOENT";
    err.errno = -2;
    err.syscall = "watch";
    err.path = target;
    throw err;
  }
  const w = new EventEmitter();
  w._path = target;
  const id = ops.fsWatch(target, options?.interval ?? 300);
  watchers.set(id, w);
  if (listener) w.on("change", listener);
  w.close = () => {
    if (watchers.delete(id)) {
      ops.fsUnwatch(id);
      queueMicrotask(() => w.emit("close"));
    }
  };
  w.ref = () => w;
  w.unref = () => w;
  options?.signal?.addEventListener("abort", () => w.close(), { once: true });
  return w;
}

const fileWatchers = new Map();
function watchFile(p, options, listener) {
  if (typeof options === "function") {
    listener = options;
    options = {};
  }
  const target = toPath(p);
  const statOrEmpty = () => sync.statSync(target, { throwIfNoEntry: false }) ?? new Stats({ mode: 0, size: 0, atimeMs: 0, mtimeMs: 0, ctimeMs: 0, birthtimeMs: 0 });
  let prev = statOrEmpty();
  let entry = fileWatchers.get(target);
  if (!entry) {
    entry = { listeners: new Set(), timer: null };
    entry.timer = setInterval(() => {
      const cur = statOrEmpty();
      if (cur.mtimeMs !== prev.mtimeMs || cur.size !== prev.size) {
        for (const l of entry.listeners) l(cur, prev);
      }
      prev = cur;
    }, options?.interval ?? 5007);
    fileWatchers.set(target, entry);
  }
  entry.listeners.add(listener);
  return { ref() {}, unref() {} };
}

function unwatchFile(p, listener) {
  const target = toPath(p);
  const entry = fileWatchers.get(target);
  if (!entry) return;
  if (listener) entry.listeners.delete(listener);
  else entry.listeners.clear();
  if (entry.listeners.size === 0) {
    clearInterval(entry.timer);
    fileWatchers.delete(target);
  }
}

const fs = {
  ...callbackApi,
  ...sync,
  promises,
  constants,
  Stats,
  Dirent,
  createReadStream,
  createWriteStream,
  watch,
  watchFile,
  unwatchFile,
  F_OK: 0,
  R_OK: 4,
  W_OK: 2,
  X_OK: 1,
};
internal.fs = fs;
internal.defineBuiltin("fs", () => fs);
internal.defineBuiltin("fs/promises", () => promises);
