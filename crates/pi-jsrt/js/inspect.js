// util.inspect / util.format (Node-compatible formatting, simplified).

const customInspect = Symbol.for("nodejs.util.inspect.custom");
internal.customInspect = customInspect;

const defaultOptions = {
  depth: 2,
  colors: false,
  showHidden: false,
  breakLength: 80,
  compact: 3,
  sorted: false,
  maxArrayLength: 100,
  maxStringLength: 10000,
  getters: false,
  numericSeparator: false,
};

const styles = {
  special: "cyan",
  number: "yellow",
  bigint: "yellow",
  boolean: "yellow",
  undefined: "grey",
  null: "bold",
  string: "green",
  symbol: "green",
  date: "magenta",
  regexp: "red",
  module: "underline",
};
const colorCodes = {
  bold: [1, 22],
  italic: [3, 23],
  underline: [4, 24],
  inverse: [7, 27],
  white: [37, 39],
  grey: [90, 39],
  gray: [90, 39],
  black: [30, 39],
  blue: [34, 39],
  cyan: [36, 39],
  green: [32, 39],
  magenta: [35, 39],
  red: [31, 39],
  yellow: [33, 39],
};

function stylize(ctx, str, type) {
  if (!ctx.colors) return str;
  const style = styles[type];
  const code = style && colorCodes[style];
  return code ? `\x1b[${code[0]}m${str}\x1b[${code[1]}m` : str;
}

const identRe = /^[A-Za-z_$][A-Za-z0-9_$]*$/;

function quoteString(str) {
  let quote = "'";
  if (str.includes("'")) {
    if (!str.includes('"')) quote = '"';
    else if (!str.includes("`") && !str.includes("${")) quote = "`";
  }
  let out = "";
  for (const ch of str) {
    const c = ch.codePointAt(0);
    if (ch === quote || ch === "\\") out += "\\" + ch;
    else if (ch === "\n") out += "\\n";
    else if (ch === "\t") out += "\\t";
    else if (ch === "\r") out += "\\r";
    else if (ch === "\b") out += "\\b";
    else if (ch === "\f") out += "\\f";
    else if (ch === "\v") out += "\\v";
    else if (c < 0x20 || c === 0x7f) out += "\\x" + c.toString(16).padStart(2, "0").toUpperCase();
    else out += ch;
  }
  return quote + out + quote;
}

function formatKey(key) {
  if (typeof key === "symbol") return `[${key.toString()}]`;
  if (identRe.test(key)) return key;
  return quoteString(key);
}

function getCtorName(obj) {
  let proto = obj;
  while (proto) {
    const desc = Object.getOwnPropertyDescriptor(proto, "constructor");
    if (desc && typeof desc.value === "function" && desc.value.name) return desc.value.name;
    proto = Object.getPrototypeOf(proto);
  }
  return null;
}

function formatPrimitive(ctx, value) {
  switch (typeof value) {
    case "string": {
      let s = value;
      let trailer = "";
      if (s.length > ctx.maxStringLength) {
        trailer = `... ${s.length - ctx.maxStringLength} more character${s.length - ctx.maxStringLength > 1 ? "s" : ""}`;
        s = s.slice(0, ctx.maxStringLength);
      }
      return stylize(ctx, quoteString(s), "string") + trailer;
    }
    case "number":
      return stylize(ctx, Object.is(value, -0) ? "-0" : String(value), "number");
    case "bigint":
      return stylize(ctx, `${value}n`, "bigint");
    case "boolean":
      return stylize(ctx, String(value), "boolean");
    case "undefined":
      return stylize(ctx, "undefined", "undefined");
    case "symbol":
      return stylize(ctx, value.toString(), "symbol");
    default:
      return stylize(ctx, "null", "null");
  }
}

function formatError(err) {
  let stack = typeof err.stack === "string" && err.stack ? err.stack : `${err.name}: ${err.message}`;
  const name = err.name || "Error";
  if (!stack.includes(name) && err.message) stack = `${name}: ${err.message}\n${stack}`;
  return stack;
}

function reduceToSingle(ctx, output, base, braces, extrasType, indentation) {
  const start = output.length + ctx.indentationLvl + braces[0].length + base.length + 10;
  if (ctx.compact !== false && typeof ctx.compact === "number" && ctx.compact >= 1) {
    const entries = output.length;
    if (extrasType === "object" || entries <= ctx.compact * 4 || true) {
      const totalLength = output.reduce((sum, s) => sum + s.length + 2, 0) + start;
      if (totalLength <= ctx.breakLength && !output.some((s) => s.includes("\n"))) {
        return `${base ? `${base} ` : ""}${braces[0]} ${output.join(", ")} ${braces[1]}`;
      }
    }
  }
  const ind = `\n${" ".repeat(indentation)}`;
  return `${base ? `${base} ` : ""}${braces[0]}${ind}  ${output.join(`,${ind}  `)}${ind}${braces[1]}`;
}

function groupArray(ctx, output) {
  // Node groups long arrays of short items into columns; keep it simple: chunk by 16 when all short.
  if (output.length > 6 && output.every((s) => s.length <= 6)) {
    const perLine = Math.min(16, Math.max(1, Math.floor((ctx.breakLength - ctx.indentationLvl) / 8)));
    const rows = [];
    for (let i = 0; i < output.length; i += perLine) {
      const width = Math.max(...output.map((s) => s.length));
      rows.push(
        output
          .slice(i, i + perLine)
          .map((s, j, arr) => (j < arr.length - 1 ? (s + ",").padEnd(width + 1) : s))
          .join(" "),
      );
    }
    return rows;
  }
  return output;
}

function formatValue(ctx, value, recurseTimes, typedArray) {
  if (value === null || (typeof value !== "object" && typeof value !== "function")) {
    return formatPrimitive(ctx, value);
  }
  if (ctx.customInspect !== false) {
    const custom = value[customInspect];
    if (typeof custom === "function" && custom !== inspect) {
      const ret = custom.call(value, recurseTimes, { ...ctx, stylize: (s, t) => stylize(ctx, s, t) }, inspect);
      if (ret !== value) {
        if (typeof ret !== "string") return formatValue(ctx, ret, recurseTimes);
        return ret.replaceAll("\n", `\n${" ".repeat(ctx.indentationLvl)}`);
      }
    }
  }
  if (ctx.seen.includes(value)) {
    ctx.circular = true;
    return stylize(ctx, "[Circular *1]", "special");
  }
  return formatRaw(ctx, value, recurseTimes, typedArray);
}

function formatRaw(ctx, value, recurseTimes) {
  let keys = Object.keys(value);
  if (ctx.showHidden) keys = Object.getOwnPropertyNames(value);
  const symbols = Object.getOwnPropertySymbols(value).filter(
    (s) => ctx.showHidden || Object.prototype.propertyIsEnumerable.call(value, s),
  );
  const ctor = getCtorName(value);
  let braces;
  let base = "";
  let formatter = null;
  let extrasType = "object";
  let prefix = "";

  if (Array.isArray(value)) {
    prefix = ctor !== "Array" && ctor ? `${ctor}(${value.length}) ` : "";
    keys = keys.filter((k) => !/^(0|[1-9][0-9]*)$/.test(k));
    braces = [`${prefix}[`, "]"];
    extrasType = "array";
    if (value.length === 0 && keys.length === 0 && symbols.length === 0) return `${braces[0]}]`;
    formatter = () => {
      const out = [];
      const len = Math.min(value.length, ctx.maxArrayLength);
      let holes = 0;
      for (let i = 0; i < len; i++) {
        if (!Object.prototype.hasOwnProperty.call(value, i)) {
          holes++;
          continue;
        }
        if (holes) {
          out.push(stylize(ctx, `<${holes} empty item${holes > 1 ? "s" : ""}>`, "undefined"));
          holes = 0;
        }
        out.push(formatValue(ctx, value[i], recurseTimes));
      }
      if (holes) out.push(stylize(ctx, `<${holes} empty item${holes > 1 ? "s" : ""}>`, "undefined"));
      if (value.length > len) out.push(`... ${value.length - len} more item${value.length - len > 1 ? "s" : ""}`);
      return out;
    };
  } else if (typeof value === "function") {
    const isClass = Function.prototype.toString.call(value).startsWith("class");
    const name = value.name ? `: ${value.name}` : isClass ? " (anonymous)" : " (anonymous)";
    let typeName = isClass ? "class" : "Function";
    if (!isClass) {
      const tag = value[Symbol.toStringTag];
      if (tag === "AsyncFunction") typeName = "AsyncFunction";
      else if (tag === "GeneratorFunction") typeName = "GeneratorFunction";
      else if (tag === "AsyncGeneratorFunction") typeName = "AsyncGeneratorFunction";
    }
    base = isClass ? `[class ${value.name || "(anonymous)"}]` : `[${typeName}${name}]`;
    keys = keys.filter((k) => k !== "prototype");
    if (keys.length === 0 && symbols.length === 0) return stylize(ctx, base, "special");
    braces = ["{", "}"];
  } else if (value instanceof RegExp) {
    base = stylize(ctx, RegExp.prototype.toString.call(value), "regexp");
    if (keys.length === 0 && symbols.length === 0) return base;
    braces = ["{", "}"];
  } else if (value instanceof Date) {
    base = stylize(ctx, Number.isNaN(value.getTime()) ? "Invalid Date" : value.toISOString(), "date");
    if (keys.length === 0 && symbols.length === 0) return base;
    braces = ["{", "}"];
  } else if (value instanceof Error) {
    base = formatError(value);
    keys = keys.filter((k) => k !== "stack" && k !== "message");
    if (keys.length === 0 && symbols.length === 0) {
      return ctx.indentationLvl ? base.replaceAll("\n", `\n${" ".repeat(ctx.indentationLvl)}`) : base;
    }
    braces = ["{", "}"];
  } else if (value instanceof Map) {
    braces = [`${ctor && ctor !== "Map" ? ctor : "Map"}(${value.size}) {`, "}"];
    if (value.size === 0 && keys.length === 0) return `${braces[0]}}`;
    formatter = () => {
      const out = [];
      for (const [k, v] of value) out.push(`${formatValue(ctx, k, recurseTimes)} => ${formatValue(ctx, v, recurseTimes)}`);
      return out;
    };
  } else if (value instanceof Set) {
    braces = [`${ctor && ctor !== "Set" ? ctor : "Set"}(${value.size}) {`, "}"];
    if (value.size === 0 && keys.length === 0) return `${braces[0]}}`;
    formatter = () => {
      const out = [];
      for (const v of value) out.push(formatValue(ctx, v, recurseTimes));
      return out;
    };
  } else if (ArrayBuffer.isView(value) && !(value instanceof DataView)) {
    const name = ctor || "TypedArray";
    braces = [`${name}(${value.length}) [`, "]"];
    keys = keys.filter((k) => !/^(0|[1-9][0-9]*)$/.test(k));
    if (value.length === 0 && keys.length === 0) return `${braces[0]}]`;
    formatter = () => {
      const out = [];
      const len = Math.min(value.length, ctx.maxArrayLength);
      for (let i = 0; i < len; i++) out.push(formatPrimitive(ctx, value[i]));
      if (value.length > len) out.push(`... ${value.length - len} more item${value.length - len > 1 ? "s" : ""}`);
      return out;
    };
    extrasType = "array";
  } else if (value instanceof ArrayBuffer) {
    const bytes = new Uint8Array(value);
    const hex = Array.from(bytes.slice(0, 50), (b) => b.toString(16).padStart(2, "0")).join(" ");
    return `ArrayBuffer { [Uint8Contents]: <${hex}${bytes.length > 50 ? ` ... ${bytes.length - 50} more bytes` : ""}>, byteLength: ${bytes.length} }`;
  } else if (value instanceof Promise) {
    braces = ["Promise {", "}"];
    formatter = () => ["<unknown>"];
  } else if (value instanceof WeakMap) {
    return `WeakMap { <items unknown> }`;
  } else if (value instanceof WeakSet) {
    return `WeakSet { <items unknown> }`;
  } else if (typeof value[Symbol.iterator] === "function" && /Iterator$/.test(Object.prototype.toString.call(value).slice(8, -1))) {
    return `Object [${Object.prototype.toString.call(value).slice(8, -1)}] {}`;
  } else if (value instanceof DataView) {
    braces = ["DataView {", "}"];
    formatter = () => [
      `byteLength: ${formatPrimitive(ctx, value.byteLength)}`,
      `byteOffset: ${formatPrimitive(ctx, value.byteOffset)}`,
      `buffer: ${formatValue(ctx, value.buffer, recurseTimes)}`,
    ];
  } else if (value instanceof Number || value instanceof String || value instanceof Boolean || (typeof BigInt === "function" && value instanceof BigInt)) {
    const prim = value.valueOf();
    base = `[${value instanceof Number ? "Number" : value instanceof String ? "String" : value instanceof Boolean ? "Boolean" : "BigInt"}: ${formatPrimitive(ctx, prim)}]`;
    if (value instanceof String) keys = keys.filter((k) => !/^(0|[1-9][0-9]*)$/.test(k));
    if (keys.length === 0 && symbols.length === 0) return base;
    braces = ["{", "}"];
  } else {
    const tag = value[Symbol.toStringTag];
    let name = ctor;
    if (name === null) name = "[Object: null prototype]";
    if (name === "Object") name = "";
    if (typeof tag === "string" && tag && tag !== name) name = name ? `${name} [${tag}]` : `Object [${tag}]`;
    if (ctor === "Module" || Object.prototype.toString.call(value) === "[object Module]") name = "[Module: null prototype]";
    braces = [name ? `${name} {` : "{", "}"];
    if (keys.length === 0 && symbols.length === 0) {
      return `${braces[0]}}`;
    }
  }

  if (recurseTimes > ctx.depth && ctx.depth !== null && ctx.depth !== Infinity) {
    const n = Array.isArray(value) ? "Array" : ctor || "Object";
    return stylize(ctx, `[${n}]`, "special");
  }

  recurseTimes += 1;
  ctx.seen.push(value);
  ctx.indentationLvl += 2;
  let output;
  try {
    output = formatter ? formatter() : [];
    if (extrasType === "array" && output.length) output = groupArray(ctx, output);
    for (const key of keys) output.push(formatProperty(ctx, value, recurseTimes, key));
    for (const sym of symbols) output.push(formatProperty(ctx, value, recurseTimes, sym));
  } finally {
    ctx.indentationLvl -= 2;
    ctx.seen.pop();
  }
  if (ctx.sorted) output.sort();
  return reduceToSingle(ctx, output, base, braces, extrasType, ctx.indentationLvl);
}

function formatProperty(ctx, value, recurseTimes, key) {
  const desc = Object.getOwnPropertyDescriptor(value, key) || { value: value[key], enumerable: true };
  let str;
  if (desc.value !== undefined || !("get" in desc || "set" in desc)) {
    ctx.indentationLvl += 2;
    str = formatValue(ctx, desc.value, recurseTimes);
    ctx.indentationLvl -= 2;
  } else if (desc.get !== undefined) {
    if (ctx.getters) {
      try {
        str = `${stylize(ctx, "[Getter]", "special")} ${formatValue(ctx, desc.get.call(value), recurseTimes)}`;
      } catch (e) {
        str = stylize(ctx, `[Getter: <Inspection threw (${e.message})>]`, "special");
      }
    } else {
      str = stylize(ctx, desc.set !== undefined ? "[Getter/Setter]" : "[Getter]", "special");
    }
  } else {
    str = stylize(ctx, "[Setter]", "special");
  }
  const name = desc.enumerable === false ? `[${typeof key === "symbol" ? key.toString() : key}]` : formatKey(key);
  return `${name}: ${str}`;
}

function inspect(value, opts, depthArg, colorsArg) {
  const ctx = { ...defaultOptions, seen: [], indentationLvl: 0, circular: false };
  if (typeof opts === "boolean") {
    ctx.showHidden = opts;
    if (depthArg !== undefined) ctx.depth = depthArg;
    if (colorsArg !== undefined) ctx.colors = colorsArg;
  } else if (opts && typeof opts === "object") {
    Object.assign(ctx, opts);
  }
  if (ctx.depth === null) ctx.depth = Infinity;
  const out = formatValue(ctx, value, 0);
  if (ctx.circular && typeof value === "object" && value !== null && !out.startsWith("[Circular")) {
    return out.replace(/^([^{[]*)([{[])/, "$1<ref *1> $2");
  }
  return out;
}
inspect.custom = customInspect;
inspect.defaultOptions = defaultOptions;
inspect.colors = colorCodes;
inspect.styles = styles;

function formatWithOptions(inspectOptions, ...args) {
  const first = args[0];
  let a = 0;
  let str = "";
  let join = "";
  if (typeof first === "string") {
    if (args.length === 1) return first;
    let tempStr;
    let lastPos = 0;
    for (let i = 0; i < first.length - 1; i++) {
      if (first.charCodeAt(i) === 37) {
        const nextChar = first.charCodeAt(++i);
        if (a + 1 !== args.length) {
          switch (nextChar) {
            case 115: {
              const tempArg = args[++a];
              if (typeof tempArg === "number" || typeof tempArg === "bigint") tempStr = formatPrimitive({ colors: false }, tempArg);
              else if (typeof tempArg !== "object" || tempArg === null) tempStr = String(tempArg);
              else tempStr = inspect(tempArg, { ...inspectOptions, depth: 0 });
              break;
            }
            case 106:
              try {
                tempStr = JSON.stringify(args[++a]);
              } catch {
                tempStr = "[Circular]";
              }
              break;
            case 100: {
              const n = args[++a];
              tempStr = typeof n === "bigint" ? `${n}n` : typeof n === "symbol" ? "NaN" : String(Number(n));
              break;
            }
            case 79:
              tempStr = inspect(args[++a], inspectOptions);
              break;
            case 111:
              tempStr = inspect(args[++a], { ...inspectOptions, showHidden: true, depth: 4 });
              break;
            case 105: {
              const n = args[++a];
              tempStr = typeof n === "bigint" ? `${n}n` : typeof n === "symbol" ? "NaN" : String(parseInt(n));
              break;
            }
            case 102: {
              const n = args[++a];
              tempStr = typeof n === "symbol" ? "NaN" : String(parseFloat(n));
              break;
            }
            case 99:
              a += 1;
              tempStr = "";
              break;
            case 37:
              str += first.slice(lastPos, i);
              lastPos = i + 1;
              continue;
            default:
              continue;
          }
          if (lastPos !== i - 1) str += first.slice(lastPos, i - 1);
          str += tempStr;
          lastPos = i + 1;
        } else if (nextChar === 37) {
          str += first.slice(lastPos, i);
          lastPos = i + 1;
        }
      }
    }
    if (lastPos !== 0) {
      a++;
      join = " ";
      if (lastPos < first.length) str += first.slice(lastPos);
    }
  }
  while (a < args.length) {
    const value = args[a];
    str += join;
    str += typeof value !== "string" ? inspect(value, inspectOptions) : value;
    join = " ";
    a++;
  }
  return str;
}

function format(...args) {
  return formatWithOptions(undefined, ...args);
}

internal.inspect = inspect;
internal.format = format;
internal.formatWithOptions = formatWithOptions;
