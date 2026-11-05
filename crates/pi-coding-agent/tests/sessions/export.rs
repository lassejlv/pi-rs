//! HTML export (port of the runnable parts of `test/export-html-*.test.ts`).

use crate::common::*;
use base64::Engine;
use pi_coding_agent::core::export_html::tool_renderer::{ToolHtmlRendererDeps, create_tool_html_renderer};
use pi_coding_agent::core::export_html::{ExportSessionToHtmlOptions, export_from_file, export_session_to_html};
use pi_coding_agent::core::extensions::types::ToolRenderers;
use pi_coding_agent::core::session_manager::SessionManager;
use pi_tui::tui::{Component, ComponentRef};
use serde_json::{Value, json};
use std::sync::Arc;

fn session_data(html: &str) -> Value {
    let re = regex::Regex::new(r#"<script id="session-data" type="application/json">([^<]+)</script>"#).unwrap();
    let encoded = &re.captures(html).expect("session data")[1];
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap();
    serde_json::from_slice(&decoded).unwrap()
}

#[tokio::test]
async fn export_from_file_embeds_session_data_and_assets() {
    let dir = tmp();
    let d = path_str(dir.path());
    let session = SessionManager::create(&d, Some(&d), None).unwrap();
    let user_id = session.append_message(user_msg("hello")).unwrap();
    let answer_id = session.append_message(assistant_msg("world")).unwrap();
    let file = session.get_session_file().unwrap();

    let out = path_str(&dir.path().join("out.html"));
    let written = export_from_file(&file, Some(ExportSessionToHtmlOptions::String(out.clone())))
        .await
        .unwrap();
    assert_eq!(written, out);
    let html = std::fs::read_to_string(&out).unwrap();
    // The vendored highlight.js contains `$&`, which `String.prototype.replace` expands to the
    // placeholder text (TS exports carry it too), so only the template slot is checked.
    assert!(!html.contains("{{CSS}}") && !html.contains("{{JS}}") && !html.contains("<script>{{HIGHLIGHT_JS}}</script>"));
    assert!(html.contains("--exportPageBg:"));
    let data = session_data(&html);
    assert_eq!(data["header"]["type"], "session");
    assert_eq!(data["header"]["id"], json!(session.get_session_id()));
    assert_eq!(data["leafId"], json!(answer_id));
    assert_eq!(data["entries"][0]["id"], json!(user_id));
    assert!(data.get("systemPrompt").is_none());

    // Missing input file.
    let error = export_from_file(&path_str(&dir.path().join("missing.jsonl")), None).await.unwrap_err();
    assert!(error.to_string().starts_with("File not found: "));
}

#[tokio::test]
async fn export_session_to_html_requires_a_persisted_file() {
    let memory = in_memory();
    assert_eq!(
        export_session_to_html(&memory, None, None).await.unwrap_err().to_string(),
        "Cannot export in-memory session to HTML"
    );
    let dir = tmp();
    let d = path_str(dir.path());
    let session = SessionManager::create(&d, Some(&d), None).unwrap();
    assert_eq!(
        export_session_to_html(&session, None, None).await.unwrap_err().to_string(),
        "Nothing to export yet - start a conversation first"
    );
}

struct Lines(Vec<String>);

impl Component for Lines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.clone()
    }
}

#[test]
fn trims_tui_spacing_lines_from_custom_tool_result_html() {
    let Some(theme) = pi_coding_agent::modes::interactive::theme::theme::get_theme_by_name("dark") else {
        return;
    };
    let component: ComponentRef = Arc::new(Lines(vec![
        String::new(),
        "\u{1b}[31mone\u{1b}[0m".into(),
        "two".into(),
        String::new(),
    ]));
    let renderer = create_tool_html_renderer(&ToolHtmlRendererDeps {
        get_tool_renderers: Arc::new(move |_| {
            let component = component.clone();
            Some(ToolRenderers {
                render_shell: None,
                render_call: None,
                render_result: Some(Arc::new(move |_, _, _, _| component.clone())),
            })
        }),
        theme,
        cwd: "/tmp".into(),
        width: None,
    });
    let rendered = renderer.render_result("id", "custom", &[], Value::Null, false).unwrap();
    assert_eq!(
        rendered.expanded.as_deref(),
        Some("<div class=\"ansi-line\"><span style=\"color:#800000\">one</span></div><div class=\"ansi-line\">two</div>")
    );
    // Collapsed equals expanded, so it is omitted.
    assert!(rendered.collapsed.is_none());
    assert!(renderer.render_call("id", "custom", json!({})).is_none());
}

// ---------------------------------------------------------------------------
// export-html-cjk.test.ts (#10154). TS runs the exported viewer scripts in `node:vm`; here they
// run in pi-jsrt (QuickJS) with the same `document`/`window` stubs.
// ---------------------------------------------------------------------------

async fn load_markdown_parser(html: &str) -> pi_jsrt::JsRuntime {
    let data_re = regex::Regex::new(r#"<script id="session-data" type="application/json">([^<]+)</script>"#).unwrap();
    let session_data = data_re.captures(html).expect("session data")[1].to_string();
    let script_re = regex::Regex::new(r"(?s)<script>(.*?)</script>").unwrap();
    let mut scripts: Vec<String> = script_re.captures_iter(html).map(|c| c[1].to_string()).collect();
    let viewer = scripts.pop().expect("viewer script");
    let runtime = pi_jsrt::JsRuntime::new(pi_jsrt::RuntimeOptions::sandboxed()).unwrap();
    runtime
        .eval(format!(
            "globalThis.document = {{ getElementById: () => ({{ textContent: {} }}), querySelector: () => null }};\
             globalThis.window = {{ location: {{ search: '' }} }};",
            serde_json::to_string(&session_data).unwrap()
        ))
        .await
        .unwrap();
    for (index, script) in scripts.into_iter().enumerate() {
        runtime.eval_script(script, format!("script-{index}.js")).await.unwrap();
    }
    let viewer = viewer.replacen(
        "function safeMarkedParse(",
        "globalThis.__safeMarkedParse = safeMarkedParse; return safeMarkedParse; function safeMarkedParse(",
        1,
    );
    assert!(viewer.contains("__safeMarkedParse"), "viewer defines safeMarkedParse");
    runtime.eval_script(viewer, "viewer.js").await.unwrap();
    runtime
}

async fn parse_markdown(runtime: &pi_jsrt::JsRuntime, text: &str) -> String {
    let value = runtime
        .eval(format!("__safeMarkedParse({})", serde_json::to_string(text).unwrap()))
        .await
        .unwrap();
    value.as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn export_html_renders_emphasis_delimited_next_to_cjk_punctuation() {
    let dir = tmp();
    let header = json!({
        "type": "session",
        "version": pi_coding_agent::core::session_manager::CURRENT_SESSION_VERSION,
        "id": "cjk-test",
        "timestamp": "1970-01-01T00:00:00.000Z",
        "cwd": path_str(dir.path()),
    });
    let session_path = dir.path().join("session.jsonl");
    std::fs::write(&session_path, format!("{header}\n")).unwrap();
    let out = path_str(&dir.path().join("out.html"));
    let written = export_from_file(&path_str(&session_path), Some(ExportSessionToHtmlOptions::String(out))).await.unwrap();
    let runtime = load_markdown_parser(&std::fs::read_to_string(written).unwrap()).await;

    assert_eq!(parse_markdown(&runtime, "**这是测试。**后面").await, "<p><strong>这是测试。</strong>后面</p>\n");
    assert_eq!(
        parse_markdown(&runtime, "**一句话说清：**一台机器").await,
        "<p><strong>一句话说清：</strong>一台机器</p>\n"
    );
    assert_eq!(
        parse_markdown(&runtime, "**要点到此为止。**20 小时").await,
        "<p><strong>要点到此为止。</strong>20 小时</p>\n"
    );
    assert_eq!(parse_markdown(&runtime, "字**「引用」**字").await, "<p>字<strong>「引用」</strong>字</p>\n");
    assert_eq!(parse_markdown(&runtime, "*斜体。*后面").await, "<p><em>斜体。</em>后面</p>\n");
    // Keeps CommonMark flanking rules for non-CJK text.
    assert_eq!(parse_markdown(&runtime, "**end.**Then next").await, "<p>**end.**Then next</p>\n");
    runtime.shutdown();
}
