//! The embedded `@earendil-works/chord` modules outside the upstream test files: they load in a
//! bare runtime (no pi-mono `node_modules`), and their native paths behave as the glue documents.

use pi_durable_jsrt::{HostOptions, install};
use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::json;

async fn run(src: &str) -> (serde_json::Value, pi_durable_jsrt::DurableHost) {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let host = install(&rt, HostOptions::default()).await.unwrap();
    let ns = rt
        .eval_module(src, "/chord-embedded-test.mjs")
        .await
        .unwrap();
    let value = ns.call_method("run", vec![]).await.unwrap();
    (value, host)
}

#[tokio::test]
async fn context_values_keep_identity_and_mirror_cancellation_natively() {
    let (value, host) = run(r#"
        import { BACKGROUND_CONTEXT, createContextKey, withCancel, withContextValue, withoutAbortSignal } from "@earendil-works/chord/context";
        export async function run() {
            const key = createContextKey("owner");
            const stored = { id: 1 };
            const { context, cancel } = withCancel(withContextValue(key, stored, BACKGROUND_CONTEXT));
            const before = context.abortSignal.aborted;
            cancel(new Error("stop"));
            let ranSpan = false;
            await context.telemetry.startSpan({ name: "noop" }, () => { ranSpan = true; });
            return {
                identity: context.value(key) === stored,
                text: String(withoutAbortSignal(context)),
                before, after: context.abortSignal.aborted, ranSpan,
            };
        }
    "#).await;
    assert_eq!(
        value,
        json!({
            "identity": true,
            "text": "[Context BACKGROUND_CONTEXT].WithValue(owner).WithValue(chord.abortSignal).WithValue(chord.abortSignal)",
            "before": false, "after": true, "ranSpan": true,
        })
    );
    let counters = host.counters();
    assert!(
        counters.get("context.abort").copied().unwrap_or(0) >= 1,
        "{counters:?}"
    );
}

#[tokio::test]
async fn delta_codec_and_diff_keep_payload_identity() {
    let (value, host) = run(r#"
        import { applyImmutable, decoder, diffRevisions, encoder, PathError, UnsafePathError } from "@earendil-works/chord/delta";
        export async function run() {
            const shared = { id: "c", stable: {} };
            const changed = { id: "c", stable: shared.stable, label: "x" };
            const ops = diffRevisions({ values: [{ id: "a" }, shared] }, { values: [changed] });
            const payload = { big: [1, 2, 3] };
            const wire = encoder().encode([["s", ["x"], payload], ["s", ["x"], 2], ["p", ["y"], 0, 0, [payload]]]);
            const decoded = decoder().decode(wire);
            const errors = [];
            for (const op of [["s", ["__proto__", "x"], 1], ["s", ["missing", "x"], 1], ["t", ["x"], -1]]) {
                try { applyImmutable({ x: "" }, [op]); } catch (e) { errors.push(e instanceof UnsafePathError ? "unsafe" : e instanceof PathError ? "path" : e.name + ":" + e.message); }
            }
            return {
                ops,
                wirePayloadIdentity: wire[0][2] === payload && decoded[0][2] === payload && decoded[2][4][0] === payload,
                wire: JSON.parse(JSON.stringify(wire)),
                errors,
            };
        }
    "#).await;
    assert_eq!(
        value,
        json!({
            "ops": [["p", ["values"], 0, 1, []], ["s", ["values", 0, "label"], "x"]],
            "wirePayloadIdentity": true,
            "wire": [["s", ["x"], {"big": [1, 2, 3]}], ["s", 2], ["p", ["y"], 0, 0, [{"big": [1, 2, 3]}]]],
            "errors": ["unsafe", "path", "TypeError:t shape"],
        })
    );
    assert!(host.counters().contains_key("delta.diff"));
}

#[tokio::test]
async fn copy_json_and_sqlite_prefixes_validate_natively() {
    let (value, host) = run(r#"
        import { copyJson, isJsonValue } from "@earendil-works/chord";
        import { validateSqliteSchemaPrefix } from "@earendil-works/chord/sqlite";
        export async function run() {
            const outcome = (f) => { try { f(); return "ok"; } catch (e) { return e.message; } };
            const nullProto = Object.create(null);
            nullProto.a = { b: 1 };
            const copied = copyJson(nullProto);
            return {
                nullProto: Object.getPrototypeOf(copied) === null && copied.a.b === 1 && copied.a !== nullProto.a,
                nonFinite: outcome(() => copyJson({ x: Infinity })),
                lone: copyJson("\ud800").charCodeAt(0),
                json: isJsonValue([1, { a: null }]),
                prefix: outcome(() => validateSqliteSchemaPrefix("sqlite_x")),
                good: outcome(() => validateSqliteSchemaPrefix("pi_durable")),
            };
        }
    "#).await;
    assert_eq!(
        value,
        json!({
            "nullProto": true,
            "nonFinite": "Value contains a non-finite number and is not strict JSON",
            "lone": 0xd800,
            "json": true,
            "prefix": "SQLite schema prefix \"sqlite_x\" is reserved",
            "good": "ok",
        })
    );
    let counters = host.counters();
    assert!(
        counters.contains_key("json.copy") && counters.contains_key("sqlite.validateSchemaPrefix")
    );
}

#[tokio::test]
async fn node_sqlite_adapter_applies_migrations() {
    let (value, _host) = run(r#"
        import { applySqliteMigrations } from "@earendil-works/chord/sqlite";
        import { openNodeSqliteDatabase } from "@earendil-works/chord/sqlite/node";
        export async function run() {
            const database = await openNodeSqliteDatabase(":memory:");
            await applySqliteMigrations(database, [
                { version: 1, statements: ["CREATE TABLE t (v INTEGER)"] },
                { version: 2, statements: ["INSERT INTO t (v) VALUES (7)"] },
            ], { table: "meta" });
            const rows = await database.all("SELECT v FROM t");
            const version = await database.get("SELECT version FROM meta");
            const rolledBack = await database.transaction(async (tx) => {
                await tx.run("INSERT INTO t (v) VALUES (?)", 8);
                throw new Error("undo");
            }).catch((e) => e.message);
            const count = await database.get("SELECT count(*) AS n FROM t");
            await database.close();
            return { rows, version, rolledBack, count };
        }
    "#)
    .await;
    assert_eq!(
        value,
        json!({ "rows": [{"v": 7}], "version": {"version": 2}, "rolledBack": "undo", "count": {"n": 1} })
    );
}

#[tokio::test]
async fn a_js_context_crosses_to_rust_with_its_cancellation() {
    use pi_durable_jsrt::{Args, Dispatch, HostValue, Methods};
    use std::sync::Arc;
    struct Probe;
    impl Dispatch for Probe {
        fn register(&self, m: &mut Methods) {
            m.sync("probe.context", |_, args: Args| {
                let context = pi_durable_jsrt::dispatch::chord::context_from(args.get(0))?;
                Ok(HostValue::object([
                    ("text", HostValue::string(&context.to_string())),
                    ("cancelled", HostValue::Bool(context.check().is_err())),
                ]))
            });
        }
    }
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let mut dispatch = pi_durable_jsrt::dispatch::defaults();
    dispatch.push(Arc::new(Probe));
    install(
        &rt,
        HostOptions {
            vitest: false,
            dispatch,
        },
    )
    .await
    .unwrap();
    let ns = rt
        .eval_module(
            r#"
        import { call } from "pi-durable:bridge";
        import { nativeContext } from "pi-durable:internal/chord/context";
        import { BACKGROUND_CONTEXT, createContextKey, withCancel, withContextValue } from "@earendil-works/chord/context";
        export function run() {
            const { context, cancel } = withCancel(withContextValue(createContextKey("k"), 1, BACKGROUND_CONTEXT));
            const before = call("probe.context", nativeContext(context));
            cancel("stop");
            const after = call("probe.context", nativeContext(context));
            const background = call("probe.context", undefined);
            const foreign = call("probe.context", nativeContext({ abortSignal: AbortSignal.abort() }));
            return { before, after, background, foreign };
        }
    "#,
            "/context-crossing.mjs",
        )
        .await
        .unwrap();
    let value = ns.call_method("run", vec![]).await.unwrap();
    assert_eq!(
        value,
        json!({
            "before": {"text": "[Context BACKGROUND_CONTEXT].WithValue(k).WithValue(chord.abortSignal)", "cancelled": false},
            "after": {"text": "[Context BACKGROUND_CONTEXT].WithValue(k).WithValue(chord.abortSignal)", "cancelled": true},
            "background": {"text": "[Context BACKGROUND_CONTEXT]", "cancelled": false},
            "foreign": {"text": "[Context BACKGROUND_CONTEXT].WithValue(chord.abortSignal)", "cancelled": true},
        })
    );
}

#[tokio::test]
async fn contexts_derived_from_a_foreign_context_keep_its_values() {
    let (value, _host) = run(r#"
        import { createContextKey, withContextValue, withoutAbortSignal, withAbortSignal } from "@earendil-works/chord/context";
        export async function run() {
            const outer = createContextKey("outer");
            const inner = createContextKey("inner");
            const foreign = { abortSignal: undefined, telemetry: undefined, value: (key) => (key === outer ? "from-foreign" : undefined) };
            const derived = withoutAbortSignal(withContextValue(inner, undefined, withAbortSignal(new AbortController().signal, foreign)));
            return { outer: derived.value(outer), innerBound: derived.value(inner) === undefined, signal: derived.abortSignal === undefined };
        }
    "#).await;
    assert_eq!(
        value,
        json!({ "outer": "from-foreign", "innerBound": true, "signal": true })
    );
}
