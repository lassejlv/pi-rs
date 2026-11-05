//! Port of `test/suite/agent-session-codemode.test.ts`: tool presentation per `codemode.mode`,
//! nested calls and hooks, images, store persistence, options, and `models.*` against a fake
//! classifier/image provider registered on the session's model runtime.

mod support;

use std::sync::Arc;

use pi_agent::types::{AgentMessage, AgentToolRef, AgentToolResult, FnAgentTool};
use pi_ai::providers::faux::FauxResponseStep;
use pi_ai::types::{StopReason, ToolResultMessage, UserContentBlock};
use pi_coding_agent::core::extensions::types::{ExtensionAPIRef, ExtensionFactory};
use pi_coding_agent::core::session_manager::SessionEntry;
use pi_coding_agent::core::tools::tool_definition_wrapper::create_tool_definition_from_agent_tool;
use pi_coding_agent::extensions::codemode::create_codemode_extension;
use pi_coding_agent::extensions::codemode::tool::{CODEMODE_STORE_ENTRY_TYPE, CodemodeToolDetails};
use serde_json::{Value, json};
use support::*;

fn echo_tool() -> AgentToolRef {
    Arc::new(FnAgentTool::new(
        "echo",
        "Echo",
        "Echo text back.\n\nSecond paragraph.",
        json!({ "type": "object", "properties": { "text": { "type": "string", "description": "Text to echo" } }, "required": ["text"] }),
        |_, params: Value, _, _| async move {
            Ok(AgentToolResult::text(format!("echo: {}", params["text"].as_str().unwrap_or_default()), Some(json!({}))))
        },
    ))
}

fn stats_tool() -> AgentToolRef {
    Arc::new(
        FnAgentTool::new("stats", "Stats", "Return structured stats", json!({ "type": "object", "properties": {} }), |_, _, _, _| async {
            Ok(AgentToolResult {
                content: vec![UserContentBlock::text("2 files")],
                details: Some(json!({})),
                structured_content: Some(json!({ "files": 2, "names": ["a", "b"] })),
                ..Default::default()
            })
        })
        .with_output_schema(json!({
            "type": "object",
            "properties": { "files": { "type": "number" }, "names": { "type": "array", "items": { "type": "string" } } },
            "required": ["files", "names"]
        })),
    )
}

fn register_tools() -> TestExtension {
    let factory: ExtensionFactory = Arc::new(|pi: ExtensionAPIRef| {
        Box::pin(async move {
            for tool in [echo_tool(), stats_tool()] {
                pi.register_tool(create_tool_definition_from_agent_tool(tool))?;
            }
            Ok(())
        })
    });
    TestExtension::new(factory)
}

async fn setup(with_tools: bool) -> Harness {
    let mut extension_factories = vec![TestExtension::new(create_codemode_extension(None))];
    if with_tools {
        extension_factories.push(register_tools());
    }
    create_harness(HarnessOptions {
        initial_active_tool_names: Some(vec!["codemode".to_string()]),
        extension_factories,
        ..Default::default()
    })
    .await
    .unwrap()
}

async fn run(harness: &Harness, code: &str) -> ToolResultMessage {
    let call = faux_tool_call("codemode", json!({ "code": code }).as_object().cloned().unwrap(), None);
    harness.set_responses(vec![
        FauxResponseStep::from(faux_assistant_message(
            vec![call],
            FauxMessageOptions { stop_reason: Some(StopReason::ToolUse), ..Default::default() },
        )),
        faux_text_message("done").into(),
    ]);
    harness.session.prompt("go", None).await.unwrap();
    harness.tool_result("codemode")
}

/// The output after the script header, which is checked on the way.
fn result_text(message: &ToolResultMessage) -> String {
    let header = message.content[0].as_text().expect("header").text.clone();
    let header_re = regex::Regex::new(r"\AScript (completed|failed)\nWall time \d+\.\d seconds\nOutput:\n\z").unwrap();
    assert!(header_re.is_match(&header), "{header:?}");
    message.content[1..]
        .iter()
        .map(|block| match block.as_text() {
            Some(text) => text.text.clone(),
            None => "<image>".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn details(message: &ToolResultMessage) -> CodemodeToolDetails {
    serde_json::from_value(message.details.clone().expect("details")).unwrap()
}

fn store_entries(harness: &Harness) -> Vec<Value> {
    harness
        .session_manager
        .get_branch(None)
        .into_iter()
        .filter_map(|entry| match entry {
            SessionEntry::Custom(entry) if entry.custom_type == CODEMODE_STORE_ENTRY_TYPE => entry.data,
            _ => None,
        })
        .collect()
}

fn description(harness: &Harness, name: &str) -> String {
    harness
        .session
        .agent()
        .state()
        .tools
        .iter()
        .find(|tool| tool.name() == name)
        .map(|tool| tool.description().to_string())
        .unwrap_or_default()
}

const INCREMENT: &str = "const next = (load(\"count\") ?? 0) + 1;\nstore(\"count\", next);\nreturn next;";

#[tokio::test]
async fn presents_callable_tools_per_codemode_mode() {
    let harness = setup(true).await;
    let names = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    harness.session.set_active_tools_by_name(&names(&["read", "echo", "codemode"]));
    assert!(description(&harness, "echo").contains("Codemode: `tools.echo(args)` resolves to"));
    assert!(!description(&harness, "echo").contains("codemode tool declaration:"));
    assert!(!description(&harness, "codemode").contains("### `echo`"));

    harness.settings_manager.apply_overrides_json(json!({ "codemode": { "mode": "only" } }).as_object().unwrap());
    harness.session.set_active_tools_by_name(&names(&["read", "echo", "codemode"]));
    assert!(!description(&harness, "echo").contains("Codemode: `tools.echo"));
    assert!(description(&harness, "codemode").contains("### `echo`"));
    assert!(!description(&harness, "codemode").contains("### `stats`"));

    harness.session.set_active_tools_by_name(&names(&["echo"]));
    assert_eq!(description(&harness, "echo"), "Echo text back.\n\nSecond paragraph.");
    harness.cleanup();
}

#[tokio::test]
async fn runs_nested_calls_in_parallel_and_returns_only_the_script_result() {
    let harness = setup(true).await;
    let result = run(
        &harness,
        r#"
            const [a, b, stats] = await Promise.all([
                tools.echo({ text: "one" }),
                tools.echo({ text: "two" }),
                tools.stats({}),
            ]);
            console.log("files", stats.files);
            text(ALL_TOOLS.map((tool) => tool.name).join(","));
            return { a, b, names: stats.names };
        "#,
    )
    .await;
    assert!(!result.is_error);
    assert_eq!(
        result_text(&result),
        "==> text 1/2 <==\necho,stats\n==> text 2/2 <==\n{\"a\":\"echo: one\",\"b\":\"echo: two\",\"names\":[\"a\",\"b\"]}\n<console_output>\nfiles 2\n</console_output>"
    );
    let calls = details(&result).calls;
    assert_eq!(
        calls.iter().map(|c| (c.name.as_str(), c.status.as_str())).collect::<Vec<_>>(),
        vec![("echo", "ok"), ("echo", "ok"), ("stats", "ok")]
    );
    assert!(calls.iter().all(|call| call.id.starts_with(&format!("{}/", result.tool_call_id))));
    let tool_results = harness.session.messages().iter().filter(|m| matches!(m, AgentMessage::ToolResult(_))).count();
    assert_eq!(tool_results, 1);
    harness.cleanup();
}

#[tokio::test]
async fn reports_script_failures_with_partial_output_and_the_calls_that_ran() {
    let harness = setup(true).await;
    let result = run(
        &harness,
        "text(\"partial\");\nconsole.log(\"log\");\nawait tools.echo({ text: \"x\" });\nthrow new Error(\"boom\");",
    )
    .await;
    assert!(result.is_error);
    let text = result_text(&result);
    assert!(text.starts_with("partial\n<console_output>\nlog\n</console_output>\nScript error:\n"), "{text}");
    assert!(text.contains("boom"));
    assert!(text.contains("Tool calls made before the failure (they are not undone): echo (ok)"));
    assert_eq!(details(&result).calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>(), vec!["echo"]);
    harness.cleanup();
}

#[tokio::test]
async fn applies_the_timeout_option_and_rejects_invalid_options() {
    let harness = setup(false).await;
    let timed_out = run(&harness, "// @options: {\"timeout_ms\": 200}\nwhile (true) {}").await;
    assert!(timed_out.is_error);
    assert!(result_text(&timed_out).contains("Script error:\nScript timed out"));

    let invalid = run(&harness, "// @options: {\"yield\": 1}\ntext(1)").await;
    assert!(invalid.is_error);
    assert_eq!(invalid.content.len(), 1);
    assert_eq!(
        invalid.content[0].as_text().unwrap().text,
        "@options only supports `max_output_tokens` and `timeout_ms`; got `yield`"
    );

    let misplaced = run(&harness, "// count\n// @options: {\"timeout_ms\": 200}\nwhile (true) {}").await;
    assert!(misplaced.is_error);
    assert_eq!(
        misplaced.content[0].as_text().unwrap().text,
        "The // @options: line must be the first line of the input; found one on line 2"
    );
    harness.cleanup();
}

#[tokio::test]
async fn persists_store_writes_as_custom_entries_for_later_calls() {
    let harness = setup(false).await;
    assert_eq!(result_text(&run(&harness, INCREMENT).await), "1");
    assert_eq!(result_text(&run(&harness, INCREMENT).await), "2");
    assert_eq!(
        store_entries(&harness),
        vec![json!({ "set": { "count": 1 }, "delete": [] }), json!({ "set": { "count": 2 }, "delete": [] })]
    );
    let deleted = run(&harness, "store(\"count\", undefined);\nreturn load(\"count\") === undefined;").await;
    assert_eq!(result_text(&deleted), "true");
    assert_eq!(store_entries(&harness).last().cloned(), Some(json!({ "set": {}, "delete": ["count"] })));
    harness.cleanup();
}

#[tokio::test]
async fn appends_nothing_for_failed_scripts_or_scripts_without_writes() {
    let harness = setup(false).await;
    assert!(run(&harness, "store(\"count\", 5);\nthrow new Error(\"boom\");").await.is_error);
    assert!(!run(&harness, "return load(\"count\") ?? \"missing\";").await.is_error);
    assert!(store_entries(&harness).is_empty());
    harness.cleanup();
}

#[tokio::test]
async fn loads_the_values_written_on_the_current_branch() {
    let harness = setup(false).await;
    run(&harness, INCREMENT).await;
    let first_prompt = harness
        .session_manager
        .get_branch(None)
        .into_iter()
        .find_map(|entry| match entry {
            SessionEntry::Message(entry) => Some(entry.id),
            _ => None,
        })
        .expect("first prompt");
    assert_eq!(result_text(&run(&harness, INCREMENT).await), "2");
    harness.session_manager.branch(&first_prompt).unwrap();
    assert_eq!(result_text(&run(&harness, INCREMENT).await), "1");
    harness.cleanup();
}

#[tokio::test]
async fn truncates_output_to_the_token_budget_and_spills_the_full_text() {
    let harness = setup(false).await;
    let result = run(&harness, "// @options: {\"max_output_tokens\": 30}\nfor (let i = 0; i < 100; i++) text(\"row \" + i);").await;
    let path = details(&result).full_output_path.expect("spill file");
    let text = result_text(&result);
    assert!(text.starts_with("Warning: truncated output"));
    assert!(text.contains("row 0\n"));
    assert!(text.contains("tokens truncated"));
    assert!(text.contains("row 99"));
    assert!(!text.contains("row 50\n"));
    assert!(text.contains(&format!("[Full output: {path} (read with offset/limit)]")));
    let expected = (0..100).map(|i| format!("==> text {}/100 <==\nrow {i}", i + 1)).collect::<Vec<_>>().join("\n");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    let _ = std::fs::remove_file(&path);

    let small = run(&harness, "return { ok: true };").await;
    assert!(details(&small).full_output_path.is_none());
    harness.cleanup();
}

// ---------------------------------------------------------------------------
// Cases added by the test audit.
// ---------------------------------------------------------------------------

use parking_lot::Mutex;
use pi_ai::providers::faux::FauxResponseStep as Step;
use pi_ai::types::{Usage, UsageCost};
use pi_coding_agent::core::extensions::types::{
    ToolCallEvent, ToolCallEventResult, ToolResultEvent, ToolResultEventResult, on_event,
};

const TINY_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";

fn usage(input: u64, cost: f64) -> Usage {
    Usage {
        input,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        total_tokens: input,
        cost: UsageCost { input: cost, output: 0.0, cache_read: 0.0, cache_write: 0.0, total: cost },
        ..Default::default()
    }
}

fn screenshot_tool() -> AgentToolRef {
    Arc::new(FnAgentTool::new("screenshot", "Screenshot", "Return a screenshot", json!({ "type": "object", "properties": {} }), |_, _, _, _| async {
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text("captured"), serde_json::from_value(json!({ "type": "image", "data": TINY_PNG_BASE64, "mimeType": "image/png" })).unwrap()],
            details: Some(json!({})),
            ..Default::default()
        })
    }))
}

fn tool_extension(tools: Vec<AgentToolRef>) -> TestExtension {
    let factory: ExtensionFactory = Arc::new(move |pi: ExtensionAPIRef| {
        let tools = tools.clone();
        Box::pin(async move {
            for tool in tools {
                pi.register_tool(create_tool_definition_from_agent_tool(tool))?;
            }
            Ok(())
        })
    });
    TestExtension::new(factory)
}

fn extension<F, Fut>(f: F) -> TestExtension
where
    F: Fn(ExtensionAPIRef) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let factory: ExtensionFactory = Arc::new(move |pi| Box::pin(f(pi)));
    TestExtension::new(factory)
}

/// The suite's `setup(extensionFactories)`: codemode, the echo/stats/screenshot tools, and more extensions.
async fn setup_with(extensions: Vec<TestExtension>) -> Harness {
    let mut extension_factories = vec![
        TestExtension::new(create_codemode_extension(None)),
        tool_extension(vec![echo_tool(), stats_tool(), screenshot_tool()]),
    ];
    extension_factories.extend(extensions);
    create_harness(HarnessOptions {
        initial_active_tool_names: Some(vec!["codemode".to_string()]),
        extension_factories,
        ..Default::default()
    })
    .await
    .unwrap()
}

/// Replace `[Image saved to ...]` labels with `<saved>` after checking each file holds the tiny PNG.
fn check_saved_images(text: &str) -> String {
    let label = regex::Regex::new(r"^\[Image saved to (\S+\.png) \(image/png, \d+B\)\]$").unwrap();
    text.split('\n')
        .map(|line| match label.captures(line) {
            Some(captures) => {
                let path = captures[1].to_string();
                let bytes = std::fs::read(&path).unwrap();
                let _ = std::fs::remove_file(&path);
                use base64::Engine;
                assert_eq!(base64::engine::general_purpose::STANDARD.encode(bytes), TINY_PNG_BASE64);
                "<saved>".to_string()
            }
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_tiny_png(block: &UserContentBlock) -> bool {
    serde_json::to_value(block).unwrap() == json!({ "type": "image", "data": TINY_PNG_BASE64, "mimeType": "image/png" })
}

#[tokio::test]
async fn presents_callable_tools_per_codemode_mode_in_requests_and_prompts() {
    let harness = setup(true).await;
    let names = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    let tools: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    let record = || {
        let (tools, prompts) = (tools.clone(), prompts.clone());
        Step::factory(move |context, _, _, _| {
            tools.lock().push(pi_ai::get_current_tools(&context.messages).into_iter().map(|t| t.name).collect());
            prompts.lock().push(pi_ai::get_current_system_prompt(&context.messages));
            faux_text_message("ok")
        })
    };

    harness.session.set_active_tools_by_name(&names(&["read", "echo", "codemode"]));
    harness.set_responses(vec![record()]);
    harness.session.prompt("on", None).await.unwrap();
    for name in ["read", "echo", "codemode"] {
        assert!(tools.lock()[0].contains(&name.to_string()), "{:?}", tools.lock()[0]);
    }
    assert!(prompts.lock()[0].contains("\n- read: "));

    harness.settings_manager.apply_overrides_json(json!({ "codemode": { "mode": "only" } }).as_object().unwrap());
    harness.session.set_active_tools_by_name(&names(&["read", "echo", "codemode"]));
    harness.set_responses(vec![record()]);
    harness.session.prompt("only", None).await.unwrap();
    let request_tools = tools.lock()[1].clone();
    assert!(request_tools.contains(&"codemode".to_string()));
    assert!(!request_tools.contains(&"echo".to_string()));
    assert!(!request_tools.contains(&"read".to_string()));
    // The prompt's tool list matches the declarations: hidden tools are not listed (#10192).
    let prompt = prompts.lock()[1].clone();
    assert!(!prompt.contains("\n- read: "));
    assert!(prompt.contains("\n- codemode: "));
    assert!(!harness.session.system_prompt().contains("\n- read: "));
    // Hidden tools' guidelines move from the rules to their codemode sections (#10343).
    assert!(!prompt.contains("Use read to examine files"));
    assert!(description(&harness, "codemode").contains("- Use read to examine files instead of cat or sed."));
    harness.cleanup();
}

// #10343
#[tokio::test]
async fn shows_the_guidelines_of_tools_that_do_not_fit_the_inline_budget_through_describe_tool() {
    let harness = setup_with(vec![]).await;
    harness
        .settings_manager
        .apply_overrides_json(json!({ "codemode": { "mode": "only", "inlineBudget": 0 } }).as_object().unwrap());
    harness.session.set_active_tools_by_name(&["read".to_string(), "codemode".to_string()]);
    assert!(!description(&harness, "codemode").contains("### `read`"));
    let result = run(&harness, "text(await describeTool(\"read\"))").await;
    assert!(result_text(&result).contains("- Use read to examine files instead of cat or sed."));
    harness.cleanup();
}

#[tokio::test]
async fn routes_nested_calls_through_extension_hooks() {
    let hooks = extension(|pi| async move {
        on_event::<ToolCallEvent, _, _>(&*pi, |event, _ctx| async move {
            if event.tool_name == "echo" && event.input.get("text").and_then(Value::as_str) == Some("forbidden") {
                return Ok(Some(ToolCallEventResult {
                    block: Some(true),
                    reason: Some("echo of forbidden text is blocked".into()),
                    ..Default::default()
                }));
            }
            Ok(None)
        })?;
        on_event::<ToolResultEvent, _, _>(&*pi, |event, _ctx| async move {
            if event.tool_name == "stats" {
                return Ok(Some(ToolResultEventResult {
                    content: Some(vec![UserContentBlock::text("redacted")]),
                    ..Default::default()
                }));
            }
            Ok(None)
        })?;
        Ok(())
    });
    let harness = setup_with(vec![hooks]).await;
    let result = run(
        &harness,
        r#"
            let blocked;
            try {
                await tools.echo({ text: "forbidden" });
            } catch (error) {
                blocked = error.message;
            }
            const stats = await tools.stats({});
            return { blocked, stats };
        "#,
    )
    .await;
    // Replacing content without replacing structured content drops the structured result.
    assert_eq!(
        serde_json::from_str::<Value>(&result_text(&result)).unwrap(),
        json!({ "blocked": "echo of forbidden text is blocked", "stats": "redacted" })
    );
    assert_eq!(details(&result).calls.iter().map(|c| c.status.as_str()).collect::<Vec<_>>(), vec!["error", "ok"]);
    harness.cleanup();
}

#[tokio::test]
async fn adds_the_usage_of_nested_results_to_the_codemode_result() {
    let billed: AgentToolRef =
        Arc::new(FnAgentTool::new("billed", "Billed", "Run a model", json!({ "type": "object", "properties": {} }), |_, _, _, _| async {
            Ok(AgentToolResult {
                content: vec![UserContentBlock::text("ran")],
                details: Some(json!({})),
                usage: Some(usage(100, 0.25)),
                ..Default::default()
            })
        }));
    let harness = setup_with(vec![tool_extension(vec![billed])]).await;
    let result = run(&harness, "await tools.billed({}); await tools.billed({}); await tools.echo({ text: \"x\" });").await;
    let total = result.usage.clone().expect("codemode usage");
    assert_eq!(total.input, 200);
    assert_eq!(total.total_tokens, 200);
    assert_eq!(total.cost.total, 0.5);
    // The usage is persisted with the result, so session totals count it.
    let persisted = harness.session_manager.get_entries().into_iter().find_map(|entry| match entry {
        SessionEntry::Message(entry) => match entry.message {
            AgentMessage::ToolResult(message) => Some(message),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(persisted.and_then(|m| m.usage), result.usage);
    assert_eq!(harness.session.get_session_stats().cost, 0.5);
    harness.cleanup();
}

#[tokio::test]
async fn keeps_structured_content_that_tool_result_handlers_replace_along_with_the_content() {
    let hooks = extension(|pi| async move {
        on_event::<ToolResultEvent, _, _>(&*pi, |event, _ctx| async move {
            Ok((event.tool_name == "stats").then(|| ToolResultEventResult {
                content: Some(vec![UserContentBlock::text("0 files")]),
                structured_content: Some(json!({ "files": 0, "names": [] })),
                ..Default::default()
            }))
        })?;
        // A later handler that only touches details keeps what the first one set.
        on_event::<ToolResultEvent, _, _>(&*pi, |event, _ctx| async move {
            Ok((event.tool_name == "stats")
                .then(|| ToolResultEventResult { details: Some(json!({ "audited": true })), ..Default::default() }))
        })?;
        Ok(())
    });
    let harness = setup_with(vec![hooks]).await;
    let result = run(&harness, "return await tools.stats({});").await;
    assert_eq!(serde_json::from_str::<Value>(&result_text(&result)).unwrap(), json!({ "files": 0, "names": [] }));
    harness.cleanup();
}

// Saved images: https://github.com/earendil-works/pi/issues/10310
#[tokio::test]
async fn attaches_only_the_images_the_script_passes_to_image_in_output_order_each_after_its_saved_path() {
    let harness = setup_with(vec![]).await;
    let code = format!(
        r#"
            // Tools without an outputSchema resolve to their text; images are not passed on.
            const shot = await tools.screenshot({{}});
            text(shot);
            image("data:image/png;base64,{TINY_PNG_BASE64}");
            image("data:image/png;base64,{TINY_PNG_BASE64}");
            text("after");
        "#
    );
    let result = run(&harness, &code).await;
    // The same image shown twice is saved once, so both labels name one file.
    let text = result_text(&result);
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(
        lines,
        vec!["==> text 1/2 <==", "captured", lines[2], "<image>", lines[2], "<image>", "==> text 2/2 <==", "after"]
    );
    assert_eq!(check_saved_images(lines[2]), "<saved>");
    assert!(is_tiny_png(&result.content[2]));
    harness.cleanup();
}

#[tokio::test]
async fn marks_where_each_text_item_starts_and_puts_console_lines_last_in_one_text_block() {
    let harness = setup_with(vec![]).await;
    let result = run(&harness, "text(\"one\\ntwo\");\nconsole.log(\"a\");\nconsole.log(\"b\");\ntext(\"three\\n\");\nreturn 4;").await;
    // Providers join adjacent text blocks with nothing or a newline, so the output is one block.
    assert_eq!(result.content.len(), 2);
    assert_eq!(
        result_text(&result),
        "==> text 1/3 <==\none\ntwo\n==> text 2/3 <==\nthree\n==> text 3/3 <==\n4\n<console_output>\na\nb\n</console_output>"
    );
    harness.cleanup();
}

#[tokio::test]
async fn reports_script_failures_with_the_script_location() {
    let harness = setup(true).await;
    let result = run(
        &harness,
        "text(\"partial\");\nconsole.log(\"log\");\nawait tools.echo({ text: \"x\" });\nthrow new Error(\"boom\");",
    )
    .await;
    assert!(result.content[0].as_text().unwrap().text.starts_with("Script failed\n"));
    let text = result_text(&result);
    assert!(text.starts_with("partial\n<console_output>\nlog\n</console_output>\nScript error:\nError: boom\n"), "{text}");
    assert!(text.contains("codemode.js:4"), "{text}");
    harness.cleanup();
}

// The VM's heap limit (`CodemodeExecutionOptions::memory_limit_bytes` in pi-codemode) ends runaway
// allocations with an error the script can catch.
#[tokio::test]
async fn limits_script_memory_so_runaway_allocations_fail_inside_the_script() {
    let harness = setup(false).await;
    let result = run(
        &harness,
        "// @options: {\"timeout_ms\": 30000}\nlet a = [];\ntry { while (true) a.push(\"x\".repeat(1 << 20) + a.length); } catch (error) { const n = a.length; a = null; return { n, error: String(error) }; }",
    )
    .await;
    assert!(!result.is_error, "{}", result_text(&result));
    let value: Value = serde_json::from_str(&result_text(&result)).unwrap();
    assert!(value["error"].as_str().unwrap_or_default().contains("out of memory"), "{value}");
    // Each entry holds at least 1 MiB, so the limit stops the script well before 4 GiB.
    assert!(value["n"].as_u64().unwrap() < 512, "{value}");
    harness.cleanup();
}

#[tokio::test]
async fn truncated_output_keeps_images_after_their_saved_paths() {
    let harness = setup(false).await;
    let result = run(
        &harness,
        &format!(
            "// @options: {{\"max_output_tokens\": 30}}\nfor (let i = 0; i < 100; i++) text(\"row \" + i);\nimage(\"data:image/png;base64,{TINY_PNG_BASE64}\");"
        ),
    )
    .await;
    let path = details(&result).full_output_path.expect("spill file");
    let text = result_text(&result);
    // Images follow the truncated text, each after the path it was saved to.
    assert!(is_tiny_png(result.content.last().unwrap()));
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(check_saved_images(lines[lines.len() - 2]), "<saved>");
    let _ = std::fs::remove_file(&path);
    harness.cleanup();
}

// #10678: joining the text items was quadratic.
#[tokio::test]
async fn formats_output_at_the_item_limit_in_linear_time() {
    let harness = setup(false).await;
    let started = std::time::Instant::now();
    let result = run(&harness, "for (let i = 0; i < 100000; i++) text(\"\");").await;
    let elapsed = started.elapsed();
    if let Some(path) = details(&result).full_output_path {
        let _ = std::fs::remove_file(path);
    }
    assert!(!result.is_error);
    assert!(result_text(&result).contains("==> text 100000/100000 <=="));
    // TS allows 5 s; debug builds of the embedded engine are slower, so allow more.
    assert!(elapsed < std::time::Duration::from_secs(30), "{elapsed:?}");
    harness.cleanup();
}

#[tokio::test]
async fn resolves_bash_calls_to_structured_results_also_for_non_zero_exit_codes() {
    let harness = create_harness(HarnessOptions {
        initial_active_tool_names: Some(vec!["codemode".into(), "bash".into()]),
        extension_factories: vec![TestExtension::new(create_codemode_extension(None))],
        ..Default::default()
    })
    .await
    .unwrap();
    let result = run(
        &harness,
        "const r = await tools.bash({ command: \"echo out; exit 3\" });\ntext(JSON.stringify([r.output, r.exit_code, typeof r.wall_time_seconds]));",
    )
    .await;
    assert!(!result.is_error);
    assert_eq!(result_text(&result), "[\"out\\n\",3,\"number\"]");
    harness.cleanup();
}

// https://github.com/earendil-works/pi/issues/10251
#[tokio::test]
async fn resolves_read_calls_to_text_for_text_files_and_to_image_blocks_that_image_shows() {
    let harness = create_harness(HarnessOptions {
        initial_active_tool_names: Some(vec!["codemode".into(), "read".into()]),
        extension_factories: vec![TestExtension::new(create_codemode_extension(None))],
        ..Default::default()
    })
    .await
    .unwrap();
    std::fs::write(harness.temp_dir.join("notes.txt"), "hello").unwrap();
    use base64::Engine;
    std::fs::write(harness.temp_dir.join("pixel.png"), base64::engine::general_purpose::STANDARD.decode(TINY_PNG_BASE64).unwrap())
        .unwrap();
    let result = run(
        &harness,
        "text(await tools.read({ path: \"notes.txt\" }));\nconst shot = await tools.read({ path: \"pixel.png\" });\ntext(shot.note);\nimage(shot);",
    )
    .await;
    assert!(!result.is_error, "{}", result_text(&result));
    assert_eq!(
        check_saved_images(&result_text(&result)),
        "==> text 1/2 <==\nhello\n==> text 2/2 <==\nRead image file [image/png]\n<saved>\n<image>"
    );
    assert!(is_tiny_png(result.content.last().unwrap()));
    harness.cleanup();
}

#[tokio::test]
async fn store_writes_are_announced_as_entry_appended_events() {
    let harness = setup(false).await;
    run(&harness, INCREMENT).await;
    run(&harness, INCREMENT).await;
    let appended = harness
        .events_of_type("entry_appended")
        .into_iter()
        .filter(|event| event["entry"]["type"] == "custom" && event["entry"]["customType"] == CODEMODE_STORE_ENTRY_TYPE)
        .count();
    assert_eq!(appended, 2);
    harness.cleanup();
}

// ---------------------------------------------------------------------------
// codemode models
// ---------------------------------------------------------------------------

use pi_ai::types::{
    AssistantImages, ClassifierContext, ClassifierModel, ClassifierResult, ImageModel, ImagesContext,
};
use pi_ai::{ProviderClassifier, ProviderImages, StreamOptions};
use pi_coding_agent::core::extensions::types::{ProviderConfig, ProviderModelConfig};
use pi_coding_agent::extensions::codemode::tool::{CODEMODE_DOCS_PATH, create_codemode_tool};

#[derive(Default)]
struct ModelObservations {
    classify: Mutex<Vec<(String, Option<String>, Value)>>,
    images: Mutex<Vec<(String, Option<String>, Vec<UserContentBlock>)>>,
    active: std::sync::atomic::AtomicUsize,
    max_active: std::sync::atomic::AtomicUsize,
}

struct ScorerImages(Arc<ModelObservations>);

#[async_trait::async_trait]
impl ProviderImages for ScorerImages {
    async fn generate_images(&self, model: &ImageModel, context: &ImagesContext, options: StreamOptions) -> AssistantImages {
        self.0.images.lock().push((model.base_url.clone(), options.api_key.clone(), context.input.clone()));
        let prompt = context.input.iter().find_map(|b| b.as_text().map(|t| t.text.clone())).unwrap_or_default();
        let base = json!({ "api": model.api, "provider": model.provider, "model": model.id, "timestamp": 0 });
        let mut value = base.clone();
        if prompt == "explode" {
            value["output"] = json!([]);
            value["stopReason"] = json!("error");
            value["errorMessage"] = json!("painter exploded");
        } else {
            value["output"] = json!([
                { "type": "text", "text": format!("painted {prompt}") },
                { "type": "image", "data": TINY_PNG_BASE64, "mimeType": "image/png" }
            ]);
            value["usage"] = serde_json::to_value(usage(100, 0.04)).unwrap();
            value["stopReason"] = json!("stop");
        }
        serde_json::from_value(value).unwrap()
    }
}

struct ScorerClassifier(Arc<ModelObservations>);

#[async_trait::async_trait]
impl ProviderClassifier for ScorerClassifier {
    async fn classify(&self, model: &ClassifierModel, context: &ClassifierContext, options: StreamOptions) -> ClassifierResult {
        use std::sync::atomic::Ordering;
        let active = self.0.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.0.max_active.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        let text = context.state.get("text").cloned().unwrap_or(Value::Null);
        self.0.classify.lock().push((model.base_url.clone(), options.api_key.clone(), text.clone()));
        let mut value = json!({ "api": model.api, "provider": model.provider, "model": model.id, "timestamp": 0 });
        if text == "explode" {
            value["answers"] = json!({});
            value["stopReason"] = json!("error");
            value["errorMessage"] = json!("classifier exploded");
        } else {
            let probability = if text == "good" { 0.9 } else { 0.1 };
            value["answers"] = json!({ "approved": { "type": "bool", "probability": probability } });
            value["usage"] = serde_json::to_value(usage(300, 0.001)).unwrap();
            value["stopReason"] = json!("stop");
        }
        serde_json::from_value(value).unwrap()
    }
}

fn scorer_models() -> Vec<ProviderModelConfig> {
    vec![
        serde_json::from_value(json!({
            "type": "classifier", "id": "judge", "name": "Judge", "api": "test-classifier", "provider": "scorer",
            "baseUrl": "https://classifier.test/v1", "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }, "contextWindow": 1000,
            "headers": { "X-Secret": "hunter2" }
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "type": "image", "id": "painter", "name": "Painter", "api": "test-images", "provider": "scorer",
            "baseUrl": "https://images.test/v1", "input": ["text", "image"], "output": ["text", "image"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }
        }))
        .unwrap(),
    ]
}

async fn setup_models() -> (Harness, Arc<ModelObservations>) {
    let harness = setup(false).await;
    let observations = Arc::new(ModelObservations::default());
    harness
        .session
        .model_runtime()
        .register_provider(
            "scorer",
            &ProviderConfig {
                api_key: Some("secret-key".into()),
                models: Some(scorer_models()),
                images: Some(
                    [("test-images".to_string(), Arc::new(ScorerImages(observations.clone())) as Arc<dyn ProviderImages>)]
                        .into_iter()
                        .collect(),
                ),
                classifiers: Some(
                    [(
                        "test-classifier".to_string(),
                        Arc::new(ScorerClassifier(observations.clone())) as Arc<dyn ProviderClassifier>,
                    )]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            },
        )
        .unwrap();
    harness.session.set_active_tools_by_name(&["codemode".to_string()]);
    (harness, observations)
}

const QUESTIONS: &str = r#"{ approved: { type: "bool", instructions: "Approval?", criteria: { true: "yes", false: "no" } } }"#;

fn call_summary(message: &ToolResultMessage) -> Vec<Value> {
    details(message)
        .calls
        .iter()
        .map(|call| serde_json::to_value(call).unwrap())
        .map(|v| json!([v["name"], v["args"], v["status"], v.get("cost").cloned().unwrap_or(Value::Null), v.get("error").cloned().unwrap_or(Value::Null)]))
        .collect()
}

#[tokio::test]
async fn declares_models_only_for_the_sessions_own_codemode_tool() {
    let (harness, _) = setup_models().await;
    let codemode = description(&harness, "codemode");
    // The description names the models globals and points to the docs for the API.
    assert!(codemode.contains("`models`: classifiers and image generation"));
    assert!(codemode.contains(CODEMODE_DOCS_PATH.as_str()));

    let overridden = create_harness(HarnessOptions { tools: Some(vec![create_codemode_tool(None, None)]), ..Default::default() })
        .await
        .unwrap();
    assert!(!description(&overridden, "codemode").contains("`models`"));
    overridden.cleanup();
    harness.cleanup();
}

#[tokio::test]
async fn lists_models_and_classifies_with_catalog_auth_ignoring_script_supplied_fields() {
    let (harness, observations) = setup_models().await;
    let result = run(
        &harness,
        &format!(
            r#"
            const [model] = await models.getAvailableOfType("classifier", "scorer");
            const listed = await models.getModelsOfType("classifier");
            const same = await models.getModelOfType("classifier", "scorer", "judge");
            const texts = ["good", "bad", "good", "bad", "good", "bad"];
            const results = await Promise.all(
                texts.map((text) => models.classify({{ ...model, baseUrl: "https://evil.test" }}, {{ state: {{ text }}, questions: {QUESTIONS} }})),
            );
            return {{
                id: model.id,
                headers: "headers" in model,
                listed: listed.some((entry) => entry.provider === "scorer" && entry.id === "judge"),
                same: same.id,
                missing: (await models.getModelOfType("classifier", "scorer", "nope")) === undefined,
                probabilities: results.map((r) => r.answers.approved.probability),
                cost: results[0].usage.cost.total,
            }};
        "#
        ),
    )
    .await;
    assert!(!result.is_error, "{}", result_text(&result));
    assert_eq!(
        serde_json::from_str::<Value>(&result_text(&result)).unwrap(),
        json!({
            "id": "judge", "headers": false, "listed": true, "same": "judge", "missing": true,
            "probabilities": [0.9, 0.1, 0.9, 0.1, 0.9, 0.1], "cost": 0.001
        })
    );
    let observed = observations.classify.lock().clone();
    assert_eq!(observed.len(), 6);
    assert!(observed.iter().all(|(base, key, _)| base == "https://classifier.test/v1" && key.as_deref() == Some("secret-key")));
    // Six classifications with at most four in flight.
    assert_eq!(observations.max_active.load(std::sync::atomic::Ordering::SeqCst), 4);
    assert_eq!(call_summary(&result), vec![json!(["models.classify", "scorer/judge", "ok", 0.001, null]); 6]);
    // The classifications' usage becomes the codemode result's usage.
    let total = result.usage.clone().unwrap();
    assert_eq!(total.input, 1800);
    assert!((total.cost.total - 0.006).abs() < 1e-10);
    assert!((harness.session.get_session_stats().cost - 0.006).abs() < 1e-10);
    harness.cleanup();
}

#[tokio::test]
async fn generates_images_with_catalog_auth_and_attaches_them_through_image() {
    let (harness, observations) = setup_models().await;
    let result = run(
        &harness,
        &format!(
            r#"
            const [model] = await models.getAvailableOfType("image", "scorer");
            const reference = {{ type: "image", data: "{TINY_PNG_BASE64}", mimeType: "image/png" }};
            const generated = await models.generateImages(
                {{ ...model, baseUrl: "https://evil.test" }},
                {{ input: [{{ type: "text", text: "a fox" }}, reference] }},
            );
            for (const block of generated.output) {{
                if (block.type === "image") image(block);
                else text(block.text);
            }}
            const failed = await models.generateImages(model, {{ input: [{{ type: "text", text: "explode" }}] }});
            const attempt = async (fn) => {{ try {{ await fn(); return "ok"; }} catch (error) {{ return error.message; }} }};
            return {{
                id: model.id,
                stopReason: generated.stopReason,
                failed: [failed.stopReason, failed.errorMessage],
                wrongType: await attempt(() => models.generateImages({{ provider: "scorer", id: "judge" }}, {{ input: [] }})),
            }};
        "#
        ),
    )
    .await;
    assert!(!result.is_error, "{}", result_text(&result));
    let text = check_saved_images(&result_text(&result));
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(&lines[..5], &["==> text 1/2 <==", "painted a fox", "<saved>", "<image>", "==> text 2/2 <=="]);
    assert_eq!(
        serde_json::from_str::<Value>(&lines[5..].join("\n")).unwrap(),
        json!({
            "id": "painter",
            "stopReason": "stop",
            "failed": ["error", "painter exploded"],
            "wrongType": "\"scorer/judge\" is a classifier model, not an image model. List the image models you can use with models.getAvailableOfType(\"image\")."
        })
    );
    assert!(is_tiny_png(&result.content[2]));
    let requests = observations.images.lock().clone();
    assert_eq!(
        requests.iter().map(|(base, key, _)| (base.clone(), key.clone())).collect::<Vec<_>>(),
        vec![
            ("https://images.test/v1".to_string(), Some("secret-key".to_string())),
            ("https://images.test/v1".to_string(), Some("secret-key".to_string())),
        ]
    );
    assert_eq!(
        serde_json::to_value(&requests[0].2).unwrap(),
        json!([{ "type": "text", "text": "a fox" }, { "type": "image", "data": TINY_PNG_BASE64, "mimeType": "image/png" }])
    );
    assert_eq!(
        call_summary(&result),
        vec![
            json!(["models.generateImages", "scorer/painter", "ok", 0.04, null]),
            json!(["models.generateImages", "scorer/painter", "error", null, "painter exploded"]),
        ]
    );
    assert!((result.usage.clone().unwrap().cost.total - 0.04).abs() < 1e-10);
    assert!((harness.session.get_session_stats().cost - 0.04).abs() < 1e-10);
    harness.cleanup();
}

#[tokio::test]
async fn notes_generated_images_that_the_script_did_not_show() {
    let (harness, _) = setup_models().await;
    let result = run(
        &harness,
        r#"
            const [model] = await models.getAvailableOfType("image", "scorer");
            const generated = await models.generateImages(model, { input: [{ type: "text", text: "a fox" }] });
            return generated.stopReason;
        "#,
    )
    .await;
    assert!(!result.is_error);
    assert_eq!(
        result_text(&result),
        "stop\nNote: models.generateImages() returned 1 image that the script did not show. Show each image block of result.output with image(block)."
    );
    harness.cleanup();
}

#[tokio::test]
async fn reports_provider_errors_as_results_and_invalid_arguments_as_exceptions() {
    let (harness, _) = setup_models().await;
    let q = QUESTIONS;
    let code = format!(
        r#"
            const model = await models.getModelOfType("classifier", "scorer", "judge");
            const failed = await models.classify(model, {{ state: {{ text: "explode" }}, questions: {q} }});
            const textOnly = await models.classify(model, {{
                state: {{ text: "good" }},
                images: [{{ type: "image", data: "aW1hZ2U=", mimeType: "image/png" }}],
                questions: {q},
            }});
            const attempt = async (fn) => {{ try {{ await fn(); return "ok"; }} catch (error) {{ return error.message; }} }};
            return {{
                failed: [failed.stopReason, failed.errorMessage],
                textOnly: [textOnly.stopReason, textOnly.errorMessage],
                badClassifierImage: await attempt(() =>
                    models.classify(model, {{ state: {{}}, images: [{{ data: "aW1hZ2U=" }}], questions: {q} }}),
                ),
                badType: await attempt(() => models.getModelsOfType("video")),
                unknown: await attempt(() => models.classify({{ provider: "scorer", id: "nope" }}, {{}})),
                noModel: await attempt(() => models.classify("judge", {{}})),
                undefinedModel: await attempt(() => models.classify(undefined, {{}})),
                noState: await attempt(() => models.classify(model, {{ questions: {q} }})),
                badQuestion: await attempt(() =>
                    models.classify(model, {{ state: {{}}, questions: {{ kind: {{ type: "choice", instructions: "Kind?", criteria: ["a", "b"] }} }} }}),
                ),
                badImage: await attempt(() => models.generateImages({{ provider: "scorer", id: "painter" }}, {{ prompt: "a fox" }})),
                arrayModel: await attempt(() => models.classify(["scorer", "judge"], {{}})),
                arrayContext: await attempt(() => models.classify(model, [])),
                arrayState: await attempt(() => models.classify(model, {{ state: [], questions: {q} }})),
                arrayQuestions: await attempt(() => models.classify(model, {{ state: {{}}, questions: [] }})),
                arrayQuestion: await attempt(() => models.classify(model, {{ state: {{}}, questions: {{ kind: [] }} }})),
                noInstructions: await attempt(() =>
                    models.classify(model, {{ state: {{}}, questions: {{ kind: {{ type: "bool", criteria: {{ true: "y", false: "n" }} }} }} }}),
                ),
                badBool: await attempt(() =>
                    models.classify(model, {{ state: {{}}, questions: {{ kind: {{ type: "bool", instructions: "Ok?", criteria: ["y", "n"] }} }} }}),
                ),
                badKind: await attempt(() =>
                    models.classify(model, {{ state: {{}}, questions: {{ kind: {{ type: "rank", instructions: "Rank?" }} }} }}),
                ),
                arrayImagesContext: await attempt(() => models.generateImages({{ provider: "scorer", id: "painter" }}, [])),
                badImageBlock: await attempt(() =>
                    models.generateImages({{ provider: "scorer", id: "painter" }}, {{ input: [{{ type: "text", text: "a fox" }}, {{ type: "image", data: "aW1hZ2U=" }}] }}),
                ),
                badSplit: await attempt(() => models.getModelOfType("classifier", "scorer/judge")),
            }};
        "#
    );
    let result = run(&harness, &code).await;
    assert!(!result.is_error, "{}", result_text(&result));
    let value: Value = serde_json::from_str(&result_text(&result)).unwrap();
    let has = |key: &str, expected: &str| {
        let actual = value[key].as_str().unwrap_or_default();
        assert!(actual.contains(expected), "{key}: {actual:?} does not contain {expected:?}");
    };
    assert_eq!(value["failed"], json!(["error", "classifier exploded"]));
    assert_eq!(value["textOnly"], json!(["error", "Model scorer/judge does not accept image input"]));
    has("badClassifierImage", "models.classify() context.images[0] must be an image block, got { data }.");
    has("badType", "Unknown model type \"video\"");
    assert_eq!(
        value["unknown"],
        "Unknown classifier model \"scorer/nope\". List the classifier models you can use with models.getAvailableOfType(\"classifier\")."
    );
    has("noModel", "models.classify() expects a classifier model as its first argument, got a string.");
    has("undefinedModel", "models.getModelOfType() returns undefined for an unknown provider or id.");
    has("noState", "models.classify() context.state must be an object, got undefined.");
    has("noState", "codemode.md");
    has("badQuestion", "context.questions.kind is a \"choice\" question, so criteria must map each label to its meaning.");
    has("badImage", "models.generateImages() context.input must be a non-empty array of blocks, got undefined.");
    has("badSplit", "The provider and the id are separate arguments");
    has("arrayModel", "models.classify() expects a classifier model as its first argument, got an array.");
    has("arrayContext", "models.classify() expects a context object as its second argument, got an empty array.");
    has("arrayState", "models.classify() context.state must be an object, got an empty array.");
    has("arrayQuestions", "models.classify() context.questions must map question IDs to questions, got an empty array.");
    has("arrayQuestion", "models.classify() context.questions.kind must be a question object, got an empty array.");
    has("noInstructions", "models.classify() context.questions.kind.instructions must be a string.");
    has("badBool", "context.questions.kind is a \"bool\" question, so criteria must be { true: string, false: string }.");
    has("badKind", "context.questions.kind.type must be \"choice\", \"score\", or \"bool\", got \"rank\".");
    has("arrayImagesContext", "models.generateImages() expects a context object as its second argument, got an empty array.");
    has("badImageBlock", "models.generateImages() context.input[1] must be a text or image block, got { type, data }.");
    assert_eq!(
        call_summary(&result).iter().map(|v| json!([v[0], v[2], v[4]])).collect::<Vec<_>>(),
        vec![
            json!(["models.classify", "error", "classifier exploded"]),
            json!(["models.classify", "error", "Model scorer/judge does not accept image input"]),
        ]
    );
    assert!(result.usage.is_none());
    harness.cleanup();
}
