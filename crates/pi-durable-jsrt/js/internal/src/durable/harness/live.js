// packages/durable/src/harness/live.ts: `LiveDoc`.
import { defineDoc } from "pi-durable:internal/src/durable/documents";

export const LiveDoc = defineDoc({
  kind: "pi.live",
  version: 1,
  scope: "conversation",
  history: "latest",
  fork: "initial",
  initial: () => ({}),
  // A complete base whenever nothing runs: no generation and no running tool slot.
  checkpointWhen: (value) =>
    value.generation === undefined &&
    !(value.tools ?? []).some((slot) => slot.status === "running") &&
    !(value.nestedTools ?? []).some((slot) => slot.status === "running"),
});
