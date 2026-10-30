//! The in-process durable service (`pi-durable:internal/transport`) and the embedded Harness.

use pi_durable_jsrt::{HostOptions, install};
use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::json;

/// The pi-ai functions the Harness glue uses, for a runtime without pi-ai.
const PI_AI_STUB: &str = r#"
export const mergeClientIdentity = (base, override) => ({ ...base, ...override });
export const withClientIdentity = (models) => models;
export const validateToolArguments = (_tool, call) => call.arguments;
let next = 0;
export const uuidv7 = () => `00000000-0000-7000-8000-${String(++next).padStart(12, "0")}`;
"#;

async fn runtime() -> JsRuntime {
    let rt = JsRuntime::new(RuntimeOptions {
        timer_control: true,
        ..Default::default()
    })
    .unwrap();
    rt.set_virtual_module(
        "@earendil-works/pi-ai",
        pi_jsrt::VirtualModule::EsmJs(PI_AI_STUB.to_string()),
    );
    install(&rt, HostOptions::default()).await.unwrap();
    rt
}

async fn run(rt: &JsRuntime, src: &str) -> serde_json::Value {
    let ns = rt
        .eval_module(src, "/harness-embedded-test.mjs")
        .await
        .unwrap();
    ns.call_method("run", vec![]).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn transport_round_trips_requests_and_closes() {
    let rt = runtime().await;
    let out = run(
        &rt,
        r#"
        import { nativeTransport } from "pi-durable:internal/transport";
        export async function run() {
            const ready = await nativeTransport.request("transport.ready");
            const registry = nativeTransport.sync("registry.create", { builtins: [] });
            const stats = nativeTransport.sync("registry.stats");
            let unknown;
            try { await nativeTransport.request("no.such.method"); } catch (e) { unknown = e.message; }
            await nativeTransport.close();
            return { ready, registry: typeof registry, operations: typeof stats.operations, unknown: typeof unknown };
        }
        "#,
    )
    .await;
    assert_eq!(
        out,
        json!({ "ready": true, "registry": "number", "operations": "number", "unknown": "string" })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn harness_runs_a_js_task_on_memory_storage() {
    let rt = runtime().await;
    let out = run(
        &rt,
        r#"
        import { createRegistry, defineExtension, defineTask, Harness, MemoryStorage, AgentDoc, DURABLE_TELEMETRY_SCHEMA, DEFAULT_HARNESS_IDENTITY, GenerationTask } from "@earendil-works/pi-durable";
        import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";
        const Echo = defineTask({
            name: "test.echo",
            version: 1,
            initial: () => ({ phase: "run" }),
            phases: {
                run: async (task, runtime, context) => {
                    await runtime.commit(() => ({ status: "terminal", outcome: { status: "completed", result: { echoed: task.input.text } } }), context);
                },
            },
            abort: async () => {},
        });
        export async function run() {
            const registry = createRegistry();
            registry.install(defineExtension({ name: "test", tasks: [Echo] }));
            const harness = await Harness.open(new MemoryStorage(), { registry }, BACKGROUND_CONTEXT);
            const root = await harness.root(BACKGROUND_CONTEXT);
            harness.resume();
            const id = await root.commit((tx) => tx.createTask(Echo, { text: "hi \ud800" }, { ownership: { kind: "conversation" } }), BACKGROUND_CONTEXT);
            const settled = await harness.waitForTask(id, BACKGROUND_CONTEXT);
            const agent = await harness.snapshot(AgentDoc, root.id, BACKGROUND_CONTEXT);
            await harness.close(BACKGROUND_CONTEXT);
            return {
                outcome: { status: settled.state.outcome.status },
                lone: settled.state.outcome.result.echoed === "hi \ud800",
                agent,
                schema: DURABLE_TELEMETRY_SCHEMA.version,
                identity: DEFAULT_HARNESS_IDENTITY.attribution.name,
                generation: GenerationTask.definition.name,
            };
        }
        "#,
    )
    .await;
    assert_eq!(out["lone"], json!(true), "{out:#}");
    assert_eq!(out["outcome"]["status"], json!("completed"));
    assert_eq!(out["agent"], json!({}));
    assert_eq!(out["schema"], json!(1));
    assert_eq!(out["identity"], json!("pi"));
    assert_eq!(out["generation"], json!("pi.generation"));
}
