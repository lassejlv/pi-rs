// node:stream (compact implementation of Readable/Writable/Duplex/Transform/PassThrough)

const EventEmitter = internal.EventEmitter;
const nextTick = (fn, ...args) => process.nextTick(fn, ...args);

function toChunk(chunk, encoding, objectMode, decodeStrings = true) {
  if (objectMode) return chunk;
  if (typeof chunk === "string") return decodeStrings ? internal.Buffer.from(chunk, encoding) : chunk;
  if (chunk instanceof Uint8Array && !(chunk instanceof internal.Buffer)) return internal.Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength);
  return chunk;
}

function chunkLength(chunk, objectMode) {
  if (objectMode) return 1;
  return chunk?.length ?? 0;
}

// A plain function like Node's legacy `Stream`, so old code can call `Stream.call(this)`.
function Stream(opts) {
  EventEmitter.call(this, opts);
}
Object.setPrototypeOf(Stream.prototype, EventEmitter.prototype);
Object.setPrototypeOf(Stream, EventEmitter);
Stream.prototype.pipe = function pipe(dest, options) {
  return Readable.prototype.pipe.call(this, dest, options);
};

class Readable extends Stream {
  constructor(options = {}) {
    super(options);
    const objectMode = Boolean(options.objectMode || options.readableObjectMode);
    this._readableState = {
      objectMode,
      highWaterMark: options.highWaterMark ?? options.readableHighWaterMark ?? (objectMode ? 16 : 65536),
      buffer: [],
      length: 0,
      flowing: null,
      ended: false,
      endEmitted: false,
      reading: false,
      decoder: null,
      encoding: null,
      destroyed: false,
      pipes: [],
      sync: false,
      readableListening: false,
      closed: false,
    };
    if (options.encoding) this.setEncoding(options.encoding);
    if (typeof options.read === "function") this._read = options.read;
    if (typeof options.destroy === "function") this._destroy = options.destroy;
    if (typeof options.construct === "function") this._construct = options.construct;
    this.readable = true;
    if (options.signal) options.signal.addEventListener("abort", () => this.destroy(options.signal.reason), { once: true });
  }
  get readableFlowing() {
    return this._readableState.flowing;
  }
  get readableDidRead() {
    return Boolean(this._readableState.dataEmitted);
  }
  get readableAborted() {
    const st = this._readableState;
    return Boolean((st.destroyed || this.errored) && !st.endEmitted);
  }
  set readableFlowing(v) {
    this._readableState.flowing = v;
  }
  get readableLength() {
    return this._readableState.length;
  }
  get readableEnded() {
    return this._readableState.endEmitted;
  }
  get readableObjectMode() {
    return this._readableState.objectMode;
  }
  get readableEncoding() {
    return this._readableState.encoding;
  }
  get readableHighWaterMark() {
    return this._readableState.highWaterMark;
  }
  get destroyed() {
    return this._readableState.destroyed || Boolean(this._writableState?.destroyed);
  }
  set destroyed(v) {
    this._readableState.destroyed = v;
    if (this._writableState) this._writableState.destroyed = v;
  }
  get closed() {
    return this._readableState.closed;
  }
  _read() {}
  push(chunk, encoding) {
    const st = this._readableState;
    if (st.destroyed) return false;
    if (chunk === null) {
      st.ended = true;
      st.reading = false;
      this._scheduleFlow();
      return false;
    }
    if (st.ended) {
      const err = new Error("stream.push() after EOF");
      err.code = "ERR_STREAM_PUSH_AFTER_EOF";
      nextTick(() => this.destroy(err));
      return false;
    }
    st.reading = false;
    chunk = toChunk(chunk, encoding, st.objectMode);
    if (st.decoder) chunk = st.decoder.decode(chunk, { stream: true });
    if (!st.objectMode && chunkLength(chunk) === 0) return st.length < st.highWaterMark;
    st.buffer.push(chunk);
    st.length += chunkLength(chunk, st.objectMode);
    this._scheduleFlow();
    return st.length < st.highWaterMark;
  }
  unshift(chunk) {
    const st = this._readableState;
    chunk = toChunk(chunk, undefined, st.objectMode);
    st.buffer.unshift(chunk);
    st.length += chunkLength(chunk, st.objectMode);
  }
  _scheduleFlow() {
    const st = this._readableState;
    if (st.flowScheduled) return;
    st.flowScheduled = true;
    nextTick(() => {
      st.flowScheduled = false;
      this._flow();
    });
  }
  _flow() {
    const st = this._readableState;
    if (st.destroyed) return;
    if (st.flowing) {
      while (st.buffer.length && st.flowing) {
        const chunk = st.buffer.shift();
        st.length -= chunkLength(chunk, st.objectMode);
        st.dataEmitted = true;
        this.emit("data", chunk);
      }
    } else if (st.buffer.length && st.readableListening) {
      this.emit("readable");
    }
    if (st.ended && st.buffer.length === 0) {
      if (!st.endEmitted) {
        if (st.decoder) {
          const tail = st.decoder.decode();
          if (tail) {
            st.decoder = null;
            this.emit("data", tail);
          }
        }
        if (st.readableListening && !st.flowing && !st.readableEmittedAtEnd) {
          st.readableEmittedAtEnd = true;
          this.emit("readable");
        }
        st.endEmitted = true;
        this.readable = false;
        this.emit("end");
        if (this._autoClose !== false && !(this._writableState && !this._writableState.finished)) {
          nextTick(() => this._emitClose());
        }
      }
      return;
    }
    if (!st.ended && !st.reading && st.length < st.highWaterMark && (st.flowing || st.readableListening)) {
      st.reading = true;
      try {
        this._read(st.highWaterMark);
      } catch (err) {
        this.destroy(err);
      }
    }
  }
  _emitClose() {
    const st = this._readableState;
    if (st.closed) return;
    st.closed = true;
    this.emit("close");
  }
  read(n) {
    const st = this._readableState;
    if (st.buffer.length === 0) {
      if (!st.ended && !st.reading) {
        st.reading = true;
        try {
          this._read(st.highWaterMark);
        } catch (err) {
          this.destroy(err);
        }
      }
      if (st.ended) this._scheduleFlow();
      return null;
    }
    let out;
    if (st.objectMode) {
      out = st.buffer.shift();
      st.length -= 1;
    } else if (n === undefined || n >= st.length) {
      out = typeof st.buffer[0] === "string" ? st.buffer.join("") : internal.Buffer.concat(st.buffer);
      st.buffer = [];
      st.length = 0;
    } else {
      const all = typeof st.buffer[0] === "string" ? st.buffer.join("") : internal.Buffer.concat(st.buffer);
      out = all.slice(0, n);
      const rest = all.slice(n);
      st.buffer = rest.length ? [rest] : [];
      st.length = rest.length;
    }
    st.dataEmitted = true;
    // Like Node, read() emits 'data' for the chunk it returns.
    this.emit("data", out);
    if (st.ended && st.buffer.length === 0) this._scheduleFlow();
    else if (!st.ended && !st.reading && st.length < st.highWaterMark) {
      st.reading = true;
      nextTick(() => this._read(st.highWaterMark));
    }
    return out;
  }
  on(ev, fn) {
    super.on(ev, fn);
    const st = this._readableState;
    if (ev === "data" && st.flowing !== false) this.resume();
    else if (ev === "readable") {
      st.readableListening = true;
      st.flowing = false;
      this._scheduleFlow();
    }
    return this;
  }
  setEncoding(enc) {
    const st = this._readableState;
    st.decoder = new internal.TextDecoder(enc === "utf8" ? "utf-8" : enc);
    st.encoding = enc;
    if (enc === "hex" || enc === "base64" || enc === "latin1" || enc === "binary") {
      st.decoder = { decode: (b) => (b === undefined ? "" : internal.Buffer.from(b).toString(enc)) };
    }
    st.buffer = st.buffer.map((b) => (typeof b === "string" ? b : st.decoder.decode(b, { stream: true })));
    return this;
  }
  pause() {
    this._readableState.flowing = false;
    this.emit("pause");
    return this;
  }
  resume() {
    const st = this._readableState;
    if (!st.flowing) {
      // Like Node: with 'readable' listeners the stream stays in paused mode (read() drives it).
      st.flowing = !st.readableListening;
      this.emit("resume");
      this._scheduleFlow();
    }
    return this;
  }
  removeListener(ev, fn) {
    super.removeListener(ev, fn);
    if (ev === "readable" && this.listenerCount("readable") === 0) this._readableState.readableListening = false;
    return this;
  }
  off(ev, fn) {
    return this.removeListener(ev, fn);
  }
  isPaused() {
    return this._readableState.flowing === false;
  }
  pipe(dest, options = {}) {
    const src = this;
    const st = this._readableState;
    st.pipes.push(dest);
    const ondata = (chunk) => {
      const ret = dest.write(chunk);
      if (ret === false) {
        src.pause();
        dest.once("drain", () => src.resume());
      }
    };
    src.on("data", ondata);
    if (options.end !== false && dest !== process.stdout && dest !== process.stderr) {
      src.once("end", () => dest.end());
    }
    dest.emit("pipe", src);
    src.resume();
    return dest;
  }
  unpipe(dest) {
    const st = this._readableState;
    st.pipes = dest ? st.pipes.filter((d) => d !== dest) : [];
    this.removeAllListeners("data");
    return this;
  }
  destroy(err, cb) {
    const st = this._readableState;
    if (st.destroyed) {
      if (typeof cb === "function") cb(err);
      return this;
    }
    st.destroyed = true;
    if (this._writableState) this._writableState.destroyed = true;
    this._destroy(err ?? null, (e) => {
      nextTick(() => {
        if (e) {
          this.errored = e;
          this.emit("error", e);
        }
        this._emitClose();
        if (typeof cb === "function") cb(e);
      });
    });
    return this;
  }
  _destroy(err, cb) {
    cb(err);
  }
  wrap(stream) {
    stream.on("data", (c) => this.push(c));
    stream.on("end", () => this.push(null));
    return this;
  }
  // Like Node: paused-mode iteration with read() and 'readable', so it also works when other
  // code listens for 'readable' (a 'data'-based iterator would never flow then).
  async *[Symbol.asyncIterator]() {
    const st = this._readableState;
    let done = false;
    let error = null;
    let wake = null;
    const onReadable = () => wake?.();
    const onEnd = () => {
      done = true;
      wake?.();
    };
    const onError = (e) => {
      error = e;
      wake?.();
    };
    this.on("readable", onReadable);
    this.once("end", onEnd);
    this.once("error", onError);
    this.once("close", onEnd);
    try {
      for (;;) {
        if (error) throw error;
        const chunk = st.destroyed ? null : this.read();
        if (chunk !== null) {
          yield chunk;
          continue;
        }
        if (error) throw error;
        if (done || st.endEmitted) return;
        await new Promise((r) => {
          wake = r;
        });
        wake = null;
      }
    } finally {
      this.off("readable", onReadable);
      this.off("end", onEnd);
      this.off("error", onError);
      this.off("close", onEnd);
      if (!st.endEmitted && !st.destroyed) this.destroy();
    }
  }
  async toArray() {
    const out = [];
    for await (const c of this) out.push(c);
    return out;
  }
  map(fn) {
    const src = this;
    return Readable.from(
      (async function* () {
        for await (const c of src) yield await fn(c);
      })(),
    );
  }
  filter(fn) {
    const src = this;
    return Readable.from(
      (async function* () {
        for await (const c of src) if (await fn(c)) yield c;
      })(),
    );
  }
  static from(iterable, options = {}) {
    if (typeof iterable === "string" || iterable instanceof Uint8Array) {
      const r = new Readable({ objectMode: true, ...options, read() {} });
      r.push(iterable);
      r.push(null);
      return r;
    }
    const it = iterable[Symbol.asyncIterator] ? iterable[Symbol.asyncIterator]() : iterable[Symbol.iterator]();
    let reading = false;
    const r = new Readable({
      objectMode: true,
      ...options,
      read() {
        if (reading) return;
        reading = true;
        Promise.resolve(it.next()).then(
          ({ value, done }) => {
            reading = false;
            if (done) r.push(null);
            else Promise.resolve(value).then((v) => r.push(v));
          },
          (err) => r.destroy(err),
        );
      },
    });
    return r;
  }
}

Readable.prototype.addListener = Readable.prototype.on;

class Writable extends Stream {
  constructor(options = {}) {
    super(options);
    const objectMode = Boolean(options.objectMode || options.writableObjectMode);
    this._writableState = {
      objectMode,
      highWaterMark: options.highWaterMark ?? options.writableHighWaterMark ?? (objectMode ? 16 : 65536),
      queue: [],
      length: 0,
      writing: false,
      ending: false,
      ended: false,
      finished: false,
      destroyed: false,
      needDrain: false,
      corked: 0,
      decodeStrings: options.decodeStrings !== false,
      defaultEncoding: options.defaultEncoding ?? "utf8",
      closed: false,
    };
    if (typeof options.write === "function") this._write = options.write;
    if (typeof options.writev === "function") this._writev = options.writev;
    if (typeof options.final === "function") this._final = options.final;
    if (typeof options.destroy === "function") this._destroy = options.destroy;
    this.writable = true;
  }
  get writableLength() {
    return this._writableState.length;
  }
  get writableEnded() {
    return this._writableState.ending;
  }
  get writableFinished() {
    return this._writableState.finished;
  }
  get writableNeedDrain() {
    return this._writableState.needDrain;
  }
  get writableObjectMode() {
    return this._writableState.objectMode;
  }
  get writableHighWaterMark() {
    return this._writableState.highWaterMark;
  }
  get writableCorked() {
    return this._writableState.corked;
  }
  get destroyed() {
    return this._writableState.destroyed;
  }
  set destroyed(v) {
    this._writableState.destroyed = v;
  }
  get closed() {
    return this._writableState.closed;
  }
  _write(chunk, encoding, cb) {
    cb(new Error("The _write() method is not implemented"));
  }
  write(chunk, encoding, cb) {
    const st = this._writableState;
    if (typeof encoding === "function") {
      cb = encoding;
      encoding = undefined;
    }
    encoding = encoding ?? st.defaultEncoding;
    if (st.ending) {
      const err = new Error("write after end");
      err.code = "ERR_STREAM_WRITE_AFTER_END";
      nextTick(() => {
        if (typeof cb === "function") cb(err);
        this.emit("error", err);
      });
      return false;
    }
    if (chunk === null) {
      const err = new TypeError("May not write null values to stream");
      err.code = "ERR_STREAM_NULL_VALUES";
      throw err;
    }
    const data = toChunk(chunk, encoding, st.objectMode, st.decodeStrings);
    st.length += chunkLength(data, st.objectMode);
    st.queue.push({ chunk: data, encoding: typeof data === "string" ? encoding : "buffer", cb });
    const ok = st.length < st.highWaterMark;
    if (!ok) st.needDrain = true;
    if (!st.writing && !st.corked) this._drainQueue();
    return ok;
  }
  _drainQueue() {
    const st = this._writableState;
    if (st.writing || st.destroyed) return;
    const item = st.queue.shift();
    if (!item) {
      if (st.needDrain) {
        st.needDrain = false;
        nextTick(() => this.emit("drain"));
      }
      if (st.ending && !st.finished) this._finish();
      return;
    }
    st.writing = true;
    let sync = true;
    const done = (err) => {
      const finish = () => {
        st.writing = false;
        st.length -= chunkLength(item.chunk, st.objectMode);
        if (typeof item.cb === "function") item.cb(err ?? null);
        if (err) {
          this.destroy(err);
          return;
        }
        this._drainQueue();
      };
      if (sync) nextTick(finish);
      else finish();
    };
    try {
      this._write(item.chunk, item.encoding, done);
    } catch (err) {
      done(err);
    }
    sync = false;
  }
  _finish() {
    const st = this._writableState;
    if (st.finishing) return;
    st.finishing = true;
    const after = (err) => {
      if (err) {
        this.destroy(err);
        return;
      }
      st.finished = true;
      nextTick(() => {
        this.emit("finish");
        if (!this._readableState || this._readableState.endEmitted) this._emitCloseW();
      });
    };
    if (typeof this._final === "function") {
      try {
        this._final(after);
      } catch (err) {
        after(err);
      }
    } else after();
  }
  _emitCloseW() {
    const st = this._writableState;
    if (st.closed) return;
    st.closed = true;
    if (this._readableState) this._readableState.closed = true;
    this.emit("close");
  }
  end(chunk, encoding, cb) {
    const st = this._writableState;
    if (typeof chunk === "function") {
      cb = chunk;
      chunk = undefined;
    } else if (typeof encoding === "function") {
      cb = encoding;
      encoding = undefined;
    }
    if (chunk !== undefined && chunk !== null) this.write(chunk, encoding);
    if (typeof cb === "function") {
      if (st.finished) nextTick(cb);
      else this.once("finish", () => cb());
    }
    if (st.ending) return this;
    st.ending = true;
    st.ended = true;
    this.writable = false;
    st.corked = 0;
    if (!st.writing) this._drainQueue();
    return this;
  }
  cork() {
    this._writableState.corked++;
  }
  uncork() {
    const st = this._writableState;
    if (st.corked) st.corked--;
    if (!st.corked && !st.writing) this._drainQueue();
  }
  setDefaultEncoding(enc) {
    this._writableState.defaultEncoding = enc;
    return this;
  }
  destroy(err, cb) {
    const st = this._writableState;
    if (st.destroyed) {
      if (typeof cb === "function") cb(err);
      return this;
    }
    st.destroyed = true;
    if (this._readableState) this._readableState.destroyed = true;
    this._destroy(err ?? null, (e) => {
      nextTick(() => {
        if (e) {
          this.errored = e;
          this.emit("error", e);
        }
        this._emitCloseW();
        if (typeof cb === "function") cb(e);
      });
    });
    return this;
  }
  _destroy(err, cb) {
    cb(err);
  }
}

class Duplex extends Readable {
  constructor(options = {}) {
    super(options);
    const w = new Writable(options);
    this._writableState = w._writableState;
    if (options.allowHalfOpen === false) this.allowHalfOpen = false;
    else this.allowHalfOpen = true;
    if (typeof options.write === "function") this._write = options.write;
    if (typeof options.writev === "function") this._writev = options.writev;
    if (typeof options.final === "function") this._final = options.final;
    this.writable = true;
  }
}
for (const key of Object.getOwnPropertyNames(Writable.prototype)) {
  if (key === "constructor" || key === "destroy" || key === "destroyed" || key === "closed") continue;
  if (!Object.prototype.hasOwnProperty.call(Duplex.prototype, key)) {
    Object.defineProperty(Duplex.prototype, key, Object.getOwnPropertyDescriptor(Writable.prototype, key));
  }
}
Duplex.prototype._emitClose = function () {
  if (this._writableState && !this._writableState.finished && !this._readableState.destroyed) return;
  Writable.prototype._emitCloseW.call(this);
};

class Transform extends Duplex {
  constructor(options = {}) {
    super(options);
    if (typeof options.transform === "function") this._transform = options.transform;
    if (typeof options.flush === "function") this._flush = options.flush;
  }
  _transform(chunk, encoding, cb) {
    cb(new Error("The _transform() method is not implemented"));
  }
  _write(chunk, encoding, cb) {
    this._transform(chunk, encoding, (err, data) => {
      if (err) return cb(err);
      if (data !== undefined && data !== null) this.push(data);
      cb();
    });
  }
  _final(cb) {
    const done = (err, data) => {
      if (err) return cb(err);
      if (data !== undefined && data !== null) this.push(data);
      this.push(null);
      cb();
    };
    if (typeof this._flush === "function") this._flush(done);
    else done();
  }
  _read() {}
}

class PassThrough extends Transform {
  _transform(chunk, encoding, cb) {
    cb(null, chunk);
  }
}

function eos(stream, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = {};
  }
  if (typeof stream?.on !== "function") {
    // A web stream (ReadableStream/WritableStream): settle with its closed promise.
    const closed = stream?._state?.closedPromise ?? stream?.closed;
    let active = true;
    Promise.resolve(closed).then(
      () => active && callback.call(stream),
      (err) => active && callback.call(stream, err),
    );
    return () => {
      active = false;
    };
  }
  let called = false;
  const done = (err) => {
    if (called) return;
    called = true;
    cleanup();
    callback.call(stream, err);
  };
  const onFinish = () => {
    if (!stream._readableState || stream._readableState.endEmitted || options.readable === false) done();
  };
  const onEnd = () => {
    if (!stream._writableState || stream._writableState.finished || options.writable === false) done();
  };
  const onError = (err) => done(err);
  const onClose = () => done();
  stream.on("finish", onFinish);
  stream.on("end", onEnd);
  stream.on("error", onError);
  stream.on("close", onClose);
  function cleanup() {
    stream.off("finish", onFinish);
    stream.off("end", onEnd);
    stream.off("error", onError);
    stream.off("close", onClose);
  }
  return cleanup;
}

function pipeline(...streams) {
  const callback = typeof streams[streams.length - 1] === "function" ? streams.pop() : null;
  if (Array.isArray(streams[0])) streams = streams[0];
  let error = null;
  let finished = 0;
  const last = streams[streams.length - 1];
  const done = (err) => {
    if (err && !error) {
      error = err;
      for (const s of streams) s.destroy?.(err);
    }
    finished++;
    if (callback && (err || finished === streams.length)) {
      const cb = callback;
      // Only call once.
      streams.length = 0;
      cb(error);
    }
  };
  for (let i = 0; i < streams.length - 1; i++) {
    let s = streams[i];
    if (typeof s === "function") s = streams[i] = Readable.from(s());
    else if (!(s instanceof EventEmitter) && (s[Symbol.asyncIterator] || s[Symbol.iterator])) s = streams[i] = Readable.from(s);
    s.pipe(streams[i + 1]);
  }
  for (const s of streams.slice()) eos(s, (err) => done(err));
  return last;
}

const promises = {
  pipeline: (...streams) =>
    new Promise((resolve, reject) => {
      pipeline(...streams, (err) => (err ? reject(err) : resolve()));
    }),
  finished: (stream, opts) =>
    new Promise((resolve, reject) => {
      eos(stream, opts ?? {}, (err) => (err ? reject(err) : resolve()));
    }),
};

// Readable stream helpers (Node 17+): toArray, forEach, some/every/find, reduce, map/filter,
// take/drop, flatMap.
{
  const P = Readable.prototype;
  const signalOf = (o) => o?.signal;
  const abortIf = (o) => {
    if (signalOf(o)?.aborted) throw Object.assign(new Error("The operation was aborted"), { name: "AbortError", code: "ABORT_ERR" });
  };
  P.toArray = async function (options) {
    const out = [];
    for await (const chunk of this) {
      abortIf(options);
      out.push(chunk);
    }
    return out;
  };
  P.forEach = async function (fn, options) {
    for await (const chunk of this) {
      abortIf(options);
      await fn(chunk, { signal: signalOf(options) });
    }
  };
  P.some = async function (fn, options) {
    for await (const chunk of this) if (await fn(chunk, { signal: signalOf(options) })) return true;
    return false;
  };
  P.every = async function (fn, options) {
    for await (const chunk of this) if (!(await fn(chunk, { signal: signalOf(options) }))) return false;
    return true;
  };
  P.find = async function (fn, options) {
    for await (const chunk of this) if (await fn(chunk, { signal: signalOf(options) })) return chunk;
    return undefined;
  };
  P.reduce = async function (fn, ...init) {
    let acc = init[0];
    let first = init.length === 0;
    for await (const chunk of this) {
      if (first) {
        acc = chunk;
        first = false;
      } else acc = await fn(acc, chunk);
    }
    if (first) throw Object.assign(new TypeError("Reduce of an empty stream requires an initial value"), { code: "ERR_INVALID_ARGS" });
    return acc;
  };
  const derive = (src, gen) => Readable.from(gen(src), { objectMode: true });
  P.map = function (fn, options) {
    return derive(this, async function* (s) {
      for await (const chunk of s) yield await fn(chunk, { signal: signalOf(options) });
    });
  };
  P.filter = function (fn, options) {
    return derive(this, async function* (s) {
      for await (const chunk of s) if (await fn(chunk, { signal: signalOf(options) })) yield chunk;
    });
  };
  P.flatMap = function (fn, options) {
    return derive(this, async function* (s) {
      for await (const chunk of s) yield* await fn(chunk, { signal: signalOf(options) });
    });
  };
  P.take = function (n) {
    return derive(this, async function* (s) {
      if (n <= 0) return;
      let i = 0;
      for await (const chunk of s) {
        yield chunk;
        if (++i >= n) return;
      }
    });
  };
  P.drop = function (n) {
    return derive(this, async function* (s) {
      let i = 0;
      for await (const chunk of s) if (i++ >= n) yield chunk;
    });
  };
}

/**
 * Node's stream constructors are plain functions, and old-style subclasses initialize them with
 * `Writable.call(this, options)` (follow-redirects, readable-stream users). Export callable
 * wrappers around the classes: `new` and `class extends` construct the class; a plain call
 * constructs one and moves its state onto `this`.
 */
function callable(Cls) {
  const F = function (...args) {
    if (new.target) return Reflect.construct(Cls, args, new.target);
    if (this === undefined || this === null || this === globalThis) return Reflect.construct(Cls, args, F);
    const inst = Reflect.construct(Cls, args, this.constructor ?? F);
    for (const key of Reflect.ownKeys(inst)) {
      if (!Object.prototype.hasOwnProperty.call(this, key)) Object.defineProperty(this, key, Object.getOwnPropertyDescriptor(inst, key));
    }
    return undefined;
  };
  Object.defineProperty(F, "name", { value: Cls.name });
  F.prototype = Cls.prototype;
  Object.setPrototypeOf(F, Cls);
  Object.defineProperty(Cls.prototype, "constructor", { value: F, writable: true, configurable: true });
  return F;
}

Stream.Stream = Stream;
Stream.Readable = callable(Readable);
Stream.Writable = callable(Writable);
Stream.Duplex = callable(Duplex);
Stream.Transform = callable(Transform);
Stream.PassThrough = callable(PassThrough);
Stream.pipeline = pipeline;
Stream.finished = eos;
Stream.promises = promises;
Stream.isReadable = (s) => Boolean(s && s.readable && !s.destroyed);
Stream.isErrored = (s) => Boolean(s?.errored);
Stream.addAbortSignal = (signal, stream) => {
  signal.addEventListener("abort", () => stream.destroy(signal.reason), { once: true });
  return stream;
};
let defaultHighWaterMark = 65536;
Stream.getDefaultHighWaterMark = (objectMode) => (objectMode ? 16 : defaultHighWaterMark);
Stream.setDefaultHighWaterMark = (objectMode, value) => {
  if (!objectMode) defaultHighWaterMark = value;
};
Stream.isDisturbed = (s) => Boolean(s && (s[Symbol.for("nodejs.stream.disturbed")] ?? (s.readableDidRead || s.readableAborted || s.locked)));
Stream.isDestroyed = (s) => Boolean(s?.destroyed);
Stream.isWritable = (s) => Boolean(s && s.writable && !s.destroyed && !s.writableEnded);
Stream.destroy = (s, err) => s?.destroy?.(err);
Stream.duplexPair = (options) => {
  const a = new Duplex(options);
  const b = new Duplex(options);
  const wire = (from, to) => {
    from._write = (chunk, encoding, cb) => {
      to.push(chunk);
      cb();
    };
    from._final = (cb) => {
      to.push(null);
      cb();
    };
    from._read = () => {};
  };
  wire(a, b);
  wire(b, a);
  return [a, b];
};
Stream._isArrayBufferView = ArrayBuffer.isView;
Stream._isUint8Array = (v) => v instanceof Uint8Array;
Stream._uint8ArrayToBuffer = (u8) => internal.Buffer.from(u8.buffer, u8.byteOffset, u8.byteLength);

internal.stream = Stream;
internal.defineBuiltin("stream", () => Stream);
internal.defineBuiltin("stream/promises", () => promises);
internal.defineBuiltin("stream/web", () => ({
  ReadableStream: globalThis.ReadableStream,
  WritableStream: globalThis.WritableStream,
  TransformStream: globalThis.TransformStream,
  TextDecoderStream: globalThis.TextDecoderStream,
  TextEncoderStream: globalThis.TextEncoderStream,
}));
