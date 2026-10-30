// packages/durable/src/harness/define.ts: identity helpers that type registrations.

/** Identity function that types an extension. */
export function defineExtension(extension) {
  return extension;
}

/** Identity function that types a tool. */
export function defineTool(tool) {
  return tool;
}

/** A prompt section; tagged unless `tag` is false. */
export function section(key, render, options) {
  return options?.tag === undefined ? { key, render } : { key, render, tag: options.tag };
}

/** Hook handlers for tasks with `task`'s name. */
export function hook(task, handlers) {
  return { task: task.definition.name, handlers };
}

/** Wrap the tool named like `tool` wherever the wrapping extension is selected. */
export function wrapTool(tool, wrapper) {
  return { tool: tool.name, wrap: wrapper };
}

/** Wrap the section `key` wherever the wrapping extension is selected. */
export function wrapSection(key, wrapper) {
  return { section: key, wrap: wrapper };
}
