// console, structuredClone, performance, crypto (web), minimal Intl.

// ---------------------------------------------------------------------------
// console
// ---------------------------------------------------------------------------

{
  let groupIndent = "";
  const counts = new Map();
  const timers = new Map();
  const write = (fd, args) => {
    let text = internal.format(...args);
    if (groupIndent) text = groupIndent + text.replaceAll("\n", `\n${groupIndent}`);
    ops.write(fd, text + "\n");
  };
  const console = {
    log: (...args) => write(1, args),
    info: (...args) => write(1, args),
    debug: (...args) => write(1, args),
    warn: (...args) => write(2, args),
    error: (...args) => write(2, args),
    trace: (...args) => {
      const err = new Error(internal.format(...args));
      err.name = "Trace";
      write(2, [err.stack]);
    },
    dir: (obj, options) => write(1, [internal.inspect(obj, { customInspect: false, ...options })]),
    dirxml: (...args) => write(1, args),
    table: (data) => {
      if (data === null || typeof data !== "object") return write(1, [data]);
      const rows = Object.entries(data);
      const cols = new Set();
      for (const [, row] of rows) if (row && typeof row === "object") for (const k of Object.keys(row)) cols.add(k);
      const header = ["(index)", ...cols];
      const body = rows.map(([k, row]) => [
        k,
        ...[...cols].map((c) => (row && typeof row === "object" && c in row ? internal.inspect(row[c]) : "")),
      ]);
      if (cols.size === 0) {
        header.push("Values");
        body.forEach((r, i) => r.push(internal.inspect(rows[i][1])));
      }
      const widths = header.map((h, i) => Math.max(h.length, ...body.map((r) => String(r[i]).length)) + 2);
      const line = (l, m, r) => l + widths.map((w) => "─".repeat(w)).join(m) + r;
      const fmt = (cells) => "│" + cells.map((c, i) => {
        const s = String(c);
        const pad = widths[i] - s.length;
        return " ".repeat(Math.floor(pad / 2)) + s + " ".repeat(Math.ceil(pad / 2));
      }).join("│") + "│";
      const out = [line("┌", "┬", "┐"), fmt(header), line("├", "┼", "┤"), ...body.map(fmt), line("└", "┴", "┘")];
      write(1, [out.join("\n")]);
    },
    assert: (cond, ...args) => {
      if (!cond) write(2, args.length ? ["Assertion failed:", ...args] : ["Assertion failed"]);
    },
    count: (label = "default") => {
      const n = (counts.get(label) ?? 0) + 1;
      counts.set(label, n);
      write(1, [`${label}: ${n}`]);
    },
    countReset: (label = "default") => counts.delete(label),
    group: (...args) => {
      if (args.length) write(1, args);
      groupIndent += "  ";
    },
    groupCollapsed: (...args) => console.group(...args),
    groupEnd: () => {
      groupIndent = groupIndent.slice(2);
    },
    time: (label = "default") => timers.set(label, ops.now()),
    timeLog: (label = "default", ...args) => {
      const start = timers.get(label);
      if (start !== undefined) write(1, [`${label}: ${(ops.now() - start).toFixed(3)}ms`, ...args]);
    },
    timeEnd: (label = "default") => {
      const start = timers.get(label);
      if (start !== undefined) {
        write(1, [`${label}: ${(ops.now() - start).toFixed(3)}ms`]);
        timers.delete(label);
      }
    },
    clear: () => {},
  };
  internal.hidden(globalThis, "console", console);
}

// ---------------------------------------------------------------------------
// structuredClone
// ---------------------------------------------------------------------------

function cloneError(message) {
  return new internal.DOMException(message, "DataCloneError");
}

function structuredClone(value, options = undefined) {
  const seen = new Map();
  const transfer = new Set(options?.transfer ?? []);
  const clone = (v) => {
    if (v === null || typeof v !== "object") {
      if (typeof v === "function") throw cloneError(`${v.toString().slice(0, 60)} could not be cloned.`);
      if (typeof v === "symbol") throw cloneError(`${v.toString()} could not be cloned.`);
      return v;
    }
    if (seen.has(v)) return seen.get(v);
    let out;
    if (Array.isArray(v)) {
      out = new Array(v.length);
      seen.set(v, out);
      for (let i = 0; i < v.length; i++) if (i in v) out[i] = clone(v[i]);
      return out;
    }
    if (v instanceof Date) out = new Date(v.getTime());
    else if (v instanceof RegExp) out = new RegExp(v.source, v.flags);
    else if (v instanceof Boolean) out = new Boolean(v.valueOf());
    else if (v instanceof Number) out = new Number(v.valueOf());
    else if (v instanceof String) out = new String(v.valueOf());
    else if (v instanceof ArrayBuffer) out = transfer.has(v) ? v : v.slice(0);
    else if (ArrayBuffer.isView(v)) {
      const buf = clone(v.buffer);
      if (v instanceof DataView) out = new DataView(buf, v.byteOffset, v.byteLength);
      else {
        const Ctor = Object.getPrototypeOf(Object.getPrototypeOf(v)) === Uint8Array.prototype ? Uint8Array : v.constructor;
        out = new Ctor(buf, v.byteOffset, v.length);
      }
    } else if (v instanceof Map) {
      out = new Map();
      seen.set(v, out);
      for (const [k, val] of v) out.set(clone(k), clone(val));
      return out;
    } else if (v instanceof Set) {
      out = new Set();
      seen.set(v, out);
      for (const val of v) out.add(clone(val));
      return out;
    } else if (v instanceof Error) {
      const Ctor = { EvalError, RangeError, ReferenceError, SyntaxError, TypeError, URIError }[v.name] ?? Error;
      out = new Ctor(v.message);
      seen.set(v, out);
      if (typeof v.stack === "string") internal.hidden(out, "stack", v.stack);
      if ("cause" in v) out.cause = clone(v.cause);
      return out;
    } else if (v instanceof Promise || v instanceof WeakMap || v instanceof WeakSet) {
      throw cloneError(`#<${v.constructor.name}> could not be cloned.`);
    } else {
      out = {};
      seen.set(v, out);
      for (const k of Object.keys(v)) {
        const val = v[k];
        // An own "__proto__" key stays a data property, as in the HTML structured clone.
        if (k === "__proto__") Object.defineProperty(out, k, { value: clone(val), writable: true, enumerable: true, configurable: true });
        else out[k] = clone(val);
      }
      return out;
    }
    seen.set(v, out);
    return out;
  };
  return clone(value);
}
internal.hidden(globalThis, "structuredClone", structuredClone);

// ---------------------------------------------------------------------------
// performance
// ---------------------------------------------------------------------------

{
  const entries = [];
  const performance = {
    now: () => ops.now(),
    get timeOrigin() {
      return ops.timeOrigin();
    },
    mark(name, options = {}) {
      const entry = { name, entryType: "mark", startTime: options.startTime ?? ops.now(), duration: 0, detail: options.detail ?? null };
      entries.push(entry);
      return entry;
    },
    measure(name, start, end) {
      const find = (n) => (typeof n === "string" ? entries.findLast((e) => e.name === n)?.startTime : n);
      let s = 0;
      let e = ops.now();
      if (typeof start === "object" && start) {
        s = find(start.start) ?? 0;
        e = find(start.end) ?? e;
      } else {
        if (start !== undefined) s = find(start) ?? 0;
        if (end !== undefined) e = find(end) ?? e;
      }
      const entry = { name, entryType: "measure", startTime: s, duration: e - s, detail: null };
      entries.push(entry);
      return entry;
    },
    getEntries: () => entries.slice(),
    getEntriesByName: (name, type) => entries.filter((e) => e.name === name && (!type || e.entryType === type)),
    getEntriesByType: (type) => entries.filter((e) => e.entryType === type),
    clearMarks: (name) => {
      for (let i = entries.length - 1; i >= 0; i--) if (entries[i].entryType === "mark" && (!name || entries[i].name === name)) entries.splice(i, 1);
    },
    clearMeasures: (name) => {
      for (let i = entries.length - 1; i >= 0; i--) if (entries[i].entryType === "measure" && (!name || entries[i].name === name)) entries.splice(i, 1);
    },
    toJSON() {
      return { timeOrigin: this.timeOrigin };
    },
    // Resource timing (undici reports fetches here) and Node extras; no observers exist.
    markResourceTiming(timingInfo, requestedUrl, initiatorType, global, cacheMode, bodyInfo, responseStatus) {
      return { name: String(requestedUrl), entryType: "resource", initiatorType, startTime: timingInfo?.startTime ?? 0, duration: 0, responseStatus };
    },
    clearResourceTimings() {},
    setResourceTimingBufferSize() {},
    eventLoopUtilization: () => ({ idle: 0, active: 0, utilization: 0 }),
    timerify: (fn) => fn,
  };
  internal.performance = performance;
  // With Node APIs, `performance` is perf_hooks' full implementation (a lazy global).
  if (!ops.config.nodeCompat) internal.hidden(globalThis, "performance", performance);
}

// ---------------------------------------------------------------------------
// Web Crypto (subset)
// ---------------------------------------------------------------------------

{
  const algName = (alg) => String(typeof alg === "object" ? alg.name : alg).toUpperCase();
  const subtle = {
    async digest(algorithm, data) {
      const map = { "SHA-1": "sha1", "SHA-256": "sha256", "SHA-384": "sha384", "SHA-512": "sha512" };
      const alg = map[algName(algorithm)];
      if (!alg) throw new internal.DOMException("Unrecognized algorithm name", "NotSupportedError");
      const bytes = ops.hash(alg, data);
      return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    },
  };
  // The rest of SubtleCrypto lives in node/webcrypto.js (loaded on first use; Node
  // compatibility only), which replaces these forwarders.
  for (const name of ["generateKey", "importKey", "exportKey", "encrypt", "decrypt", "sign", "verify", "deriveBits", "deriveKey", "wrapKey", "unwrapKey"]) {
    subtle[name] = async (...args) => {
      if (!internal.subtleImpl && internal.hasBuiltin?.("crypto")) internal.loadGlue("node/webcrypto.js");
      if (!internal.subtleImpl) throw new internal.DOMException(`crypto.subtle.${name} is not available`, "NotSupportedError");
      return internal.subtleImpl[name](...args);
    };
  }
  const crypto = {
    randomUUID: () => ops.randomUUID(),
    getRandomValues: (array) => ops.fillRandom(array),
    subtle,
  };
  internal.webcrypto = crypto;
  internal.hidden(globalThis, "crypto", crypto);
}

// ---------------------------------------------------------------------------
// Number/Date locale formatting and a minimal Intl (QuickJS has no Intl)
// ---------------------------------------------------------------------------

function groupDigits(intPart, sep = ",") {
  return intPart.replace(/\B(?=(\d{3})+(?!\d))/g, sep);
}

function formatNumber(n, opts = {}) {
  if (typeof n === "bigint") return groupDigits(n.toString());
  n = Number(n);
  if (!Number.isFinite(n)) return Number.isNaN(n) ? "NaN" : n > 0 ? "∞" : "-∞";
  const style = opts.style ?? "decimal";
  let value = style === "percent" ? n * 100 : n;
  const minFD = opts.minimumFractionDigits ?? (style === "currency" ? 2 : 0);
  const maxFD = Math.max(minFD, opts.maximumFractionDigits ?? (style === "currency" ? 2 : style === "percent" ? 0 : 3));
  let s = Math.abs(value).toFixed(maxFD);
  if (s.includes(".")) {
    let [i, f] = s.split(".");
    f = f.replace(/0+$/, "");
    if (f.length < minFD) f = f.padEnd(minFD, "0");
    s = f ? `${i}.${f}` : i;
  }
  let [intPart, frac] = s.split(".");
  if (opts.useGrouping !== false) intPart = groupDigits(intPart);
  let out = frac ? `${intPart}.${frac}` : intPart;
  if (opts.minimumIntegerDigits) out = out.padStart(opts.minimumIntegerDigits + (frac ? frac.length + 1 : 0), "0");
  if (value < 0 || Object.is(value, -0)) out = `-${out}`;
  if (style === "percent") out += "%";
  if (style === "currency") {
    const sym = { USD: "$", EUR: "€", GBP: "£", JPY: "¥" }[opts.currency] ?? `${opts.currency} `;
    out = out.startsWith("-") ? `-${sym}${out.slice(1)}` : `${sym}${out}`;
  }
  if (style === "unit" && opts.unit) out += ` ${opts.unit}`;
  return out;
}

const pad2 = (n) => String(n).padStart(2, "0");
function formatDate(d, opts = {}, kind = "datetime") {
  if (Number.isNaN(d.getTime())) return "Invalid Date";
  const date = `${d.getMonth() + 1}/${d.getDate()}/${d.getFullYear()}`;
  const h = d.getHours();
  const time = `${h % 12 === 0 ? 12 : h % 12}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())} ${h < 12 ? "AM" : "PM"}`;
  if (kind === "date") return date;
  if (kind === "time") return time;
  return `${date}, ${time}`;
}

Object.defineProperty(Number.prototype, "toLocaleString", {
  value: function toLocaleString(_locales, options) {
    return formatNumber(this.valueOf(), options);
  },
  writable: true,
  configurable: true,
});
Object.defineProperty(BigInt.prototype, "toLocaleString", {
  value: function toLocaleString() {
    return groupDigits(this.valueOf().toString());
  },
  writable: true,
  configurable: true,
});
for (const [name, kind] of [
  ["toLocaleString", "datetime"],
  ["toLocaleDateString", "date"],
  ["toLocaleTimeString", "time"],
]) {
  Object.defineProperty(Date.prototype, name, {
    value: function (_locales, options) {
      return formatDate(this, options, kind);
    },
    writable: true,
    configurable: true,
  });
}

if (typeof Intl === "undefined") {
  class NumberFormat {
    #opts;
    constructor(_locales, options = {}) {
      this.#opts = { ...options };
    }
    format(n) {
      return formatNumber(n, this.#opts);
    }
    formatToParts(n) {
      return [{ type: "literal", value: this.format(n) }];
    }
    resolvedOptions() {
      return { locale: "en-US", numberingSystem: "latn", style: "decimal", ...this.#opts };
    }
    static supportedLocalesOf() {
      return ["en-US"];
    }
  }
  class DateTimeFormat {
    #opts;
    constructor(_locales, options = {}) {
      this.#opts = { ...options };
    }
    format(d = new Date()) {
      const date = d instanceof Date ? d : new Date(d);
      const o = this.#opts;
      const hasDate = o.year || o.month || o.day || o.weekday || o.dateStyle;
      const hasTime = o.hour || o.minute || o.second || o.timeStyle;
      return formatDate(date, o, hasDate && !hasTime ? "date" : hasTime && !hasDate ? "time" : hasDate ? "datetime" : "date");
    }
    formatToParts(d) {
      return [{ type: "literal", value: this.format(d) }];
    }
    resolvedOptions() {
      let timeZone = "UTC";
      try {
        timeZone = ops.config?.env?.TZ || "UTC";
      } catch {}
      return { locale: "en-US", calendar: "gregory", numberingSystem: "latn", timeZone, ...this.#opts };
    }
    static supportedLocalesOf() {
      return ["en-US"];
    }
  }
  class Collator {
    #opts;
    constructor(_locales, options = {}) {
      this.#opts = options;
    }
    compare = (a, b) => {
      let x = String(a);
      let y = String(b);
      if (this.#opts.sensitivity === "base" || this.#opts.sensitivity === "accent") {
        x = x.toLowerCase();
        y = y.toLowerCase();
      }
      if (this.#opts.numeric) {
        const re = /(\d+)|(\D+)/g;
        const xa = x.match(re) ?? [];
        const ya = y.match(re) ?? [];
        for (let i = 0; i < Math.min(xa.length, ya.length); i++) {
          const xn = /^\d/.test(xa[i]);
          const yn = /^\d/.test(ya[i]);
          if (xn && yn) {
            const d = Number(xa[i]) - Number(ya[i]);
            if (d) return d < 0 ? -1 : 1;
          } else if (xa[i] !== ya[i]) return xa[i].localeCompare(ya[i]);
        }
        return xa.length - ya.length;
      }
      return x.localeCompare(y);
    };
    resolvedOptions() {
      return { locale: "en-US", ...this.#opts };
    }
  }
  class Segmenter {
    #granularity;
    constructor(_locales, options = {}) {
      this.#granularity = options.granularity ?? "grapheme";
    }
    segment(input) {
      const str = String(input);
      const granularity = this.#granularity;
      const pieces = granularity === "grapheme" ? ops.graphemes(str) : ops.words(str);
      const segments = [];
      let index = 0;
      for (const segment of pieces) {
        const item = { segment, index, input: str };
        if (granularity === "word") item.isWordLike = /[\p{L}\p{N}]/u.test(segment);
        segments.push(item);
        index += segment.length;
      }
      return {
        [Symbol.iterator]: () => segments[Symbol.iterator](),
        containing: (i = 0) => segments.find((s) => i >= s.index && i < s.index + s.segment.length),
      };
    }
    resolvedOptions() {
      return { locale: "en-US", granularity: this.#granularity };
    }
  }
  class PluralRules {
    select(n) {
      return Number(n) === 1 ? "one" : "other";
    }
    resolvedOptions() {
      return { locale: "en-US", type: "cardinal" };
    }
  }
  class RelativeTimeFormat {
    format(value, unit) {
      const u = String(unit).replace(/s$/, "");
      const abs = Math.abs(value);
      const label = `${abs} ${u}${abs === 1 ? "" : "s"}`;
      return value < 0 ? `${label} ago` : `in ${label}`;
    }
  }
  class ListFormat {
    #opts;
    constructor(_locales, options = {}) {
      this.#opts = options;
    }
    format(list) {
      const items = Array.from(list, String);
      const word = this.#opts.type === "disjunction" ? "or" : "and";
      if (items.length <= 1) return items.join("");
      if (items.length === 2) return `${items[0]} ${word} ${items[1]}`;
      return `${items.slice(0, -1).join(", ")}, ${word} ${items[items.length - 1]}`;
    }
  }
  const Intl = {
    NumberFormat,
    DateTimeFormat,
    Collator,
    Segmenter,
    PluralRules,
    RelativeTimeFormat,
    ListFormat,
    getCanonicalLocales: (l) => (l === undefined ? [] : Array.isArray(l) ? l : [l]),
    supportedValuesOf: () => [],
  };
  internal.hidden(globalThis, "Intl", Intl);
}
