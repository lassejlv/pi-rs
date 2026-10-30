//! Port of `packages/codemode/test/sandbox.test.ts`. Every behavior test runs on the thread
//! backend and on the remote backend (in process, every message round-tripped through JSON).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::*;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use pi_codemode::*;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// Base64 of the leading bytes of each format. image() only inspects the signature.
const PNG: &str = "iVBORw0KGgo=";
const JPEG: &str = "/9j/4A==";
const GIF: &str = "R0lGODlh";
const WEBP: &str = "UklGRgAAAABXRUJQ";

#[test]
fn embedded_sources_parse_as_javascript() {
    let mut context = boa_engine::Context::default();
    let parsed = boa_engine::Script::parse(boa_engine::Source::from_bytes(PRELUDE_SOURCE.as_bytes()), None, &mut context);
    assert!(parsed.is_ok());
}

backend_tests!(
    returns_the_scripts_return_value_after_a_json_round_trip,
    supports_top_level_await,
    collects_text_image_and_console_output_in_order,
    rejects_invalid_text_and_image_arguments,
    accepts_wrapped_and_large_base64_image_data,
    ends_the_script_successfully_on_exit,
    keeps_output_produced_before_a_failure,
    reports_syntax_errors_with_the_scripts_line_number,
    reports_thrown_errors_with_the_scripts_line_number,
    formats_stacks_like_v8_without_prelude_frames,
    reports_non_error_throws,
    reports_a_non_serializable_return_value_as_a_script_error,
    exposes_tools_as_async_functions_and_records_calls,
    runs_concurrent_calls_and_lists_tool_names,
    exposes_tools_under_normalized_identifiers,
    passes_undefined_arguments_and_results_through,
    turns_tool_errors_into_catchable_errors,
    rejects_calls_to_unknown_tools,
    aborts_unawaited_calls_when_the_script_returns,
    names_close_matches_when_a_script_reads_a_tool_that_does_not_exist,
    supports_register_and_unregister_between_executions,
    store_reads_the_snapshot_and_reports_writes,
    store_returns_copies,
    store_rejects_invalid_keys_values_and_oversized_writes,
    store_explains_oversized_writes,
    reserves_the_store_and_load_names,
    exposes_globals_as_top_level_functions_without_recording_them,
    groups_namespaced_globals_and_spreads_arguments,
    names_the_members_of_a_namespace,
    rejects_invalid_and_reserved_global_names,
    terminates_a_synchronous_infinite_loop_on_timeout,
    terminates_a_loop_after_an_await_on_timeout,
    runs_without_a_deadline_when_timeout_is_infinite,
    fails_a_script_that_waits_on_a_promise_nothing_can_settle,
    terminates_a_microtask_spinning_loop_on_timeout,
    aborts_via_signal_and_cancels_in_flight_calls,
    fails_a_script_whose_output_passes_the_limits,
    rejects_execute_after_close,
    runs_executions_in_parallel_without_sharing_state,
    ends_deep_recursion_with_a_range_error,
    has_no_host_globals,
    keeps_eval_and_function_inside_the_vm,
    rejects_dynamic_import,
    ignores_patches_to_built_ins,
    freezes_intrinsics_only_reachable_from_instances,
    still_lets_instances_override_properties_of_frozen_prototypes,
    reports_errors_whose_name_or_message_is_not_a_string,
    keeps_tools_and_console_frozen,
);

// ---------------------------------------------------------------------------
// Script execution
// ---------------------------------------------------------------------------

async fn returns_the_scripts_return_value_after_a_json_round_trip(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    assert_matches(
        &run(&sandbox, "return { a: 1, b: [true, 'x'] }").await,
        json!({ "ok": true, "value": { "a": 1, "b": [true, "x"] }, "output": [], "calls": [] }),
    );
    assert_matches(&run(&sandbox, "return 'plain'").await, json!({ "ok": true, "value": "plain" }));
    let empty = run(&sandbox, "").await;
    assert!(empty.ok());
    assert_eq!(empty.value(), None);
    sandbox.close().await;
}

async fn supports_top_level_await(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(&sandbox, "const x = await Promise.resolve(41); return x + 1").await;
    assert_matches(&result, json!({ "ok": true, "value": 42 }));
}

async fn collects_text_image_and_console_output_in_order(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(
        &sandbox,
        &format!(
            r#"
            console.log("hello", 1, {{ a: 1 }});
            text({{ json: true }});
            text(undefined);
            text(7);
            image("data:image/png;base64,{PNG}");
            image({{ image_url: "data:image/jpeg;base64,{JPEG}" }});
            image({{ type: "image", data: "{GIF}", mimeType: "image/gif" }});
            image("data:image/png;base64,{WEBP}");
            image({{ type: "image", data: "{PNG}" }});
            console.error(new Error("bad"));
            return null;
        "#
        ),
    )
    .await;
    assert!(result.ok());
    let output = json(&result)["output"].clone();
    let items = output.as_array().unwrap();
    assert_eq!(
        Value::Array(items[..items.len() - 1].to_vec()),
        json!([
            { "type": "text", "text": "hello 1 {\"a\":1}", "console": true },
            { "type": "text", "text": "{\"json\":true}" },
            { "type": "text", "text": "undefined" },
            { "type": "text", "text": "7" },
            { "type": "image", "data": PNG, "mimeType": "image/png" },
            { "type": "image", "data": JPEG, "mimeType": "image/jpeg" },
            { "type": "image", "data": GIF, "mimeType": "image/gif" },
            // The MIME type comes from the data, not from the declared type.
            { "type": "image", "data": WEBP, "mimeType": "image/webp" },
            { "type": "image", "data": PNG, "mimeType": "image/png" },
        ])
    );
    let last = items.last().unwrap();
    assert_eq!(last["console"], json!(true));
    assert!(last["text"].as_str().unwrap().starts_with("Error: bad"), "{last}");
}

async fn rejects_invalid_text_and_image_arguments(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(
        &sandbox,
        r#"
        const errors = [];
        const circular = {};
        circular.self = circular;
        for (const run of [
            () => text(circular),
            () => image(""),
            () => image("https://example.com/a.png"),
            () => image("data:image/png,raw"),
            () => image({ type: "text", text: "x" }),
            () => image({ type: "image", data: "" }),
            () => image(42),
            () => image("data:image/png;base64,AAAA!"),
            () => image("data:image/png;base64,AAAAA"),
            () => image("data:image/png;base64,AA=A"),
            () => image("data:image/png;base64,"),
            () => image("data:image/png;base64,AAAA\n[Output truncated]"),
            () => image({ type: "image", data: "AAAA!", mimeType: "image/png" }),
            () => image("data:image/png;base64,AAAA"),
            () => image("data:image/png;base64,QUJD"),
            () => image("data:image/jpeg;base64,/9j/9w=="),
        ]) {
            try { run(); errors.push("no error"); } catch (error) { errors.push(error.name + ": " + error.message); }
        }
        return errors;
    "#,
    )
    .await;
    assert!(result.ok(), "{:?}", result.error());
    assert!(result.output.is_empty());
    let errors: Vec<String> = serde_json::from_value(result.value().unwrap().clone()).unwrap();
    // The engine's own message: QuickJS and V8 say "circular", and so does the patched Boa
    // ("Converting circular structure to JSON", vendor/boa_engine/PATCH.md).
    assert!(errors[0].starts_with("TypeError: ") && errors[0].contains("circular"), "{}", errors[0]);
    let mut expected = vec![
        "TypeError: image expects a non-empty image URL string, an object with image_url, or an image block { type: \"image\", data, mimeType }".to_string(),
        "TypeError: remote image URLs are not supported in tool outputs. Pass a base64 data URI instead".to_string(),
        "TypeError: invalid image output. Pass a base64 data URI instead".to_string(),
        "TypeError: image only accepts image blocks, got \"text\"".to_string(),
        "TypeError: image expected image block data".to_string(),
        "TypeError: image expects a non-empty image URL string, an object with image_url, or an image block { type: \"image\", data, mimeType }".to_string(),
    ];
    expected.extend(std::iter::repeat_n(
        "TypeError: invalid image output. The image data is not valid base64 (truncated or corrupted?)".to_string(),
        6,
    ));
    expected.extend(std::iter::repeat_n(
        "TypeError: invalid image output. The image data is not a PNG, JPEG, GIF, or WebP image".to_string(),
        3,
    ));
    assert_eq!(errors[1..].to_vec(), expected);
}

// https://github.com/earendil-works/pi/issues/10215
async fn accepts_wrapped_and_large_base64_image_data(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let large = format!("iVBORw0KGgoA{}", "QUJD".repeat(256 * 1024));
    let result = run(
        &sandbox,
        &format!("image(\"data:image/png;base64,iVBORw0K\\r\\nGgo=\\n\");\nimage(\"data:image/png;base64,{large}\");"),
    )
    .await;
    assert!(result.ok(), "{:?}", result.error());
    assert_eq!(
        result.output,
        vec![
            CodemodeOutputItem::Image { data: PNG.into(), mime_type: "image/png".into() },
            CodemodeOutputItem::Image { data: large, mime_type: "image/png".into() },
        ]
    );
}

async fn ends_the_script_successfully_on_exit(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![echo()]);
    let result = run(
        &sandbox,
        r#"
        text("before");
        store("k", 1);
        await tools.echo(1);
        try { exit(); } catch {}
        text("after");
        return "unreachable";
    "#,
    )
    .await;
    assert_matches(
        &result,
        json!({ "ok": true, "output": [{ "type": "text", "text": "before" }], "storeWrites": { "set": { "k": 1 }, "delete": [] } }),
    );
    assert_eq!(result.value(), None);
}

async fn keeps_output_produced_before_a_failure(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(&sandbox, "text(\"partial\");\nthrow new Error(\"boom\")").await;
    assert_matches(&result, json!({ "ok": false, "output": [{ "type": "text", "text": "partial" }] }));
}

async fn reports_syntax_errors_with_the_scripts_line_number(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(&sandbox, "const a = 1;\nconst b = ;\nreturn a").await;
    let error = result.error().expect("error");
    assert_eq!(error.kind, CodemodeErrorKind::Script);
    assert_eq!(error.name.as_deref(), Some("SyntaxError"));
    assert!(error.stack.as_deref().unwrap().contains("codemode.js:2"), "{error:?}");
}

async fn reports_thrown_errors_with_the_scripts_line_number(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(&sandbox, "const a = 1;\nthrow new TypeError('boom ' + a)").await;
    assert_matches(&result, json!({ "ok": false, "error": { "kind": "script", "name": "TypeError", "message": "boom 1" } }));
    assert!(result.error().unwrap().stack.as_deref().unwrap().contains("codemode.js:2"));
}

async fn formats_stacks_like_v8_without_prelude_frames(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(&sandbox, "console.log(new Error('inner'));\nthrow new RangeError('outer')").await;
    let stack = result.error().unwrap().stack.clone().unwrap();
    let pattern = regex_lite("RangeError: outer\n    at ", "codemode.js:2");
    assert!(pattern(&stack), "{stack}");
    assert!(!stack.contains("codemode-prelude.js"));
    let CodemodeOutputItem::Text { text, .. } = &result.output[0] else { panic!("text") };
    assert!(regex_lite("Error: inner\n    at ", "codemode.js:1")(text), "{text}");
    assert!(!serde_json::to_string(&result.output).unwrap().contains("codemode-prelude.js"));
}

/// `^{head}.*{tail}` on the first line after `head`.
fn regex_lite(head: &'static str, tail: &'static str) -> impl Fn(&str) -> bool {
    move |text: &str| text.starts_with(head) && text[head.len()..].lines().next().is_some_and(|line| line.contains(tail))
}

async fn reports_non_error_throws(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(&sandbox, "throw { code: 7 }").await;
    assert_matches(&result, json!({ "ok": false, "error": { "kind": "script", "message": "{\"code\":7}" } }));
}

async fn reports_a_non_serializable_return_value_as_a_script_error(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(&sandbox, "return 10n").await;
    assert_matches(&result, json!({ "ok": false, "error": { "kind": "script", "name": "TypeError" } }));
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

async fn exposes_tools_as_async_functions_and_records_calls(backend: Backend) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let sandbox = create_sandbox(
        backend,
        vec![CodemodeTool::new_sync("add", move |args, _| {
            let args = args.unwrap();
            recorder.lock().push(args.clone());
            Ok(Some(json!({ "sum": args["a"].as_f64().unwrap() + args["b"].as_f64().unwrap() })))
        })],
    );
    let result = run(
        &sandbox,
        r#"
        const first = await tools.add({ a: 1, b: 2 });
        const second = await tools.add({ a: first.sum, b: 10 });
        return second.sum;
    "#,
    )
    .await;
    assert_matches(&result, json!({ "ok": true, "value": 13 }));
    assert_eq!(*seen.lock(), vec![json!({ "a": 1, "b": 2 }), json!({ "a": 3, "b": 10 })]);
    let calls: Vec<(String, CodemodeCallStatus)> = result.calls.iter().map(|call| (call.name.clone(), call.status)).collect();
    assert_eq!(calls, vec![("add".into(), CodemodeCallStatus::Ok), ("add".into(), CodemodeCallStatus::Ok)]);
    assert!(result.calls.iter().all(|call| call.duration_ms >= 0.0));
}

fn delay_tool() -> CodemodeTool {
    CodemodeTool::new("delay", |args, _| async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(args)
    })
}

async fn runs_concurrent_calls_and_lists_tool_names(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![echo(), delay_tool()]);
    let result = run(
        &sandbox,
        r#"
        const [a, b, c] = await Promise.all([tools.delay(1), tools.delay(2), tools.echo(3)]);
        return { values: [a, b, c], names: Object.keys(tools) };
    "#,
    )
    .await;
    assert_matches(&result, json!({ "ok": true, "value": { "values": [1, 2, 3], "names": ["echo", "delay"] } }));
}

async fn exposes_tools_under_normalized_identifiers(backend: Backend) {
    let sandbox = create_sandbox(
        backend,
        vec![
            CodemodeTool::new_sync("my-tool", |_, _| Ok(Some(json!("dash")))).with_description("Dashes"),
            CodemodeTool::new_sync("my_tool", |_, _| Ok(Some(json!("underscore")))).with_description("Shadowed"),
            CodemodeTool::new_sync("mcp__docs__search", |_, _| Ok(Some(json!("mcp")))),
        ],
    );
    let result = run(
        &sandbox,
        r#"
        try { ALL_TOOLS.push({}); } catch {}
        return {
            all: ALL_TOOLS,
            calls: [await tools.my_tool(), await tools["my-tool"](), await tools.mcp__docs__search()],
        };
    "#,
    )
    .await;
    assert_matches(
        &result,
        json!({
            "ok": true,
            "value": {
                "all": [{ "name": "my_tool", "description": "Dashes" }, { "name": "mcp__docs__search", "description": "" }],
                "calls": ["dash", "dash", "mcp"],
            },
        }),
    );
}

async fn passes_undefined_arguments_and_results_through(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![CodemodeTool::new_sync("noop", |args, _| Ok(args))]);
    let result = run(&sandbox, "return [await tools.noop(), await tools.noop(null)]").await;
    assert_matches(&result, json!({ "ok": true, "value": [null, null] }));
}

async fn turns_tool_errors_into_catchable_errors(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![CodemodeTool::new_sync("fail", |_, _| Err("tool exploded".to_string()))]);
    let result = run(
        &sandbox,
        r#"
        try {
            await tools.fail();
            return "no error";
        } catch (error) {
            return { isError: error instanceof Error, message: error.message };
        }
    "#,
    )
    .await;
    assert_matches(&result, json!({ "ok": true, "value": { "isError": true, "message": "tool exploded" } }));
    assert_matches(&result, json!({ "calls": [{ "name": "fail", "status": "error" }] }));
}

async fn rejects_calls_to_unknown_tools(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(&sandbox, "return await tools.missing()").await;
    assert_matches(&result, json!({ "ok": false, "error": { "kind": "script", "name": "TypeError" } }));
}

async fn aborts_unawaited_calls_when_the_script_returns(backend: Backend) {
    let aborted = Arc::new(AtomicBool::new(false));
    let flag = aborted.clone();
    let sandbox = create_sandbox(
        backend,
        vec![CodemodeTool::new("slow", move |_, context| {
            let flag = flag.clone();
            async move {
                context.signal.cancelled().await;
                flag.store(true, Ordering::SeqCst);
                Err("aborted".to_string())
            }
        })],
    );
    let result = run(&sandbox, "tools.slow(); return 'early'").await;
    assert_matches(&result, json!({ "ok": true, "value": "early", "calls": [{ "name": "slow", "status": "cancelled" }] }));
    wait_for(|| aborted.load(Ordering::SeqCst)).await;
}

async fn names_close_matches_when_a_script_reads_a_tool_that_does_not_exist(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![echo(), CodemodeTool::new_sync("web-search", |_, _| Ok(Some(json!(""))))]);
    let attempt = async |expression: &str| -> Value {
        let result = run(&sandbox, &format!("return {expression};")).await;
        match result.outcome {
            Ok(success) => success.value.unwrap_or(Value::Null),
            Err(error) => Value::String(error.message),
        }
    };
    assert_eq!(
        attempt("tools.Echo").await,
        json!("tools.Echo does not exist. Did you mean tools.echo? ALL_TOOLS lists every tool; searchTools(query) finds tools by topic. Check for a member with \"Echo\" in tools.")
    );
    assert!(attempt("tools.websearch").await.as_str().unwrap().contains("Did you mean tools.web_search?"));
    assert!(attempt("tools.nothing").await.as_str().unwrap().contains("Available: echo, web_search."));
    assert_eq!(
        attempt("['echo' in tools, 'nothing' in tools, String(tools.toString), JSON.stringify(tools)]").await,
        json!([true, false, "undefined", "{}"])
    );
}

async fn supports_register_and_unregister_between_executions(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    sandbox.register_tool(echo()).unwrap();
    assert!(sandbox.register_tool(echo()).unwrap_err().0.contains("already registered"));
    assert_eq!(sandbox.tools().iter().map(|tool| tool.name.clone()).collect::<Vec<_>>(), vec!["echo"]);
    assert_matches(&run(&sandbox, "return await tools.echo('a')").await, json!({ "ok": true, "value": "a" }));
    assert!(sandbox.unregister_tool("echo"));
    assert_matches(&run(&sandbox, "return 'echo' in tools").await, json!({ "ok": true, "value": false }));
}

// ---------------------------------------------------------------------------
// Store and load
// ---------------------------------------------------------------------------

async fn store_reads_the_snapshot_and_reports_writes(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run_with(
        &sandbox,
        r#"
        const seen = load("counter");
        store("counter", seen + 1);
        store("list", [1, { a: null }]);
        store("old", undefined);
        return [seen, load("counter"), load("missing"), load("old")];
    "#,
        CodemodeExecuteOptions { store: store(json!({ "counter": 41, "old": "x" })), ..Default::default() },
    )
    .await;
    assert_matches(
        &result,
        json!({
            "ok": true,
            // undefined array elements become null in the JSON round trip of the return value.
            "value": [41, 42, null, null],
            "storeWrites": { "set": { "counter": 42, "list": [1, { "a": null }] }, "delete": ["old"] },
        }),
    );
}

async fn store_returns_copies(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run_with(
        &sandbox,
        r#"const value = load("obj"); value.a = 2; const kept = { b: 1 }; store("kept", kept); kept.b = 2;
        return [load("obj").a, load("kept").b];"#,
        CodemodeExecuteOptions { store: store(json!({ "obj": { "a": 1 } })), ..Default::default() },
    )
    .await;
    assert_matches(&result, json!({ "ok": true, "value": [1, 1], "storeWrites": { "set": { "kept": { "b": 1 } } } }));
}

async fn store_rejects_invalid_keys_values_and_oversized_writes(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(
        &sandbox,
        r#"
        const attempt = (fn) => { try { fn(); return "ok"; } catch (error) { return error.name; } };
        return [
            attempt(() => store(1, "x")),
            attempt(() => load({})),
            attempt(() => store("fn", () => 1)),
            attempt(() => store("big", "x".repeat(300 * 1024))),
            attempt(() => { for (let i = 0; i < 8; i++) store("k" + i, "x".repeat(200 * 1024)); }),
        ];
    "#,
    )
    .await;
    assert_matches(&result, json!({ "ok": true, "value": ["TypeError", "TypeError", "TypeError", "RangeError", "RangeError"] }));
}

async fn store_explains_oversized_writes(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(&sandbox, "store(\"img\", \"x\".repeat(300 * 1024));").await;
    let message = &result.error().unwrap().message;
    assert!(message.contains("store(\"img\") value has 307202 characters of JSON"), "{message}");
    assert!(message.contains("Show images with image()"));
}

fn noop_tool(name: &str) -> CodemodeTool {
    CodemodeTool::new_sync(name, |_, _| Ok(None))
}

fn globals_sandbox(backend: Backend, globals: Vec<CodemodeTool>) -> Result<CodemodeSandbox, CodemodeSandboxError> {
    new_sandbox(backend, CodemodeSandboxBaseOptions { globals, timeout_ms: Some(10_000.0), ..Default::default() })
}

async fn reserves_the_store_and_load_names(backend: Backend) {
    for name in ["store", "load"] {
        let error = globals_sandbox(backend, vec![noop_tool(name)]).unwrap_err();
        assert!(error.0.contains("Invalid global"), "{error}");
    }
}

// ---------------------------------------------------------------------------
// Globals
// ---------------------------------------------------------------------------

async fn exposes_globals_as_top_level_functions_without_recording_them(backend: Backend) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let sandbox = new_sandbox(
        backend,
        CodemodeSandboxBaseOptions {
            tools: vec![echo()],
            globals: vec![CodemodeTool::new("attach", move |args, _| {
                recorder.lock().push(args.unwrap_or(Value::Null));
                async { Ok(None) }
            })],
            timeout_ms: Some(10_000.0),
            ..Default::default()
        },
    )
    .unwrap();
    let result = run(
        &sandbox,
        r#"
        await attach({ ref: 1 });
        attach("not awaited");
        return [typeof attach, typeof globalThis.attach, await tools.echo(2)];
    "#,
    )
    .await;
    assert_matches(&result, json!({ "ok": true, "value": ["function", "function", 2] }));
    assert_eq!(result.calls.iter().map(|call| call.name.clone()).collect::<Vec<_>>(), vec!["echo"]);
    // Messages are handled in order, so an unawaited global still runs before the script settles.
    assert_eq!(*seen.lock(), vec![json!({ "ref": 1 }), json!("not awaited")]);
}

async fn groups_namespaced_globals_and_spreads_arguments(backend: Backend) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let sandbox = globals_sandbox(
        backend,
        vec![
            CodemodeTool::new_sync("models.list", move |args, _| {
                recorder.lock().push(args.unwrap_or(Value::Null));
                Ok(None)
            })
            .with_spread(true),
            CodemodeTool::new_sync("models.first", |args, _| Ok(args)),
        ],
    )
    .unwrap();
    let result = run(
        &sandbox,
        r#"
        await models.list("classifier", undefined, 3);
        await models.list();
        try { models.extra = 1; } catch {}
        return [Object.keys(models), await models.first("a", "ignored"), "extra" in models];
    "#,
    )
    .await;
    assert_matches(&result, json!({ "ok": true, "value": [["list", "first"], "a", false] }));
    // undefined array elements become null in the JSON round trip.
    assert_eq!(*seen.lock(), vec![json!(["classifier", null, 3]), json!([])]);
}

async fn names_the_members_of_a_namespace(backend: Backend) {
    let sandbox = globals_sandbox(backend, vec![noop_tool("models.classify"), noop_tool("models.generateImages")]).unwrap();
    let result = run(&sandbox, "await models.generateImage();").await;
    assert_eq!(
        result.error().unwrap().message,
        "models.generateImage does not exist. Did you mean models.generateImages? Check for a member with \"generateImage\" in models."
    );
}

async fn rejects_invalid_and_reserved_global_names(backend: Backend) {
    for name in ["a.b.c", "a.", ".a", "tools.x", "store.x", "a.not-valid", "not-valid", "tools", "console"] {
        let error = globals_sandbox(backend, vec![noop_tool(name)]).unwrap_err();
        assert!(error.0.contains("Invalid global"), "{name}: {error}");
    }
    let error = globals_sandbox(backend, vec![noop_tool("models"), noop_tool("models.list")]).unwrap_err();
    assert!(error.0.contains("conflicts with the namespace"), "{error}");
}

// ---------------------------------------------------------------------------
// Limits and lifetime
// ---------------------------------------------------------------------------

fn timeout(ms: f64) -> CodemodeExecuteOptions {
    CodemodeExecuteOptions { timeout_ms: Some(ms), ..Default::default() }
}

async fn terminates_a_synchronous_infinite_loop_on_timeout(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let started = Instant::now();
    let result = run_with(&sandbox, "while (true) {}", timeout(200.0)).await;
    assert_matches(&result, json!({ "ok": false, "error": { "kind": "timeout" } }));
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// Boa discards errors raised in an async function that resumed after an await; the instrumented
/// loop still stops the script and the VM reports why.
async fn terminates_a_loop_after_an_await_on_timeout(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![echo()]);
    let started = Instant::now();
    let result = run_with(&sandbox, "await tools.echo(1); for (;;) { [1, 2].map((x) => x); }", timeout(200.0)).await;
    assert_matches(&result, json!({ "ok": false, "error": { "kind": "timeout" } }));
    assert!(started.elapsed() < Duration::from_secs(5));
}

async fn runs_without_a_deadline_when_timeout_is_infinite(backend: Backend) {
    let sandbox = create_sandbox(
        backend,
        vec![CodemodeTool::new("wait", |_, _| async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(Some(json!("late")))
        })],
    );
    let result = run_with(&sandbox, "return await tools.wait()", timeout(f64::INFINITY)).await;
    assert_matches(&result, json!({ "ok": true, "value": "late" }));
}

async fn fails_a_script_that_waits_on_a_promise_nothing_can_settle(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![echo()]);
    let result =
        run_with(&sandbox, "await tools.echo(1); await new Promise(() => {}); return 'never'", timeout(f64::INFINITY)).await;
    let error = result.error().unwrap();
    assert_eq!(error.kind, CodemodeErrorKind::Script);
    assert!(error.message.contains("can never settle"));
    // Returning while a call is still pending is not a stall.
    let returned = run_with(&sandbox, "tools.echo(2); return 'early'", timeout(f64::INFINITY)).await;
    assert_matches(&returned, json!({ "ok": true, "value": "early" }));
}

async fn terminates_a_microtask_spinning_loop_on_timeout(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run_with(&sandbox, "while (true) await null", timeout(200.0)).await;
    assert_matches(&result, json!({ "ok": false, "error": { "kind": "timeout" } }));
}

async fn aborts_via_signal_and_cancels_in_flight_calls(backend: Backend) {
    let tool_signal: Arc<Mutex<Option<CancellationToken>>> = Arc::default();
    let called = Arc::new(tokio::sync::Notify::new());
    let (recorder, notify) = (tool_signal.clone(), called.clone());
    let sandbox = create_sandbox(
        backend,
        vec![CodemodeTool::new("hang", move |_, context| {
            *recorder.lock() = Some(context.signal.clone());
            notify.notify_one();
            std::future::pending()
        })],
    );
    let first = sandbox.clone();
    let promise = tokio::spawn(async move { run(&first, "await tools.hang(); return 'never'").await });
    called.notified().await;
    let controller = AbortController::new();
    controller.abort_with_reason("user cancelled");
    assert!(!tool_signal.lock().as_ref().unwrap().is_cancelled());
    let result =
        run_with(&sandbox, "await tools.hang()", CodemodeExecuteOptions { signal: Some(controller.signal()), ..Default::default() })
            .await;
    assert_matches(&result, json!({ "ok": false, "error": { "kind": "aborted", "message": "user cancelled" } }));

    let second = AbortController::new();
    let signal = second.signal();
    let other = sandbox.clone();
    let pending = tokio::spawn(async move {
        run_with(&other, "await tools.hang(); return 'never'", CodemodeExecuteOptions { signal: Some(signal), ..Default::default() })
            .await
    });
    called.notified().await;
    second.abort();
    let aborted = pending.await.unwrap();
    // Node's `controller.abort()` reason is a DOMException: "This operation was aborted".
    assert_matches(
        &aborted,
        json!({
            "ok": false,
            "error": { "kind": "aborted", "message": "This operation was aborted" },
            "calls": [{ "name": "hang", "status": "cancelled" }]
        }),
    );
    assert!(tool_signal.lock().as_ref().unwrap().is_cancelled());

    sandbox.close().await;
    assert_matches(&promise.await.unwrap(), json!({ "ok": false, "error": { "kind": "aborted", "message": "Sandbox closed" } }));
}

// #10283: the host keeps all output, so a script that prints in a loop must not grow it without bound.
async fn fails_a_script_whose_output_passes_the_limits(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    for print in ["text(s)", "console.log(s)", "image(\"data:image/png;base64,\" + p)"] {
        let result = run_with(
            &sandbox,
            &format!(
                r#"
                const s = "x".repeat(1 << 20);
                const p = "iVBORw0KGgoA" + "A".repeat(1 << 20);
                for (;;) {{ try {{ {print}; }} catch {{}} }}
            "#
            ),
            timeout(60_000.0),
        )
        .await;
        let error = result.error().unwrap_or_else(|| panic!("{print}: no error"));
        assert_eq!(error.kind, CodemodeErrorKind::Script, "{print}: {error:?}");
        assert_eq!(error.name.as_deref(), Some("RangeError"));
        assert!(error.message.contains("script output exceeded"));
        let chars: usize = result.output.iter().map(CodemodeOutputItem::chars).sum();
        assert!(chars <= MAX_OUTPUT_CHARS);
        assert!(chars > MAX_OUTPUT_CHARS - (2 << 20));
    }

    let empty = run_with(&sandbox, "for (;;) text(\"\");", timeout(60_000.0)).await;
    assert_eq!(empty.error().unwrap().name.as_deref(), Some("RangeError"));
    assert_eq!(empty.output.len(), MAX_OUTPUT_ITEMS);
}

async fn rejects_execute_after_close(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    sandbox.close().await;
    let error = sandbox.execute("return 1", CodemodeExecuteOptions::default()).await.unwrap_err();
    assert!(error.0.contains("closed"));
}

async fn runs_executions_in_parallel_without_sharing_state(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let results = futures::future::join_all([
        run(&sandbox, "globalThis.shared = 'a'; await null; return globalThis.shared"),
        run(&sandbox, "globalThis.shared = 'b'; await null; return globalThis.shared"),
        run(&sandbox, "return typeof globalThis.shared"),
    ])
    .await;
    let values: Vec<Value> = results.iter().map(|result| result.value().cloned().unwrap_or(Value::Null)).collect();
    assert_eq!(values, vec![json!("a"), json!("b"), json!("undefined")]);
}

/// QuickJS throws a catchable `RangeError`; Boa's recursion limit ends the script, which reports
/// the same error.
async fn ends_deep_recursion_with_a_range_error(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(
        &sandbox,
        r#"
        let depth = 0;
        function dive() { depth++; dive(); }
        try { dive(); } catch (error) { return [error.name, depth > 1000]; }
    "#,
    )
    .await;
    // Catchable, as in QuickJS (vendor/boa_engine/PATCH.md).
    assert_matches(&result, json!({ "ok": true, "value": ["RangeError", true] }));
    // Uncaught, it ends the script as a script error.
    let uncaught = run(&sandbox, "function dive() { dive(); } dive();").await;
    assert_matches(
        &uncaught,
        json!({ "ok": false, "error": { "kind": "script", "name": "RangeError", "message": "Maximum call stack size exceeded" } }),
    );
}

// ---------------------------------------------------------------------------
// Escape hatches
// ---------------------------------------------------------------------------

async fn has_no_host_globals(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(
        &sandbox,
        r#"
        return [
            typeof process, typeof require, typeof module, typeof setTimeout, typeof fetch,
            typeof WebAssembly, typeof std, typeof os, typeof globalThis.constructor,
        ]
    "#,
    )
    .await;
    assert_matches(
        &result,
        json!({ "ok": true, "value": ["undefined", "undefined", "undefined", "undefined", "undefined", "undefined", "undefined", "undefined", "function"] }),
    );
}

async fn keeps_eval_and_function_inside_the_vm(backend: Backend) {
    // Code generation is allowed: it can only produce more code in the same VM.
    let sandbox = create_sandbox(backend, vec![echo()]);
    let result = run(
        &sandbox,
        r#"
        return [
            eval("typeof process"),
            new Function("return typeof process")(),
            tools.echo.constructor("return typeof require")(),
            (async () => {}).constructor("return typeof setTimeout")() instanceof Promise,
        ];
    "#,
    )
    .await;
    assert_matches(&result, json!({ "ok": true, "value": ["undefined", "undefined", "undefined", true] }));
}

async fn rejects_dynamic_import(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(
        &sandbox,
        r#"
        try { await import("node:fs"); return "imported"; } catch (error) { return error.constructor.name; }
    "#,
    )
    .await;
    assert!(result.ok(), "{:?}", result.error());
    assert_ne!(result.value(), Some(&json!("imported")));
}

// #10444: the prelude shares the built-ins with the script, so patching them could corrupt what
// the prelude sends to the host.
async fn ignores_patches_to_built_ins(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![echo()]);
    let result = run(
        &sandbox,
        r#"
        Array.prototype.toJSON = () => null;
        Object.prototype.toJSON = () => 5;
        Promise.prototype.then = () => {};
        Map.prototype.get = () => undefined;
        globalThis.JSON = { stringify: () => "x", parse: () => "x" };
        store("k", [1]);
        return [await tools.echo([2]), JSON.stringify({ a: 1 })];
    "#,
    )
    .await;
    assert_matches(&result, json!({ "ok": true, "value": [[2], "{\"a\":1}"], "storeWrites": { "set": { "k": [1] } } }));
}

async fn freezes_intrinsics_only_reachable_from_instances(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(
        &sandbox,
        r#"
        return [
            Object.getPrototypeOf(function* () {}).prototype,
            Object.getPrototypeOf(async function () {}),
            Object.getPrototypeOf(Int8Array).prototype,
            Object.getPrototypeOf([][Symbol.iterator]()),
            Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]())),
            Object.getPrototypeOf(new Map()[Symbol.iterator]()),
            Object.getPrototypeOf(/a/[Symbol.matchAll]("")),
        ].every((object) => Object.isFrozen(object));
    "#,
    )
    .await;
    assert_matches(&result, json!({ "ok": true, "value": true }));
}

async fn still_lets_instances_override_properties_of_frozen_prototypes(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(
        &sandbox,
        r#"
        const object = {};
        object.toString = () => "custom";
        function Legacy() {}
        Legacy.prototype = Object.create(Error.prototype);
        Legacy.prototype.constructor = Legacy;
        const bare = new Error();
        bare.message = "set later";
        class MyError extends Error {
            constructor(message) {
                super(message);
                this.name = "MyError";
            }
        }
        let patched = "silent";
        try { Error.prototype.name = "Patched"; } catch (error) { patched = error.constructor.name; }
        return [String(object), new Legacy().constructor === Legacy, bare.message, new MyError("x").name, Error.prototype.name, patched];
    "#,
    )
    .await;
    assert_matches(&result, json!({ "ok": true, "value": ["custom", true, "set later", "MyError", "Error", "TypeError"] }));
}

async fn reports_errors_whose_name_or_message_is_not_a_string(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![]);
    let result = run(&sandbox, "const error = new Error('x'); error.message = 42; throw error;").await;
    assert_matches(&result, json!({ "ok": false, "error": { "kind": "script", "name": "Error", "message": "42" } }));
}

async fn keeps_tools_and_console_frozen(backend: Backend) {
    let sandbox = create_sandbox(backend, vec![echo()]);
    let result = run(
        &sandbox,
        r#"
        try { tools.echo = () => 'nope'; } catch {}
        try { tools.extra = () => 'nope'; } catch {}
        try { globalThis.tools = null; } catch {}
        return ["extra" in tools, await tools.echo('still')];
    "#,
    )
    .await;
    assert_matches(&result, json!({ "ok": true, "value": [false, "still"] }));
}

// ---------------------------------------------------------------------------
// Bridge validation (the TS "worker host" tests with a raw worker)
// ---------------------------------------------------------------------------

/// A launcher whose "VM" posts the JSON messages listed in the script.
struct RawLauncher;

struct NoopChannel;

impl VmChannel for NoopChannel {
    fn post(&self, _message: HostToWorkerMessage) {}
    fn stop(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

impl VmLauncher for RawLauncher {
    fn launch(&self, data: VmStartData, events: VmEvents) -> BoxFuture<'static, Result<Box<dyn VmChannel>, String>> {
        let messages: Vec<Value> = serde_json::from_str(&data.code).unwrap_or_default();
        for message in messages {
            events.message_json(message);
        }
        Box::pin(async { Ok(Box::new(NoopChannel) as Box<dyn VmChannel>) })
    }
}

fn raw_sandbox() -> CodemodeSandbox {
    CodemodeSandbox::with_launcher(CodemodeSandboxBaseOptions::default(), Arc::new(RawLauncher)).unwrap()
}

#[tokio::test]
async fn accepts_well_formed_raw_messages() {
    let sandbox = raw_sandbox();
    let result = run(&sandbox, &json!([{ "type": "done", "ok": true, "value": "42", "writes": "[]" }]).to_string()).await;
    assert_matches(&result, json!({ "ok": true, "value": 42 }));
}

// #10444: a malformed payload from the VM must fail the run as a sandbox error instead of throwing
// in the host's message handling and never settling.
#[tokio::test]
async fn reports_a_broken_bridge_as_a_sandbox_error() {
    let cases = [
        (json!({ "type": "done", "ok": true, "value": "1", "writes": "null" }), "store writes are not an array"),
        (json!({ "type": "done", "ok": true, "value": "1", "writes": "[1]" }), "store writes contain a malformed entry"),
        (json!({ "type": "done", "ok": true, "value": "1", "writes": "[[\"k\", \"{\"]]" }), "store value for \"k\" is not valid JSON"),
        (json!({ "type": "done", "ok": true, "value": "{", "writes": "[]" }), "return value is not valid JSON"),
        (json!({ "type": "done", "ok": false, "error": "5" }), "script error is not an object"),
        (json!({ "type": "done", "ok": false, "error": "{}" }), "script error is malformed"),
        (json!({ "type": "nonsense" }), "unknown message from the worker"),
    ];
    for (message, reason) in cases {
        let sandbox = raw_sandbox();
        let result = run(&sandbox, &Value::Array(vec![message.clone()]).to_string()).await;
        let error = result.error().unwrap_or_else(|| panic!("{message}: no error"));
        assert_eq!(error.kind, CodemodeErrorKind::Sandbox);
        assert!(error.message.contains(&format!("Sandbox bridge broken: {reason}")), "{message}: {}", error.message);
    }
}
