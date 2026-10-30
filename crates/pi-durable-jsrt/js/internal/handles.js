// pi-durable:internal/handles — JS values the native service refers to by id (port of
// scripts/stock/durable-native-handles.mjs). Objects and functions keep one id for life;
// `temporaryHandle` ids are released by the caller.
import { nativeTransport } from "pi-durable:internal/transport";

let next = 1;
const objects = new Map();
const ids = new WeakMap();
const nativeTools = new WeakSet();
const nativeGenerations = new WeakSet();
const nativeCompactions = new WeakSet();

/** The built-in compaction task runs natively (its JS phases are never called). */
export function markNativeCompaction(task) {
  nativeCompactions.add(task);
}

/** The built-in generation task runs natively. */
export function markNativeGeneration(task) {
  nativeGenerations.add(task);
}

/** The built-in tool task runs natively. */
export function markNativeTool(task) {
  nativeTools.add(task);
}

/** A stable id for an object or function. */
export function handle(value) {
  let id = ids.get(value);
  if (id === undefined) {
    id = next++;
    ids.set(value, id);
    objects.set(id, value);
  }
  return id;
}

/** An id for any value (objects keep their stable id; primitives get a fresh one). */
export function valueHandle(value) {
  if ((typeof value === "object" && value !== null) || typeof value === "function") return handle(value);
  const id = next++;
  objects.set(id, value);
  return id;
}

/** The value behind an id. */
export function objectFor(id) {
  return objects.get(id);
}

/** A fresh id for `value`, released with `release()`. */
export function temporaryHandle(value) {
  const id = next++;
  objects.set(id, value);
  return { id, release: () => objects.delete(id) };
}

/** The wire description of a task definition. */
export function describeTask(task) {
  const { name, version, phases, migrate } = task.definition;
  return {
    id: handle(task),
    name,
    version,
    phases: Object.keys(phases),
    migrate: migrate !== undefined,
    ...(nativeTools.has(task) ? { nativeTool: true } : {}),
    ...(nativeGenerations.has(task) ? { nativeGeneration: true } : {}),
    ...(nativeCompactions.has(task) ? { nativeCompaction: true } : {}),
  };
}

/** The wire description of a document token. */
export function describeDoc(token) {
  const { kind, version, scope, history, fork, family, migrate } = token.definition;
  return { id: handle(token), kind, version, scope, history, fork, family, migrate: migrate !== undefined };
}

nativeTransport.register("initial", ([id, input]) => objectFor(id).definition.initial(input));
nativeTransport.register("migrate", ([id, input, checkpoint, version]) => objectFor(id).definition.migrate(input, checkpoint, version));
