// pi-durable:internal/env/port — a JS `ExecutionEnv` handed to native code (src/dispatch/env/host.rs
// `JsEnv`). `describeEnv(env)` is what glue passes to Rust; Rust calls the env's own methods
// through `port`, which normalizes `Result` errors to plain data and links native cancellation
// to a JS `AbortSignal`. No Node built-ins: portable like env/index.

import { call, ref } from "pi-durable:bridge";
import { BACKGROUND_CONTEXT, withAbortSignal } from "@earendil-works/chord/context";
import { FileError } from "@earendil-works/pi-durable/env";

function contextFor(link) {
  if (link === undefined) return BACKGROUND_CONTEXT;
  const controller = new AbortController();
  if (call("env.linkSignal", link, ref(controller))) controller.abort();
  return withAbortSignal(controller.signal, BACKGROUND_CONTEXT);
}

function errorData(error) {
  if (error === null || typeof error !== "object") return { code: "unknown", message: String(error) };
  return {
    name: error.name,
    code: error.code,
    message: String(error.message),
    path: error.path,
    spillPath: error.spillPath,
  };
}

/** A `Result` as plain data; `byRef`: its value is an object Rust calls methods on (a reader, a watcher). */
function normalize(result, byRef = false) {
  if (result !== null && typeof result === "object" && result.ok === false) {
    return { ok: false, error: errorData(result.error) };
  }
  if (byRef && result?.ok === true) return { ok: true, value: ref(result.value) };
  return result;
}

const unsupported = { ok: false, error: { code: "not_supported", message: "Filesystem capability is not supported" } };

const port = {
  /** `target[method](...args, context)`; `raw`: the method returns a value, not a `Result`. */
  async invoke(target, method, args, raw, link) {
    const fn = target[method];
    if (typeof fn !== "function") return unsupported;
    const result = await Reflect.apply(fn, target, [...args, contextFor(link)]);
    return raw ? { ok: true, value: result } : normalize(result, method.startsWith("open"));
  },
  async exec(env, command, options, output, link) {
    const forwarded =
      output === undefined
        ? options
        : { ...options, onOutput: (text, _context, info) => call("env.output", output, text, info) };
    return normalize(await env.exec(command, forwarded, contextFor(link)));
  },
  async watch(env, targets, deliver, link) {
    const onChange = (change) =>
      call("env.watchDeliver", deliver, change.error instanceof FileError || change.error !== undefined ? { error: errorData(change.error) } : change);
    return normalize(await env.watch(targets, onChange, contextFor(link)), true);
  },
  get(target, key) {
    return target[key];
  },
};

/** What native code needs to use `env` (`undefined` stays `undefined`). */
export function describeEnv(env) {
  if (env === undefined || env === null) return undefined;
  return { port: ref(port), env: ref(env), id: String(env.id), cwd: String(env.cwd) };
}
