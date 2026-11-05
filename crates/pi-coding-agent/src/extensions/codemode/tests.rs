//! Ports of the codemode description catalog tests (`test/tool-search.test.ts`), the renderer tests
//! (`test/codemode-renderer.test.ts`), and direct `execute_codemode` checks.

use tokio_util::sync::CancellationToken;
use super::execute::{execute_codemode, read_codemode_store};
use super::renderer::codemode_renderers;
use super::tool::*;
use crate::core::extensions::types::{ToolNamespace, ToolRenderContext, ToolRenderResultOptions};
use crate::core::session_manager::{CustomEntry, SessionEntry};
use indexmap::{IndexMap, IndexSet};
use pi_agent::types::{AgentToolRef, AgentToolResult, FnAgentTool};
use pi_ai::types::UserContentBlock;
use serde_json::{Value, json};
use std::sync::Arc;

fn tool(name: &str, description: &str) -> AgentToolRef {
    Arc::new(FnAgentTool::new(name, name, description, json!({ "type": "object", "properties": {} }), |_, _, _, _| async {
        Ok(AgentToolResult::default())
    }))
}

struct Catalog {
    plain: AgentToolRef,
    github: Vec<AgentToolRef>,
    docs: Vec<AgentToolRef>,
    namespaces: IndexMap<String, ToolNamespace>,
}

impl Catalog {
    fn new() -> Self {
        let plain = tool("read_notes", "Read notes.");
        let github: Vec<AgentToolRef> =
            ["a", "b", "c"].iter().map(|s| tool(&format!("mcp__github__{s}"), &format!("GitHub {s}."))).collect();
        let docs = vec![tool("mcp__docs__search", "Search docs."), tool("mcp__docs__long", &"Long ".repeat(200))];
        let mut namespaces = IndexMap::new();
        for entry in &github {
            namespaces.insert(
                entry.name().to_string(),
                ToolNamespace { name: "mcp__github".into(), description: Some("GitHub server".into()), ..Default::default() },
            );
        }
        for entry in &docs {
            namespaces.insert(entry.name().to_string(), ToolNamespace { name: "mcp__docs".into(), ..Default::default() });
        }
        Self { plain, github, docs, namespaces }
    }

    fn all(&self) -> Vec<AgentToolRef> {
        let mut all = vec![self.plain.clone()];
        all.extend(self.github.iter().cloned());
        all.extend(self.docs.iter().cloned());
        all
    }

    fn options(&self, inline_budget: Option<f64>) -> CodemodeDescriptionOptions {
        CodemodeDescriptionOptions { namespaces: Some(self.namespaces.clone()), inline_budget, ..Default::default() }
    }
}

#[test]
fn catalog_lists_everything_without_a_budget() {
    let catalog = Catalog::new();
    let description = create_codemode_description(&catalog.all(), Some(&catalog.options(None)));
    assert!(description.contains("Nested tools:"));
    assert!(description.contains("## mcp__github\nGitHub server"));
    assert!(description.contains("## mcp__docs\n\n### `mcp__docs"));
    assert!(description.contains("find unlisted tools, such as MCP tools"));
    assert!(description.contains("`await searchTools(query, { limit?, namespace? })`"));
    assert!(description.contains("`await describeTool(name)`"));
    assert!(description.contains("`await describeNamespace(name)`"));
}

#[test]
fn catalog_fills_the_budget_round_robin_cheapest_first() {
    let catalog = Catalog::new();
    let description = create_codemode_description(&catalog.all(), Some(&catalog.options(Some(170.0))));
    assert!(description.contains("### `read_notes`"));
    assert!(description.contains("## mcp__docs (some tools not listed)"));
    assert!(description.contains("### `mcp__docs__search`"));
    assert!(!description.contains("### `mcp__docs__long`"));
    assert!(description.contains("## mcp__github (some tools not listed)"));
    assert!(description.contains("find unlisted tools, such as MCP tools"));
    assert_eq!(create_codemode_description(&catalog.all(), Some(&catalog.options(Some(170.0)))), description);
}

#[test]
fn catalog_leaves_deferred_tools_and_their_namespaces_out() {
    let catalog = Catalog::new();
    let mut options = catalog.options(None);
    options.deferred = Some(catalog.github.iter().map(|t| t.name().to_string()).collect::<IndexSet<_>>());
    let description = create_codemode_description(&catalog.all(), Some(&options));
    assert!(!description.contains("mcp__github"));
    let mut without = vec![catalog.plain.clone()];
    without.extend(catalog.docs.iter().cloned());
    assert_eq!(description, create_codemode_description(&without, Some(&catalog.options(None))));
}

#[test]
fn catalog_leaves_namespace_instructions_out() {
    let catalog = Catalog::new();
    let namespaces = catalog
        .github
        .iter()
        .map(|t| {
            (
                t.name().to_string(),
                ToolNamespace {
                    name: "mcp__github".into(),
                    instructions: Some("Long usage guide.".into()),
                    ..Default::default()
                },
            )
        })
        .collect();
    let description = create_codemode_description(
        &catalog.github,
        Some(&CodemodeDescriptionOptions { namespaces: Some(namespaces), ..Default::default() }),
    );
    assert!(description.contains("## mcp__github\n\n### `mcp__github__a`"));
    assert!(!description.contains("Long usage guide."));
}

#[test]
fn catalog_lists_only_namespaces_with_a_zero_budget() {
    let catalog = Catalog::new();
    let description = create_codemode_description(&catalog.all(), Some(&catalog.options(Some(0.0))));
    assert!(description.contains("## mcp__docs (tools not listed)"));
    assert!(!description.contains("codemode tool declaration:"));
}

#[test]
fn declaration_appends_guidelines_and_defaults_to_text_output() {
    let echo = tool("echo", "Echo text back.\n");
    let declaration = to_codemode_declaration(echo.as_ref(), Some(&["Use it.".to_string(), "  ".to_string()]));
    assert_eq!(declaration.description.as_deref(), Some("Echo text back.\n\n- Use it."));
    assert_eq!(declaration.output_schema, Some(json!({ "type": "string" })));
    let plain = to_codemode_declaration(echo.as_ref(), None);
    assert_eq!(plain.description.as_deref(), Some("Echo text back.\n"));
}

#[test]
fn reaches_tools_by_words_or_all() {
    let definition = create_codemode_tool_definition(None);
    let reach = definition.reaches_tools.unwrap();
    let input = |code: &str| json!({ "code": code }).as_object().unwrap().clone();
    assert_eq!(
        serde_json::to_value(reach(&input("await tools.mcp__docs__search({})"))).unwrap(),
        json!(["await", "tools", "mcp__docs__search"])
    );
    assert_eq!(serde_json::to_value(reach(&input("await searchTools('x')"))).unwrap(), json!("all"));
    assert_eq!(serde_json::to_value(reach(&json!({}).as_object().unwrap().clone())).unwrap(), json!([]));
}

#[test]
fn reads_the_store_from_branch_entries() {
    let entry = |data: Value| {
        SessionEntry::Custom(CustomEntry {
            custom_type: CODEMODE_STORE_ENTRY_TYPE.into(),
            data: Some(data),
            id: String::new(),
            parent_id: None,
            timestamp: String::new(),
            extra: serde_json::Map::new(),
        })
    };
    let branch = vec![
        entry(json!({ "set": { "a": 1, "b": 2 }, "delete": [] })),
        entry(json!({ "set": { "c": 3 }, "delete": ["a"] })),
        entry(json!({ "set": "invalid", "delete": [] })),
    ];
    let store = read_codemode_store(&branch);
    assert_eq!(serde_json::to_value(store).unwrap(), json!({ "b": 2, "c": 3 }));
}

fn result_text(result: &AgentToolResult) -> (String, String) {
    let header = result.content[0].as_text().unwrap().text.clone();
    let rest = result.content[1..]
        .iter()
        .map(|block| match block {
            UserContentBlock::Text(t) => t.text.clone(),
            _ => "<image>".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    (header, rest)
}

#[tokio::test]
async fn executes_scripts_without_a_session() {
    let input = CodemodeToolInput { code: "text('a'); console.log('log'); return { b: 1 };".into() };
    let result = execute_codemode("call", &input, None, None, None, None).await.unwrap();
    let (header, rest) = result_text(&result);
    assert!(header.starts_with("Script completed\nWall time "), "{header}");
    assert_eq!(rest, "==> text 1/2 <==\na\n==> text 2/2 <==\n{\"b\":1}\n<console_output>\nlog\n</console_output>");
    assert_eq!(result.is_error, None);

    let failed = execute_codemode(
        "call",
        &CodemodeToolInput { code: "text('before'); throw new Error('boom')".into() },
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let (header, rest) = result_text(&failed);
    assert!(header.starts_with("Script failed\n"));
    assert!(rest.starts_with("before\nScript error:\n"), "{rest}");
    assert!(rest.contains("boom"));
    assert!(rest.ends_with("No tool calls were made."));
    assert_eq!(failed.is_error, Some(true));

    let invalid = execute_codemode(
        "call",
        &CodemodeToolInput { code: "// @options: {}".into() },
        None,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(
        invalid.unwrap_err().to_string(),
        "The @options line must be followed by JavaScript source on subsequent lines"
    );
}

// The agent aborts tools with `controller.abort()`, whose reason is Node's DOMException
// "This operation was aborted".
#[tokio::test]
async fn aborted_scripts_report_the_default_abort_reason() {
    let token = CancellationToken::new();
    token.cancel();
    let input = CodemodeToolInput { code: "return 1".into() };
    let result = execute_codemode("call", &input, Some(token), None, None, None).await.unwrap();
    let (_, rest) = result_text(&result);
    assert_eq!(rest, "Script error:\nScript aborted: This operation was aborted\n\nThe script did not start.");

    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();
    });
    let input = CodemodeToolInput { code: "while (true) await null".into() };
    let result = execute_codemode("call", &input, Some(token), None, None, None).await.unwrap();
    let (_, rest) = result_text(&result);
    assert!(rest.contains("Script aborted: This operation was aborted"), "{rest}");
}

// --- renderer (`test/codemode-renderer.test.ts`) ---

fn render(result: AgentToolResult, is_error: bool, expanded: bool, width: usize) -> String {
    crate::modes::interactive::theme::theme::init_theme(Some("dark"), Some(false));
    let context = ToolRenderContext {
        args: json!({ "code": "" }),
        tool_call_id: "call".into(),
        invalidate: Arc::new(|| {}),
        last_component: None,
        state: Default::default(),
        cwd: "/".into(),
        execution_started: true,
        args_complete: true,
        is_partial: false,
        expanded,
        show_images: false,
        is_error,
        duration_ms: None,
        output_pad: 1,
    };
    let theme = crate::modes::interactive::theme::theme::theme();
    let renderers = codemode_renderers();
    let component = (renderers.render_result.unwrap())(
        &result,
        ToolRenderResultOptions { expanded, is_partial: false },
        &theme,
        &context,
    );
    let text = crate::utils::ansi::strip_ansi(&component.render(width).join("\n")).unwrap();
    text.split('\n').map(|line| line.trim_end()).collect::<Vec<_>>().join("\n").trim().to_string()
}

fn header() -> UserContentBlock {
    UserContentBlock::text("Script completed\nWall time 0.1 seconds\nOutput:\n")
}

#[test]
fn renderer_hides_the_script_header_and_shows_the_output() {
    let text = render(
        AgentToolResult {
            content: vec![header(), UserContentBlock::text("hello")],
            details: Some(json!({ "calls": [{ "id": "call/1", "name": "read", "args": "{\"path\":\"a\"}", "status": "ok", "durationMs": 5 }] })),
            ..Default::default()
        },
        false,
        true,
        200,
    );
    assert_eq!(text, "✓ read {\"path\":\"a\"} 5ms\n\nhello");
}

#[test]
fn renderer_shows_the_cost_of_model_calls_and_their_total() {
    let call = |id: &str, cost: Option<f64>| {
        let mut call = json!({ "id": id, "name": "models.classify", "args": "scorer/judge", "status": "ok", "durationMs": 5 });
        if let Some(cost) = cost {
            call["cost"] = json!(cost);
        }
        call
    };
    let text = render(
        AgentToolResult {
            content: vec![header()],
            details: Some(json!({ "calls": [call("c/1", Some(0.000012936)), call("c/2", Some(0.02)), call("c/3", None)] })),
            ..Default::default()
        },
        false,
        true,
        200,
    );
    assert_eq!(
        text,
        [
            "✓ models.classify scorer/judge 5ms $0.000013",
            "✓ models.classify scorer/judge 5ms $0.02",
            "✓ models.classify scorer/judge 5ms",
            "Model calls: $0.02",
        ]
        .join("\n")
    );
}

#[test]
fn renderer_shows_results_without_a_header() {
    let text = render(
        AgentToolResult {
            content: vec![UserContentBlock::text("The @options line must be followed by JavaScript source")],
            ..Default::default()
        },
        true,
        true,
        200,
    );
    assert_eq!(text, "The @options line must be followed by JavaScript source");
}

#[test]
fn renderer_limits_collapsed_output_to_wrapped_lines() {
    let text = render(
        AgentToolResult {
            content: vec![header(), UserContentBlock::text("x".repeat(1000))],
            details: Some(json!({ "calls": [], "fullOutputPath": "/tmp/out.txt" })),
            ..Default::default()
        },
        false,
        false,
        50,
    );
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(lines.len(), 7, "{text}");
    for line in &lines[..5] {
        assert_eq!(*line, "x".repeat(50));
    }
    assert!(lines[5].starts_with("... (15 more lines,"), "{}", lines[5]);
    assert_eq!(lines[6], "Full output: /tmp/out.txt");
}
