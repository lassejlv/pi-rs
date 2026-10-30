// packages/durable/src/codemode/models.ts: port of scripts/stock/durable-codemode-models-adapter.mjs.
// The catalog, credential stripping, validation and internal model tools are native (the hosted
// stock service, builtin_bridge.rs); the caller's model collections and provider request methods
// stay JS and are called back through the transport.
import { nativeTransport } from "pi-durable:internal/transport";
import { handle, objectFor } from "pi-durable:internal/handles";

const values = nativeTransport.sync("builtin.modelExports");
const controllers = new Map();

export const MODELS_DESCRIPTION = values.description;
export const CATALOG_GLOBALS = values.catalog;
export const CODEMODE_INTERNAL_PREFIX = values.prefix;
export const CLASSIFY_TOOL = values.classify;
export const GENERATE_IMAGES_TOOL = values.images;
export const MAX_CONCURRENT_MODEL_CALLS = values.concurrency;
export const MODEL_CALLS = { "models.classify": CLASSIFY_TOOL, "models.generateImages": GENERATE_IMAGES_TOOL };

const describe = (model) => ({ handle: handle(model), reference: { provider: model.provider, modelId: model.id } });

nativeTransport.register("codemode.models.info", (id) => {
  const info = { ...objectFor(id) };
  delete info.headers;
  return JSON.parse(JSON.stringify(info));
});
nativeTransport.register("codemode.models.list", (args) =>
  objectFor(args.collection).getModelsOfType(args.kind, args.provider ?? undefined).map(describe),
);
nativeTransport.register("codemode.models.get", (args) => {
  const model = objectFor(args.collection).getModelOfType(args.kind, args.reference.provider, args.reference.modelId);
  return model === undefined ? null : describe(model);
});
nativeTransport.event("codemode.models.signal", (args) => controllers.get(args.stream)?.abort(new Error("Aborted")));
nativeTransport.register("codemode.models.available", async (args) => {
  const controller = new AbortController();
  controllers.set(args.stream, controller);
  if (args.aborted) controller.abort(new Error("Aborted"));
  try {
    return (
      await objectFor(args.collection).getAvailableOfType(args.kind, args.provider ?? undefined, {
        signal: controller.signal,
      })
    ).map(describe);
  } finally {
    controllers.delete(args.stream);
  }
});
nativeTransport.register("codemode.models.request", async (args, context) => {
  const controller = new AbortController();
  controllers.set(args.stream, controller);
  if (args.aborted) controller.abort(new Error("Aborted"));
  try {
    const models = objectFor(args.collection);
    const method = args.effect === "classifier" ? "classify" : "generateImages";
    return await models[method](objectFor(args.model), args.input, {
      signal: controller.signal,
      ...(context?.telemetry === undefined ? {} : { telemetry: context.telemetry }),
    });
  } finally {
    controllers.delete(args.stream);
  }
});

/** A catalog global (`models.list()`, ...) answered natively from `models`. */
export function catalogCall(name, args, models, context) {
  return nativeTransport.request("codemode.catalog", { name, args, collection: handle(models) }, context);
}

/** The internal `pi.codemode.models.*` tools; the native tool task executes them. */
export function createModelTools() {
  return [CLASSIFY_TOOL, GENERATE_IMAGES_TOOL].map((name) => ({
    ...nativeTransport.sync("builtin.describe", { tool: name }),
    callers: ["tools"],
    __piNativeBuiltin: name,
    execute() {
      throw new Error("Native model-tool execution was bypassed");
    },
  }));
}
