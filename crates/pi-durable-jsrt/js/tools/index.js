// @earendil-works/pi-durable/tools (packages/durable/src/tools/index.ts): the built-in coding tools
// are pi_durable::tools's (src/dispatch/tools.rs `builtin.*`). Each `execute` runs natively against
// the call's own `api.env` and `api` (subclassed environments, test doubles and model collections
// included) and its context's cancellation.

import { call, callAsync, ref } from "pi-durable:bridge";
import { nativeContext } from "pi-durable:internal/chord/context";
import { describeEnv } from "pi-durable:internal/env/port";
import { imagesOption } from "pi-durable:internal/images/native";
import { DEFAULT_IMAGE_LIMITS } from "pi-durable:internal/src/durable/tools/image-processor";

export { DEFAULT_IMAGE_LIMITS };

/** The conversation model's image input, as upstream's `read` derives it. */
async function imageModel(api, context) {
  const reference = (await api.agent(context)).model;
  const model = reference === undefined ? undefined : api.models.getModel(reference.provider, reference.modelId);
  const resize = model?.inputLimits?.images?.resize;
  return {
    vision: model?.input.includes("image") ?? true,
    maxWidth: resize?.maxWidth,
    maxHeight: resize?.maxHeight,
    maxBytes: resize?.maxBytes,
  };
}

/** The call's API for native code: its callbacks keep the caller's `api` and `context`. */
function apiPort(api, context, options) {
  return ref({
    output: (text, skipped) => api.output(text, skipped),
    retainedOutput: () => api.retainedOutput(),
    diagnostic: (diagnostic) => api.diagnostic(diagnostic),
    imageModel: () => imageModel(api, context),
    prepare: async (execution) => {
      await options.prepare(execution, api, context);
      return execution;
    },
  });
}

function apiDescription(api) {
  return { env: describeEnv(api.env), window: api.outputWindow };
}

function shellOptions(options) {
  return {
    ...(options?.commandPrefix === undefined ? {} : { commandPrefix: options.commandPrefix }),
    ...(options?.prepare === undefined ? {} : { prepare: ref(options.prepare) }),
    ...(options?.programs === undefined ? {} : { programs: [...options.programs] }),
  };
}

export function createReadTool(options = {}) {
  const images = imagesOption(options.images);
  return {
    ...call("builtin.describe", "read", options.images !== undefined),
    execute(args, api, context) {
      return callAsync("builtin.read", args, apiPort(api, context, options), apiDescription(api), images, nativeContext(context));
    },
  };
}

export function createWriteTool() {
  return {
    ...call("builtin.describe", "write"),
    execute(args, api, context) {
      return callAsync("builtin.write", args, describeEnv(api.env), nativeContext(context));
    },
  };
}

export function createEditTool() {
  return {
    ...call("builtin.describe", "edit"),
    prepareArguments: (args) => call("builtin.prepareEdit", args),
    execute(args, api, context) {
      return callAsync("builtin.edit", args, describeEnv(api.env), nativeContext(context));
    },
  };
}

function shellTool(name, options) {
  const wire = shellOptions(options);
  return {
    ...call("builtin.describe", name),
    execute(args, api, context) {
      return callAsync(`builtin.${name}`, args, apiPort(api, context, options), apiDescription(api), wire, nativeContext(context));
    },
  };
}

export function createBashTool(options) {
  return shellTool("bash", options);
}

export function createPowerShellTool(options) {
  return shellTool("powershell", options);
}

/**
 * The `coding-tools` extension: `read`, `write`, `edit`, and `bash`; nothing installs it automatically.
 * `createPowerShellTool()` adds `powershell`. `images` prepares the images `read` returns (see `ReadToolOptions`).
 */
export function createCodingTools(options = {}) {
  return {
    name: "coding-tools",
    tools: [createReadTool(options), createWriteTool(), createEditTool(), createBashTool()],
  };
}

/** `createCodingTools()` without an image processor. */
export const CodingTools = createCodingTools();
