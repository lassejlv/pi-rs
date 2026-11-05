//! Extensions import `@earendil-works/pi-durable` and `@earendil-works/chord` like the other
//! packages pi ships to extensions: the embedded modules of `pi-durable-jsrt` over native
//! pi-durable and pi-chord, installed in the extension runtime.

mod extensions_support;
mod support;

use extensions_support::*;
use serde_json::{Value, json};

/// Load `source` as the only extension and return its single tool's description as JSON.
async fn run_extension(source: &str) -> Value {
    let dir = ExtDir::new();
    dir.write("durable.ts", source);
    let result = dir.discover().await;
    assert!(result.errors.is_empty(), "{:#?}", result.errors);
    let extension = result.extensions[0].read();
    serde_json::from_str(&extension.tools["out"].definition.description).unwrap()
}

/// A Session over MemoryStorage commits and reads a document; a Harness over another
/// MemoryStorage runs one turn with the faux provider.
#[tokio::test(flavor = "multi_thread")]
async fn extension_runs_a_session_and_a_harness_turn() {
    let out = run_extension(
        r#"
        import { createModels, fauxAssistantMessage, fauxProvider, fauxText } from "@earendil-works/pi-ai";
        import { createRegistry, createSession, defineDoc, Harness, MemoryStorage, UserEntry, AssistantEntry } from "@earendil-works/pi-durable";
        import { MemoryStorage as SubpathMemoryStorage } from "@earendil-works/pi-durable/storage/memory";
        import { BACKGROUND_CONTEXT as context } from "@earendil-works/chord/context";

        const Counter = defineDoc({ kind: "ext.counter", version: 1, scope: "session", initial: () => ({ count: 0 }), checkpointWhen: () => false });

        async function sessionRoundTrip() {
            const session = createSession(new MemoryStorage());
            await session.commit(async (tx) => { (await tx.doc(Counter)).count += 2; }, context);
            await session.commit(async (tx) => { (await tx.doc(Counter)).count += 3; }, context);
            const snapshot = await session.snapshot(Counter, context);
            await session.close?.(context);
            return snapshot;
        }

        async function harnessTurn() {
            const faux = fauxProvider();
            faux.setResponses([fauxAssistantMessage([fauxText("hello from faux")])]);
            const models = createModels();
            models.setProvider(faux.provider);
            const harness = await Harness.open(new SubpathMemoryStorage(), { models, registry: createRegistry() }, context);
            const root = await harness.root(context, { agent: { model: { provider: "faux", modelId: "faux-1" } } });
            const submission = await root.submit({ type: "input", content: "hi" }, context);
            const settled = await submission.wait(context);
            const page = await root.entries({}, 100, undefined, context);
            const kinds = [...page.items].reverse().map((entry) => entry.kind);
            const assistant = [...page.items].find((entry) => entry.kind === AssistantEntry.kind);
            const text = assistant?.model?.[0]?.content?.find?.((c) => c.type === "text")?.text;
            await harness.close(context);
            return { status: settled.status, kinds, text, user: kinds.includes(UserEntry.kind) };
        }

        export default async function (pi) {
            let out;
            try {
                out = { session: await sessionRoundTrip(), harness: await harnessTurn() };
            } catch (error) {
                out = { error: String(error?.stack ?? error) };
            }
            pi.registerTool({ name: "out", label: "out", description: JSON.stringify(out), parameters: { type: "object", properties: {} }, execute: async () => ({ content: [] }) });
        }
        "#,
    )
    .await;
    assert!(out.get("error").is_none(), "{out}");
    assert_eq!(out["session"], json!({ "count": 5 }));
    assert_eq!(out["harness"]["status"], "done", "{out}");
    assert_eq!(out["harness"]["user"], true, "{out}");
    assert_eq!(out["harness"]["text"], "hello from faux", "{out}");
}

/// Every public subpath of both packages loads in the extension runtime.
#[tokio::test(flavor = "multi_thread")]
async fn extension_imports_every_embedded_subpath() {
    let out = run_extension(
        r#"
        const specifiers = [
            "@earendil-works/pi-durable", "@earendil-works/pi-durable/env", "@earendil-works/pi-durable/env/node",
            "@earendil-works/pi-durable/codemode", "@earendil-works/pi-durable/tool-search", "@earendil-works/pi-durable/tools",
            "@earendil-works/pi-durable/images", "@earendil-works/pi-durable/images/node", "@earendil-works/pi-durable/images/cloudflare",
            "@earendil-works/pi-durable/storage/memory", "@earendil-works/pi-durable/storage/jsonl", "@earendil-works/pi-durable/storage/jsonl/node",
            "@earendil-works/pi-durable/storage/sqlite", "@earendil-works/pi-durable/storage/sqlite/node",
            "@earendil-works/pi-durable/testing", "@earendil-works/chord", "@earendil-works/chord/context", "@earendil-works/chord/output",
            "@earendil-works/chord/delta", "@earendil-works/chord/sqlite", "@earendil-works/chord/sqlite/node",
            "@earendil-works/pi-telemetry", "@earendil-works/pi-telemetry/render",
        ];
        export default async function (pi) {
            const out = {};
            for (const specifier of specifiers) {
                try { out[specifier] = Object.keys(await import(specifier)).sort(); } catch (error) { out[specifier] = String(error); }
            }
            pi.registerTool({ name: "out", label: "out", description: JSON.stringify(out), parameters: { type: "object", properties: {} }, execute: async () => ({ content: [] }) });
        }
        "#,
    )
    .await;
    for (specifier, value) in out.as_object().unwrap() {
        assert!(
            value.as_array().is_some_and(|keys| !keys.is_empty()),
            "{specifier}: {value}"
        );
    }
    if let Ok(path) = std::env::var("PI_DURABLE_EXPORTS_DUMP") {
        std::fs::write(path, out.to_string()).unwrap();
    }
}
