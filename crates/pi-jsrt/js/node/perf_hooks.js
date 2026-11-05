// node:perf_hooks and the Node `performance` global: the user timing timeline (marks and
// measures), PerformanceObserver, timerify, histograms, monitorEventLoopDelay and event loop
// utilization (the engine measures the time its loop spends waiting).

const { DOMException } = internal;

function codeError(Ctor, code, message) {
  const err = new Ctor(message);
  err.code = code;
  return err;
}

const kEntry = Symbol("entry");
const supportedEntryTypes = Object.freeze(["dns", "function", "gc", "http", "http2", "mark", "measure", "net", "resource"]);

class PerformanceEntry {
  constructor(token, name, type, start, duration, detail) {
    if (token !== kEntry) throw codeError(TypeError, "ERR_ILLEGAL_CONSTRUCTOR", "Illegal constructor");
    this._name = name;
    this._type = type;
    this._start = start;
    this._duration = duration;
    this._detail = detail;
  }
  get name() {
    return this._name;
  }
  get entryType() {
    return this._type;
  }
  get startTime() {
    return this._start;
  }
  get duration() {
    return this._duration;
  }
  get detail() {
    return this._detail;
  }
  toJSON() {
    return { name: this.name, entryType: this.entryType, startTime: this.startTime, duration: this.duration, detail: this.detail };
  }
  [Symbol.for("nodejs.util.inspect.custom")](depth, options, inspect) {
    return `${this.constructor.name} ${inspect(this.toJSON(), options)}`;
  }
}
for (const key of ["_name", "_type", "_start", "_duration", "_detail"]) {
  Object.defineProperty(PerformanceEntry.prototype, key, { writable: true, enumerable: false, configurable: true, value: undefined });
}
function hide(entry) {
  for (const key of ["_name", "_type", "_start", "_duration", "_detail"]) {
    Object.defineProperty(entry, key, { value: entry[key], writable: true, enumerable: false, configurable: true });
  }
  return entry;
}

function validateTimestamp(value, name) {
  if (typeof value !== "number" || value < 0 || Number.isNaN(value)) {
    throw codeError(TypeError, "ERR_PERFORMANCE_INVALID_TIMESTAMP", `${value} is not a valid timestamp`);
  }
  void name;
}

class PerformanceMark extends PerformanceEntry {
  constructor(name, options = {}) {
    if (arguments.length === 0) throw codeError(TypeError, "ERR_MISSING_ARGS", 'The "name" argument must be specified');
    name = `${name}`;
    if (options !== null && typeof options !== "object") {
      throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options" argument must be of type object. Received ${internal.describeReceived(options)}`);
    }
    const startTime = options?.startTime ?? now();
    validateTimestamp(startTime, "startTime");
    let detail = options?.detail;
    detail = detail != null ? structuredClone(detail) : null;
    super(kEntry, name, "mark", startTime, 0, detail);
    hide(this);
  }
}

class PerformanceMeasure extends PerformanceEntry {
  constructor(token, ...args) {
    super(token, ...args);
    hide(this);
  }
}

class PerformanceResourceTiming extends PerformanceEntry {
  constructor(token, ...args) {
    super(token, ...args);
    hide(this);
  }
}

class PerformanceNodeTiming {
  get name() {
    return "node";
  }
  get entryType() {
    return "node";
  }
  get startTime() {
    return 0;
  }
  get duration() {
    return now();
  }
  get nodeStart() {
    return 0.5;
  }
  get v8Start() {
    return 1;
  }
  get environment() {
    return 2;
  }
  get bootstrapComplete() {
    return 3;
  }
  get loopStart() {
    return ops.loopTiming()[0];
  }
  get loopExit() {
    return -1;
  }
  get idleTime() {
    return ops.loopTiming()[1];
  }
  toJSON() {
    const out = {};
    for (const k of ["name", "entryType", "startTime", "duration", "nodeStart", "v8Start", "bootstrapComplete", "environment", "loopStart", "loopExit", "idleTime"]) out[k] = this[k];
    return out;
  }
}

const now = () => ops.now();
const timeline = []; // marks and measures
const resourceTimings = [];
const sortByStart = (a, b) => a.startTime - b.startTime;

// ---------------------------------------------------------------------------
// Observers
// ---------------------------------------------------------------------------

const observers = new Set();
const pending = new Set();
let dispatchScheduled = false;

function enqueue(entry) {
  for (const observer of observers) observer._maybeBuffer(entry);
}

function scheduleDispatch(observer) {
  pending.add(observer);
  if (dispatchScheduled) return;
  dispatchScheduled = true;
  setImmediate(() => {
    dispatchScheduled = false;
    const list = [...pending];
    pending.clear();
    for (const o of list) o._dispatch();
  });
}

class PerformanceObserverEntryList {
  constructor(token, entries) {
    if (token !== kEntry) throw codeError(TypeError, "ERR_ILLEGAL_CONSTRUCTOR", "Illegal constructor");
    this._entries = entries.sort(sortByStart);
    Object.defineProperty(this, "_entries", { enumerable: false });
  }
  getEntries() {
    return this._entries.slice();
  }
  getEntriesByType(type) {
    type = `${type}`;
    return this._entries.filter((e) => e.entryType === type);
  }
  getEntriesByName(name, type) {
    name = `${name}`;
    return this._entries.filter((e) => e.name === name && (type === undefined || e.entryType === `${type}`));
  }
}

class PerformanceObserver {
  constructor(callback) {
    if (typeof callback !== "function") {
      throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "callback" argument must be of type function. Received ${internal.describeReceived(callback)}`);
    }
    this._callback = callback;
    this._buffer = [];
    this._types = new Set();
    this._mode = undefined;
    for (const k of ["_callback", "_buffer", "_types", "_mode"]) Object.defineProperty(this, k, { enumerable: false });
  }
  static get supportedEntryTypes() {
    return supportedEntryTypes;
  }
  observe(options = {}) {
    if (options === null || typeof options !== "object") {
      throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options" argument must be of type object. Received ${internal.describeReceived(options)}`);
    }
    const { entryTypes, type, buffered } = options;
    if (entryTypes === undefined && type === undefined) throw codeError(TypeError, "ERR_MISSING_ARGS", 'The "options.entryTypes" or "options.type" argument must be specified');
    if (entryTypes != null && type != null) {
      throw codeError(TypeError, "ERR_INVALID_ARG_VALUE", `The property 'options.entryTypes' can not be used with 'options.type'. Received ${internal.inspect(entryTypes)}`);
    }
    if (this._mode === "multiple" && type != null) throw codeError(Error, "ERR_INVALID_ARG_VALUE", "PerformanceObserver can not change to single observer type");
    if (this._mode === "single" && entryTypes != null) throw codeError(Error, "ERR_INVALID_ARG_VALUE", "PerformanceObserver can not change to multiple observations");
    if (entryTypes != null) {
      if (!Array.isArray(entryTypes)) {
        throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options.entryTypes" property must be an instance of Array. Received ${internal.describeReceived(entryTypes)}`);
      }
      this._mode = "multiple";
      this._types = new Set(entryTypes.filter((t) => supportedEntryTypes.includes(t)));
    } else {
      if (!supportedEntryTypes.includes(type)) return;
      this._mode = "single";
      this._types.add(type);
      if (buffered) {
        const entries = type === "resource" ? resourceTimings : timeline.filter((e) => e.entryType === type);
        if (entries.length) {
          this._buffer.push(...entries);
          scheduleDispatch(this);
        }
      }
    }
    if (this._types.size) observers.add(this);
    else observers.delete(this);
  }
  disconnect() {
    observers.delete(this);
    pending.delete(this);
    this._buffer = [];
    this._types.clear();
    this._mode = undefined;
  }
  takeRecords() {
    const list = this._buffer;
    this._buffer = [];
    return list;
  }
  _maybeBuffer(entry) {
    if (!this._types.has(entry.entryType)) return;
    this._buffer.push(entry);
    scheduleDispatch(this);
  }
  _dispatch() {
    const entries = this.takeRecords();
    if (!entries.length) return;
    this._callback(new PerformanceObserverEntryList(kEntry, entries), this);
  }
  [Symbol.for("nodejs.util.inspect.custom")]() {
    return `PerformanceObserver { connected: ${observers.has(this)}, pending: ${pending.has(this)}, entryTypes: ${internal.inspect([...this._types])}, buffer: ${internal.inspect(this._buffer)} }`;
  }
}

// ---------------------------------------------------------------------------
// User timing
// ---------------------------------------------------------------------------

function mark(name, options) {
  const entry = new PerformanceMark(name, options);
  enqueue(entry);
  timeline.push(entry);
  return entry;
}

function markTime(name, which) {
  if (typeof name === "number") {
    validateTimestamp(name, which);
    return name;
  }
  if (typeof name !== "string") name = `${name}`;
  const nodeTiming = performance.nodeTiming;
  if (Object.prototype.hasOwnProperty.call(PerformanceNodeTiming.prototype, name)) {
    const v = nodeTiming[name];
    if (typeof v === "number") return v;
  }
  for (let i = timeline.length - 1; i >= 0; i--) {
    if (timeline[i].entryType === "mark" && timeline[i].name === name) return timeline[i].startTime;
  }
  throw new DOMException(`The "${name}" performance mark has not been set`, "SyntaxError");
}

function measure(name, startOrMeasureOptions, endMark) {
  name = `${name}`;
  let start;
  let end;
  let duration;
  let detail = null;
  if (startOrMeasureOptions !== null && typeof startOrMeasureOptions === "object") {
    ({ start, end, duration, detail = null } = startOrMeasureOptions);
    if (start === undefined && end === undefined) {
      throw codeError(TypeError, "ERR_INVALID_ARG_VALUE", "The property 'options' One of options.start or options.end is required. Received {}");
    }
    if (start !== undefined && end !== undefined && duration !== undefined) {
      throw codeError(TypeError, "ERR_INVALID_ARG_VALUE", "The property 'options.duration' is invalid. Received " + internal.inspect(duration));
    }
  } else {
    start = startOrMeasureOptions;
  }
  if (endMark !== undefined) end = endMark;
  let endTime;
  if (end !== undefined) endTime = markTime(end, "end");
  else if (start !== undefined && duration !== undefined) endTime = markTime(start, "start") + duration;
  else endTime = now();
  let startTime;
  if (start !== undefined) startTime = markTime(start, "start");
  else if (duration !== undefined && end !== undefined) startTime = endTime - duration;
  else startTime = 0;
  detail = detail != null ? structuredClone(detail) : null;
  const entry = new PerformanceMeasure(kEntry, name, "measure", startTime, endTime - startTime, detail);
  enqueue(entry);
  timeline.push(entry);
  return entry;
}

function clearEntries(type, name) {
  for (let i = timeline.length - 1; i >= 0; i--) {
    if (timeline[i].entryType === type && (name === undefined || timeline[i].name === `${name}`)) timeline.splice(i, 1);
  }
}

function getEntries() {
  return [...timeline, ...resourceTimings].sort(sortByStart);
}

// ---------------------------------------------------------------------------
// timerify, event loop utilization
// ---------------------------------------------------------------------------

function timerify(fn, options = {}) {
  if (typeof fn !== "function") {
    throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "fn" argument must be of type function. Received ${internal.describeReceived(fn)}`);
  }
  if (options === null || typeof options !== "object") {
    throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options" argument must be of type object. Received ${internal.describeReceived(options)}`);
  }
  const { histogram } = options;
  if (fn._timerified) return fn._timerified;
  function timerified(...args) {
    const start = now();
    const result = new.target ? Reflect.construct(fn, args, fn) : Reflect.apply(fn, this, args);
    const finish = () => {
      const duration = now() - start;
      if (histogram) histogram.record(Math.max(1, Math.round(duration * 1e6)));
      enqueue(new PerformanceEntry(kEntry, fn.name, "function", start, duration, args));
    };
    if (!new.target && result && typeof result.finally === "function") return result.finally(finish);
    finish();
    return result;
  }
  Object.defineProperties(timerified, {
    length: { configurable: false, enumerable: true, value: fn.length },
    name: { configurable: false, enumerable: true, value: `timerified ${fn.name}` },
  });
  Object.defineProperty(fn, "_timerified", { value: timerified, configurable: true, enumerable: false, writable: true });
  return timerified;
}

function eventLoopUtilization(util1, util2) {
  const [loopStart, idle] = ops.loopTiming();
  if (loopStart <= 0) return { idle: 0, active: 0, utilization: 0 };
  if (util2) {
    const idleDelta = util1.idle - util2.idle;
    const activeDelta = util1.active - util2.active;
    const utilization = activeDelta / (idleDelta + activeDelta);
    return { idle: idleDelta, active: activeDelta, utilization };
  }
  const active = now() - loopStart - idle;
  if (!util1) return { idle, active, utilization: active / (idle + active) };
  const idleDelta = idle - util1.idle;
  const activeDelta = active - util1.active;
  const utilization = activeDelta / (idleDelta + activeDelta);
  return { idle: idleDelta, active: activeDelta, utilization };
}

// ---------------------------------------------------------------------------
// Histograms
// ---------------------------------------------------------------------------

class Histogram {
  constructor(lowest = 1, highest = Number.MAX_SAFE_INTEGER) {
    this._lowest = lowest;
    this._highest = highest;
    this.reset();
    for (const k of ["_lowest", "_highest", "_counts", "_count", "_sum", "_sumSq", "_min", "_max", "_exceeds", "_sorted"]) Object.defineProperty(this, k, { enumerable: false, writable: true });
  }
  reset() {
    this._counts = new Map();
    this._count = 0;
    this._sum = 0;
    this._sumSq = 0;
    this._min = 9223372036854776000;
    this._max = 0;
    this._exceeds = 0;
    this._sorted = null;
  }
  _record(value) {
    if (value > this._highest) {
      this._exceeds++;
      return;
    }
    this._counts.set(value, (this._counts.get(value) ?? 0) + 1);
    this._count++;
    this._sum += value;
    this._sumSq += value * value;
    if (value < this._min) this._min = value;
    if (value > this._max) this._max = value;
    this._sorted = null;
  }
  get count() {
    return this._count;
  }
  get countBigInt() {
    return BigInt(this._count);
  }
  get min() {
    return this._min;
  }
  get minBigInt() {
    return BigInt(this._min);
  }
  get max() {
    return this._max;
  }
  get maxBigInt() {
    return BigInt(this._max);
  }
  get mean() {
    return this._count ? this._sum / this._count : NaN;
  }
  get stddev() {
    if (!this._count) return NaN;
    const mean = this.mean;
    return Math.sqrt(Math.max(0, this._sumSq / this._count - mean * mean));
  }
  get exceeds() {
    return this._exceeds;
  }
  get exceedsBigInt() {
    return BigInt(this._exceeds);
  }
  percentile(percentile) {
    if (typeof percentile !== "number") {
      throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "percentile" argument must be of type number. Received ${internal.describeReceived(percentile)}`);
    }
    if (!(percentile > 0 && percentile <= 100)) {
      throw codeError(RangeError, "ERR_OUT_OF_RANGE", `The value of "percentile" is out of range. It must be > 0 && <= 100. Received ${percentile}`);
    }
    if (!this._count) return 0;
    this._sorted ??= [...this._counts.entries()].sort((a, b) => a[0] - b[0]);
    const target = Math.ceil((percentile / 100) * this._count);
    let seen = 0;
    for (const [value, n] of this._sorted) {
      seen += n;
      if (seen >= target) return value;
    }
    return this._max;
  }
  percentileBigInt(percentile) {
    return BigInt(this.percentile(percentile));
  }
  get percentiles() {
    const out = new Map();
    if (!this._count) {
      out.set(100, 0);
      return out;
    }
    for (const p of [0, 50, 75, 87.5, 93.75, 96.875, 98.4375, 99.21875, 100]) out.set(p, p === 0 ? this._min : this.percentile(p));
    return out;
  }
  get percentilesBigInt() {
    return new Map([...this.percentiles].map(([k, v]) => [k, BigInt(v)]));
  }
  toJSON() {
    return { count: this.count, min: this.min, max: this.max, mean: this.mean, exceeds: this.exceeds, stddev: this.stddev, percentiles: Object.fromEntries(this.percentiles) };
  }
}

class RecordableHistogram extends Histogram {
  record(val) {
    if (typeof val === "bigint") val = Number(val);
    if (typeof val !== "number") {
      throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "val" argument must be of type number or bigint. Received ${internal.describeReceived(val)}`);
    }
    if (val < 1 || !Number.isFinite(val)) {
      throw codeError(RangeError, "ERR_OUT_OF_RANGE", `The value of "val" is out of range. It must be >= 1 && <= 9007199254740991. Received ${val}`);
    }
    this._record(val);
  }
  recordDelta() {
    const t = ops.hrtime();
    if (this._prev !== undefined) this._record(Math.max(1, Math.round(t - this._prev)));
    this._prev = t;
  }
  add(other) {
    for (const [value, n] of other._counts) for (let i = 0; i < n; i++) this._record(value);
  }
}

class IntervalHistogram extends Histogram {
  constructor(resolution) {
    super();
    this._resolution = resolution;
    this._timer = null;
    Object.defineProperty(this, "_resolution", { enumerable: false });
    Object.defineProperty(this, "_timer", { enumerable: false, writable: true });
  }
  enable() {
    if (this._timer) return false;
    let last = ops.hrtime();
    this._timer = setInterval(() => {
      const t = ops.hrtime();
      // Delay beyond the expected interval, in nanoseconds (like libuv's timer-based sampling).
      const delay = Math.max(1, Math.round(t - last));
      last = t;
      this._record(delay);
    }, this._resolution);
    this._timer.unref();
    return true;
  }
  disable() {
    if (!this._timer) return false;
    clearInterval(this._timer);
    this._timer = null;
    return true;
  }
  [Symbol.dispose]() {
    this.disable();
  }
}

function createHistogram(options = {}) {
  const { lowest = 1, highest = Number.MAX_SAFE_INTEGER } = options ?? {};
  return new RecordableHistogram(lowest, highest);
}

function monitorEventLoopDelay(options = {}) {
  if (options === null || typeof options !== "object") {
    throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options" argument must be of type object. Received ${internal.describeReceived(options)}`);
  }
  const { resolution = 10 } = options;
  if (typeof resolution !== "number") {
    throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options.resolution" property must be of type number. Received ${internal.describeReceived(resolution)}`);
  }
  if (resolution <= 0 || !Number.isSafeInteger(resolution)) {
    throw codeError(RangeError, "ERR_OUT_OF_RANGE", `The value of "options.resolution" is out of range. It must be > 0 && <= 9007199254740991. Received ${resolution}`);
  }
  return new IntervalHistogram(resolution);
}

// ---------------------------------------------------------------------------
// Performance
// ---------------------------------------------------------------------------

const nodeTiming = new PerformanceNodeTiming();

class Performance extends internal.EventTarget {
  constructor(token) {
    if (token !== kEntry) throw codeError(TypeError, "ERR_ILLEGAL_CONSTRUCTOR", "Illegal constructor");
    super();
  }
  now() {
    return now();
  }
  get timeOrigin() {
    return ops.timeOrigin();
  }
  mark(name, options) {
    return mark(name, options);
  }
  measure(name, startOrMeasureOptions, endMark) {
    return measure(name, startOrMeasureOptions, endMark);
  }
  clearMarks(name) {
    clearEntries("mark", name);
  }
  clearMeasures(name) {
    clearEntries("measure", name);
  }
  clearResourceTimings(name) {
    for (let i = resourceTimings.length - 1; i >= 0; i--) if (name === undefined || resourceTimings[i].name === `${name}`) resourceTimings.splice(i, 1);
  }
  getEntries() {
    return getEntries();
  }
  getEntriesByName(name, type) {
    name = `${name}`;
    return getEntries().filter((e) => e.name === name && (type === undefined || e.entryType === `${type}`));
  }
  getEntriesByType(type) {
    type = `${type}`;
    return getEntries().filter((e) => e.entryType === type);
  }
  get nodeTiming() {
    return nodeTiming;
  }
  get eventLoopUtilization() {
    return eventLoopUtilization;
  }
  get timerify() {
    return timerify;
  }
  setResourceTimingBufferSize() {}
  markResourceTiming(timingInfo, requestedUrl, initiatorType, global, cacheMode, bodyInfo, responseStatus) {
    const start = timingInfo?.startTime ?? now();
    const end = timingInfo?.endTime ?? start;
    const entry = new PerformanceResourceTiming(kEntry, String(requestedUrl), "resource", start, end - start, undefined);
    Object.defineProperties(entry, {
      initiatorType: { value: initiatorType, enumerable: true },
      responseStatus: { value: responseStatus, enumerable: true },
    });
    resourceTimings.push(entry);
    enqueue(entry);
    return entry;
  }
  toJSON() {
    return { nodeTiming: this.nodeTiming, timeOrigin: this.timeOrigin, eventLoopUtilization: this.eventLoopUtilization() };
  }
  get [Symbol.toStringTag]() {
    return "Performance";
  }
}

const performance = new Performance(kEntry);
internal.performance = performance;
for (const [name, value] of Object.entries({
  performance,
  Performance,
  PerformanceEntry,
  PerformanceMark,
  PerformanceMeasure,
  PerformanceObserver,
  PerformanceObserverEntryList,
  PerformanceResourceTiming,
})) {
  internal.hidden(globalThis, name, value);
}

const constants = {
  NODE_PERFORMANCE_GC_MAJOR: 4,
  NODE_PERFORMANCE_GC_MINOR: 1,
  NODE_PERFORMANCE_GC_INCREMENTAL: 8,
  NODE_PERFORMANCE_GC_WEAKCB: 16,
  NODE_PERFORMANCE_GC_FLAGS_NO: 0,
  NODE_PERFORMANCE_GC_FLAGS_CONSTRUCT_RETAINED: 2,
  NODE_PERFORMANCE_GC_FLAGS_FORCED: 4,
  NODE_PERFORMANCE_GC_FLAGS_SYNCHRONOUS_PHANTOM_PROCESSING: 8,
  NODE_PERFORMANCE_GC_FLAGS_ALL_AVAILABLE_GARBAGE: 16,
  NODE_PERFORMANCE_GC_FLAGS_ALL_EXTERNAL_MEMORY: 32,
  NODE_PERFORMANCE_GC_FLAGS_SCHEDULE_IDLE: 64,
};

internal.defineBuiltin("perf_hooks", () => ({
  performance,
  Performance,
  PerformanceEntry,
  PerformanceMark,
  PerformanceMeasure,
  PerformanceObserver,
  PerformanceObserverEntryList,
  PerformanceResourceTiming,
  monitorEventLoopDelay,
  createHistogram,
  constants,
}));
