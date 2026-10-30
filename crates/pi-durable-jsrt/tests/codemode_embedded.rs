//! Embedded `@earendil-works/pi-codemode`: declarations and the caller-answered `start` run over
//! the native pi-codemode VM.

use pi_durable_jsrt::{HostOptions, install};
use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::json;

async fn runtime() -> (JsRuntime, pi_durable_jsrt::DurableHost) {
    let rt = JsRuntime::new(RuntimeOptions {
        timer_control: true,
        ..Default::default()
    })
    .unwrap();
    let host = install(&rt, HostOptions::default()).await.unwrap();
    (rt, host)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_runs_answers_calls_in_order_and_settles_after_them() {
    let (rt, host) = runtime().await;
    let ns = rt
        .eval_module(
            r#"
            import { CodemodeSandbox } from "@earendil-works/pi-codemode";
            export async function run() {
              const sandbox = new CodemodeSandbox();
              const events = [];
              let handle;
              handle = sandbox.start(
                'const a = await tools.echo({ text: "x\uD800" }); text(a); store("k", 1); return 7;',
                {
                  startedAt: 1000,
                  seed: 1,
                  tools: [{ name: "echo", description: "Echo" }],
                  onStarted: (fingerprint) => events.push(["started", typeof fingerprint]),
                  onCall: (call) => {
                    events.push(["call", call.target, call.name, call.args.text === "x\uD800"]);
                    queueMicrotask(() => handle.deliver(1, 1001, call.id, { ok: true, value: "echo " + call.args.text }));
                  },
                  onIdle: (seq, issued) => events.push(["idle", seq, issued]),
                },
              );
              const result = await handle.result;
              events.push(["done", handle.finished]);
              await sandbox.close();
              // Lone surrogates survive the VM round trip (checked here: the test reads JSON).
              const lossless = result.output[0].text === "echo x\uD800";
              result.output = [];
              return { result, events, lossless };
            }
            export async function aborted() {
              const sandbox = new CodemodeSandbox();
              const controller = new AbortController();
              const handle = sandbox.start("await tools.wait({}); return 1;", {
                startedAt: 0,
                seed: 0,
                tools: [{ name: "wait" }],
                signal: controller.signal,
                onCall: () => controller.abort(new Error("stop now")),
              });
              return await handle.result;
            }
            "#,
            "/codemode-embedded-test.mjs",
        )
        .await
        .unwrap();
    let out = ns.call_method("run", vec![]).await.unwrap();
    let result = &out["result"];
    assert_eq!(result["ok"], json!(true), "{out}");
    assert_eq!(result["value"], json!(7));
    assert_eq!(out["lossless"], json!(true), "{out}");
    let events = out["events"].as_array().unwrap();
    assert_eq!(events[0], json!(["started", "string"]));
    assert_eq!(events[1], json!(["call", "tool", "echo", true]));
    assert_eq!(events.last().unwrap(), &json!(["done", true]));
    let aborted = ns.call_method("aborted", vec![]).await.unwrap();
    assert_eq!(aborted["ok"], json!(false), "{aborted}");
    assert_eq!(aborted["error"]["kind"], json!("aborted"));
    assert_eq!(aborted["error"]["message"], json!("stop now"));
    assert!(host.counters()["codemode.sdk.start"] >= 2);
    rt.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_and_declarations_are_native() {
    let (rt, _host) = runtime().await;
    let ns = rt
        .eval_module(
            r#"
            import { parseCodemodeSource, CodemodeSourceError, renderToolSignature, toCodemodeIdentifier } from "@earendil-works/pi-codemode";
            export function run() {
              let error;
              try { parseCodemodeSource("  "); } catch (e) { error = [e instanceof CodemodeSourceError, e.name]; }
              return {
                parsed: parseCodemodeSource('// @options: {"timeout_ms": 5}\nreturn 1;'),
                error,
                signature: renderToolSignature({ name: "my-tool", inputSchema: { type: "object" } }),
                identifier: toCodemodeIdentifier("a-b"),
              };
            }
            "#,
            "/codemode-embedded-source.mjs",
        )
        .await
        .unwrap();
    let out = ns.call_method("run", vec![]).await.unwrap();
    assert_eq!(
        out["parsed"],
        json!({ "code": "\nreturn 1;", "options": { "timeoutMs": 5 } })
    );
    assert_eq!(out["error"], json!([true, "CodemodeSourceError"]));
    assert_eq!(
        out["signature"],
        json!("my_tool(args: { [key: string]: unknown; }): Promise<unknown>;")
    );
    assert_eq!(out["identifier"], json!("a_b"));
    rt.shutdown();
}

/// `tool_search` and the code mode description over the in-process stock service (the Harness
/// itself, and so the complete extensions, need the upstream pi-ai sources: see the runner).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_builtin_registrations_describe_the_native_tools() {
    let (rt, _host) = runtime().await;
    let ns = rt
        .eval_module(
            r####"
            import { createToolSearchExtension, TOOL_SEARCH_TOOL_NAME } from "@earendil-works/pi-durable/tool-search";
            import { codemodeDescription, codemodeSection } from "pi-durable:internal/src/durable/codemode/describe";
            export function run() {
              const search = createToolSearchExtension().tools[0];
              const read = { name: "read", description: "Read a file.", parameters: { type: "object", properties: { path: { type: "string" } } } };
              return {
                search: [search.name === TOOL_SEARCH_TOOL_NAME, search.__piNativeBuiltin, search.callers, search.parameters.required],
                models: [codemodeDescription(true).includes("`models`"), codemodeDescription(false).includes("`models`")],
                section: codemodeSection([read], [], 3000).includes("### `read`\nRead a file."),
                empty: codemodeSection([], [], 3000) === undefined,
              };
            }
            "####,
            "/codemode-embedded-builtins.mjs",
        )
        .await
        .unwrap();
    let out = ns.call_method("run", vec![]).await.unwrap();
    assert_eq!(
        out["search"],
        json!([true, "tool_search", ["model"], ["query"]])
    );
    assert_eq!(out["models"], json!([true, false]));
    assert_eq!(out["section"], json!(true), "{out}");
    assert_eq!(out["empty"], json!(true));
    rt.shutdown();
}

/// `execute` with live JS tools, and the remote sandbox served in process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_runs_live_tools_locally_and_through_a_remote() {
    let (rt, _host) = runtime().await;
    let ns = rt
        .eval_module(
            r#"
            import { CodemodeSandbox, RemoteCodemodeSandbox, serveCodemodeRemote } from "@earendil-works/pi-codemode";
            const tools = [{ name: "add", execute: (args) => args.a + 1 }, { name: "fail", execute: () => { throw new Error("boom"); } }];
            const code = "let caught; try { await tools.fail(); } catch (e) { caught = e.message; } return [await tools.add({ a: 1 }), caught];";
            export async function run() {
              const local = new CodemodeSandbox({ tools });
              const exchanges = [];
              const remote = new RemoteCodemodeSandbox({
                tools,
                remote: () => ({
                  execute: (request, exchange) =>
                    serveCodemodeRemote(structuredClone(request), async (messages) => {
                      exchanges.push(messages.length);
                      return structuredClone(await exchange(structuredClone(messages)));
                    }),
                }),
              });
              const results = [await local.execute(code), await remote.execute(code)];
              await local.close();
              await remote.close();
              return { results: results.map((r) => [r.ok, r.value, r.calls.map((c) => c.status)]), exchanged: exchanges.length > 0 };
            }
            "#,
            "/codemode-embedded-execute.mjs",
        )
        .await
        .unwrap();
    let out = ns.call_method("run", vec![]).await.unwrap();
    let expected = json!([true, [2, "boom"], ["error", "ok"]]);
    assert_eq!(out["results"], json!([expected.clone(), expected]), "{out}");
    assert_eq!(out["exchanged"], json!(true));
    rt.shutdown();
}
