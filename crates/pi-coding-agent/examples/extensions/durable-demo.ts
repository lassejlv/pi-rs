/**
 * Durable demo: native Rust pi-durable from a pi extension.
 *
 * /durable        bump a counter stored in .pi/durable-demo.sqlite (survives restarting pi)
 * /durable chat   run one Harness turn against the faux provider and show the reply
 *
 * Usage: copy to ~/.pi/agent/extensions/ or .pi/extensions/
 */

import { createModels, fauxAssistantMessage, fauxProvider, fauxText } from "@earendil-works/pi-ai"
import { AssistantEntry, createRegistry, createSession, defineDoc, Harness } from "@earendil-works/pi-durable"
import { MemoryStorage } from "@earendil-works/pi-durable/storage/memory"
import { openNodeSqliteStorage } from "@earendil-works/pi-durable/storage/sqlite/node"
import { BACKGROUND_CONTEXT as context } from "@earendil-works/chord/context"

const Counter = defineDoc({
  kind: "demo.counter",
  version: 1,
  scope: "session",
  initial: () => ({ count: 0 }),
  checkpointWhen: () => false,
})

async function bumpCounter(path: string) {
  const session = createSession(await openNodeSqliteStorage(path))
  await session.commit(async (tx) => {
    ;(await tx.doc(Counter)).count += 1
  }, context)
  const snapshot = await session.snapshot(Counter, context)
  await session.close?.(context)
  return snapshot.count
}

async function harnessTurn() {
  const faux = fauxProvider()
  faux.setResponses([fauxAssistantMessage([fauxText("hello from a durable Harness turn")])])
  const models = createModels()
  models.setProvider(faux.provider)
  const harness = await Harness.open(new MemoryStorage(), { models, registry: createRegistry() }, context)
  const root = await harness.root(context, { agent: { model: { provider: "faux", modelId: "faux-1" } } })
  const settled = await (await root.submit({ type: "input", content: "hi" }, context)).wait(context)
  const page = await root.entries({}, 100, undefined, context)
  const assistant = [...page.items].find((entry) => entry.kind === AssistantEntry.kind)
  const text = assistant?.model?.[0]?.content?.find?.((c) => c.type === "text")?.text
  await harness.close(context)
  return `${settled.status}: ${text}`
}

export default function durableDemo(pi) {
  pi.registerCommand("durable", {
    description: "pi-durable demo: counter in SQLite, or `chat` for a Harness turn",
    handler: async (args, ctx) => {
      try {
        if (args.trim() === "chat") {
          ctx.ui.notify(`Harness ${await harnessTurn()}`, "info")
        } else {
          const count = await bumpCounter(`${ctx.cwd}/.pi/durable-demo.sqlite`)
          ctx.ui.notify(`Durable counter: ${count} (stored in .pi/durable-demo.sqlite)`, "info")
        }
      } catch (error) {
        ctx.ui.notify(`durable demo failed: ${error?.stack ?? error}`, "error")
      }
    },
  })
}
