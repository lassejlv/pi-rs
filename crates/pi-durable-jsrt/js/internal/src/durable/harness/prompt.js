// packages/durable/src/harness/prompt.ts: prompt planning, rendering and replay
// (examples/support/prompt_bridge.rs; port of scripts/stock/durable-prompt-adapter.mjs).
import { nativeTransport, failure } from "pi-durable:internal/transport";
import { handle, objectFor } from "pi-durable:internal/handles";

nativeTransport.register("promptRender", ({ section, input }, context) => objectFor(section).render(objectFor(input), context));
nativeTransport.register("promptReport", ({ report, section, error }) => {
  objectFor(report)(failure(error), objectFor(section));
  return null;
});

/** The sections of a transcript's last system entries, by key. */
export function replaySections(messages) {
  return new Map(nativeTransport.sync("prompt.replay", { messages }));
}

/** The system entries to append so the transcript shows `desired` and `tools`. */
export function planSystemEntries(view, desired, tools, timestamp) {
  const declarations = tools.map(({ name, description, parameters, constrainedSampling }) => ({
    name,
    description,
    parameters: JSON.parse(JSON.stringify(parameters)),
    ...(constrainedSampling === undefined ? {} : { constrainedSampling }),
  }));
  return nativeTransport.sync("prompt.plan", {
    view: { head: view.head ?? null, entries: view.entries, messages: view.messages },
    desired: [...desired],
    tools: declarations,
    timestamp,
  });
}

/** Render `sections` (tagged unless `tag: false`); a throwing section is reported and skipped. */
export async function renderSections(sections, input, shown, report, context) {
  return new Map(
    await nativeTransport.request(
      "prompt.render",
      {
        sections: sections.map((section) => ({ id: handle(section), key: section.key, tagged: section.tag !== false })),
        input: handle(input),
        shown: [...shown],
        report: handle(report),
      },
      context,
    ),
  );
}
