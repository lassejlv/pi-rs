//! Port of `packages/coding-agent/src/core/export-html/index.ts`.
//!
//! The HTML template, stylesheet, script and vendored libraries are embedded in the binary
//! (`template.*`, `vendor/*.js` next to this module) instead of being read from
//! `getExportTemplateDir()` at runtime.

pub mod ansi_to_html;
pub mod tool_renderer;

use crate::config::APP_NAME;
use crate::core::session_manager::{FileEntry, SessionEntry, SessionManager};
use crate::modes::interactive::theme::theme::{get_resolved_theme_colors, get_theme_export_colors};
use crate::utils::paths::{normalize_path, resolve_path};
use base64::Engine;
use indexmap::IndexMap;
use pi_agent::types::AgentState;
use pi_ai::types::AssistantContent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, LazyLock};

pub use tool_renderer::{ToolHtmlRenderer, ToolHtmlRendererRenderResultResult, ToolHtmlRendererRenderResultResult2};

pub const TEMPLATE_HTML: pi_embed::Embedded = pi_embed::include_str!("template.html");
pub const TEMPLATE_CSS: pi_embed::Embedded = pi_embed::include_str!("template.css");
pub const TEMPLATE_JS: pi_embed::Embedded = pi_embed::include_str!("template.js");
pub const MARKED_JS: pi_embed::Embedded = pi_embed::include_str!("vendor/marked.min.js");
pub const MARKED_CJK_FRIENDLY_JS: pi_embed::Embedded = pi_embed::include_str!("vendor/marked-cjk-friendly.min.js");
pub const HIGHLIGHT_JS: pi_embed::Embedded = pi_embed::include_str!("vendor/highlight.min.js");

/// Pre-rendered HTML for a custom tool call and result
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedToolHtml {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_html: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_html_collapsed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_html_expanded: Option<String>,
}

#[derive(Clone, Default)]
pub struct ExportOptions {
    pub output_path: Option<String>,
    pub theme_name: Option<String>,
    /// Optional tool renderer for custom tools
    pub tool_renderer: Option<Arc<dyn ToolHtmlRenderer>>,
}

/// RGB components of a parsed color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rgb {
    r: i64,
    g: i64,
    b: i64,
}

static HEX_COLOR: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^#([0-9a-fA-F]{2})([0-9a-fA-F]{2})([0-9a-fA-F]{2})$").expect("valid regex")
});
static RGB_COLOR: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^rgb\s*\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*\)$").expect("valid regex")
});

/// Parse a color string to RGB values. Supports hex (#RRGGBB) and rgb(r,g,b) formats.
fn parse_color(color: &str) -> Option<Rgb> {
    if let Some(caps) = HEX_COLOR.captures(color) {
        let hex = |i: usize| i64::from_str_radix(&caps[i], 16).unwrap_or(0);
        return Some(Rgb {
            r: hex(1),
            g: hex(2),
            b: hex(3),
        });
    }
    if let Some(caps) = RGB_COLOR.captures(color) {
        let dec = |i: usize| caps[i].parse::<i64>().unwrap_or(i64::MAX);
        return Some(Rgb {
            r: dec(1),
            g: dec(2),
            b: dec(3),
        });
    }
    None
}

/// Calculate relative luminance of a color (0-1, higher = lighter).
fn get_luminance(r: i64, g: i64, b: i64) -> f64 {
    let to_linear = |c: i64| {
        let s = c as f64 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * to_linear(r) + 0.7152 * to_linear(g) + 0.0722 * to_linear(b)
}

/// JS `Math.round` (rounds .5 up).
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// Adjust color brightness. Factor > 1 lightens, < 1 darkens.
fn adjust_brightness(color: &str, factor: f64) -> String {
    let Some(parsed) = parse_color(color) else {
        return color.to_string();
    };
    let adjust = |c: i64| js_round(c as f64 * factor).clamp(0.0, 255.0) as i64;
    format!("rgb({}, {}, {})", adjust(parsed.r), adjust(parsed.g), adjust(parsed.b))
}

struct ExportColors {
    page_bg: String,
    card_bg: String,
    info_bg: String,
}

/// Derive export background colors from a base color (e.g., userMessageBg).
fn derive_export_colors(base_color: &str) -> ExportColors {
    let Some(parsed) = parse_color(base_color) else {
        return ExportColors {
            page_bg: "rgb(24, 24, 30)".into(),
            card_bg: "rgb(30, 30, 36)".into(),
            info_bg: "rgb(60, 55, 40)".into(),
        };
    };

    let luminance = get_luminance(parsed.r, parsed.g, parsed.b);
    let is_light = luminance > 0.5;

    if is_light {
        return ExportColors {
            page_bg: adjust_brightness(base_color, 0.96),
            card_bg: base_color.to_string(),
            info_bg: format!(
                "rgb({}, {}, {})",
                (parsed.r + 10).min(255),
                (parsed.g + 5).min(255),
                (parsed.b - 20).max(0)
            ),
        };
    }
    ExportColors {
        page_bg: adjust_brightness(base_color, 0.7),
        card_bg: adjust_brightness(base_color, 0.85),
        info_bg: format!(
            "rgb({}, {}, {})",
            (parsed.r + 20).min(255),
            (parsed.g + 15).min(255),
            parsed.b
        ),
    }
}

fn user_message_bg(colors: &IndexMap<String, String>) -> String {
    colors
        .get("userMessageBg")
        .filter(|c| !c.is_empty())
        .cloned()
        .unwrap_or_else(|| "#343541".to_string())
}

/// Generate CSS custom property declarations from theme colors.
fn generate_theme_vars(theme_name: Option<&str>) -> String {
    let colors = get_resolved_theme_colors(theme_name);
    let mut lines: Vec<String> = colors
        .iter()
        .map(|(key, value)| format!("--{key}: {value};"))
        .collect();

    // Use explicit theme export colors if available, otherwise derive from userMessageBg
    let theme_export = get_theme_export_colors(theme_name);
    let derived_colors = derive_export_colors(&user_message_bg(&colors));

    lines.push(format!(
        "--exportPageBg: {};",
        theme_export.page_bg.unwrap_or(derived_colors.page_bg)
    ));
    lines.push(format!(
        "--exportCardBg: {};",
        theme_export.card_bg.unwrap_or(derived_colors.card_bg)
    ));
    lines.push(format!(
        "--exportInfoBg: {};",
        theme_export.info_bg.unwrap_or(derived_colors.info_bg)
    ));

    lines.join("\n      ")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDataTools {
    /// Tool name (used in LLM tool calls)
    pub name: String,
    /// Description for LLM
    pub description: String,
    /// Parameter schema (TypeBox)
    pub parameters: Value,
}

/// Data embedded (base64 JSON) in the exported page.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionData {
    /// Session header (with `"type": "session"`), or null.
    pub header: Value,
    /// Session entries (excluding the header).
    pub entries: Value,
    #[serde(default)]
    pub leaf_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<SessionDataTools>>,
    /// Pre-rendered HTML for custom tool calls/results, keyed by tool call ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendered_tools: Option<IndexMap<String, RenderedToolHtml>>,
}

/// Replace the first occurrence of `pattern`, exactly like TS `String.prototype.replace` with a
/// string pattern: `$$`, `$&`, `` $` `` and `$'` in the replacement are substitution patterns
/// (`GetSubstitution`). The vendored highlight.js contains `$&`, so TS exports carry the
/// substituted text; reproducing it keeps exports byte-identical.
fn replace_first(haystack: &str, pattern: &str, replacement: &str) -> String {
    let Some(index) = haystack.find(pattern) else {
        return haystack.to_string();
    };
    let before = &haystack[..index];
    let after = &haystack[index + pattern.len()..];
    let mut out = String::with_capacity(haystack.len() + replacement.len());
    out.push_str(before);
    let mut rest = replacement;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let tail = &rest[dollar + 1..];
        match tail.as_bytes().first() {
            Some(b'$') => out.push('$'),
            Some(b'&') => out.push_str(pattern),
            Some(b'`') => out.push_str(before),
            Some(b'\'') => out.push_str(after),
            _ => {
                out.push('$');
                rest = tail;
                continue;
            }
        }
        rest = &tail[1..];
    }
    out.push_str(rest);
    out.push_str(after);
    out
}

/// Core HTML generation logic shared by both export functions.
fn generate_html(session_data: &SessionData, theme_name: Option<&str>) -> anyhow::Result<String> {
    let theme_vars = generate_theme_vars(theme_name);
    let colors = get_resolved_theme_colors(theme_name);
    let theme_export = get_theme_export_colors(theme_name);
    let derived_export_colors = derive_export_colors(&user_message_bg(&colors));
    let body_bg = theme_export.page_bg.unwrap_or(derived_export_colors.page_bg);
    let container_bg = theme_export.card_bg.unwrap_or(derived_export_colors.card_bg);
    let info_bg = theme_export.info_bg.unwrap_or(derived_export_colors.info_bg);

    // Base64 encode session data to avoid escaping issues
    let session_data_base64 =
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_string(session_data)?);

    // Build the CSS with theme variables injected
    let mut css = replace_first(TEMPLATE_CSS.get(), "{{THEME_VARS}}", &theme_vars);
    css = replace_first(&css, "{{BODY_BG}}", &body_bg);
    css = replace_first(&css, "{{CONTAINER_BG}}", &container_bg);
    css = replace_first(&css, "{{INFO_BG}}", &info_bg);

    let mut html = replace_first(TEMPLATE_HTML.get(), "{{CSS}}", &css);
    html = replace_first(&html, "{{JS}}", TEMPLATE_JS.get());
    html = replace_first(&html, "{{SESSION_DATA}}", &session_data_base64);
    html = replace_first(&html, "{{MARKED_JS}}", MARKED_JS.get());
    html = replace_first(&html, "{{MARKED_CJK_FRIENDLY_JS}}", MARKED_CJK_FRIENDLY_JS.get());
    html = replace_first(&html, "{{HIGHLIGHT_JS}}", HIGHLIGHT_JS.get());
    Ok(html)
}

/// Tools rendered directly by the HTML template (not pre-rendered via TUI→ANSI→HTML pipeline)
const TEMPLATE_RENDERED_TOOLS: [&str; 5] = ["bash", "read", "write", "edit", "ls"];

/// Pre-render custom tools to HTML using their TUI renderers.
fn pre_render_custom_tools(
    entries: &[SessionEntry],
    tool_renderer: &dyn ToolHtmlRenderer,
) -> IndexMap<String, RenderedToolHtml> {
    let mut rendered_tools: IndexMap<String, RenderedToolHtml> = IndexMap::new();

    for entry in entries {
        let SessionEntry::Message(entry) = entry else {
            continue;
        };
        match &entry.message {
            // Find tool calls in assistant messages
            pi_agent::types::AgentMessage::Assistant(assistant) => {
                for block in &assistant.content {
                    if let AssistantContent::ToolCall(call) = block
                        && !TEMPLATE_RENDERED_TOOLS.contains(&call.name.as_str())
                        && let Some(call_html) = tool_renderer
                            .render_call(&call.id, &call.name, Value::Object(call.arguments.clone()))
                            .filter(|html| !html.is_empty())
                    {
                        rendered_tools.insert(
                            call.id.clone(),
                            RenderedToolHtml {
                                call_html: Some(call_html),
                                ..Default::default()
                            },
                        );
                    }
                }
            }
            // Find tool results
            pi_agent::types::AgentMessage::ToolResult(result) if !result.tool_call_id.is_empty() => {
                let tool_name = result.tool_name.as_str();
                // Only render if we have a pre-rendered call OR it's not template-rendered
                let existing = rendered_tools.get(&result.tool_call_id).cloned();
                if existing.is_some() || !TEMPLATE_RENDERED_TOOLS.contains(&tool_name) {
                    let blocks: Vec<ToolHtmlRendererRenderResultResult> =
                        result.content.iter().map(Into::into).collect();
                    if let Some(rendered) = tool_renderer.render_result(
                        &result.tool_call_id,
                        tool_name,
                        &blocks,
                        result.details.clone().unwrap_or(Value::Null),
                        result.is_error,
                    ) {
                        let mut html = existing.unwrap_or_default();
                        html.result_html_collapsed = rendered.collapsed;
                        html.result_html_expanded = rendered.expanded;
                        rendered_tools.insert(result.tool_call_id.clone(), html);
                    }
                }
            }
            _ => {}
        }
    }

    rendered_tools
}

#[derive(Clone)]
pub enum ExportSessionToHtmlOptions {
    ExportOptions(ExportOptions),
    String(String),
}

impl ExportSessionToHtmlOptions {
    fn into_options(self) -> ExportOptions {
        match self {
            Self::ExportOptions(options) => options,
            Self::String(output_path) => ExportOptions {
                output_path: Some(output_path),
                ..Default::default()
            },
        }
    }
}

fn header_value(sm: &SessionManager) -> Value {
    sm.get_header()
        .map(|header| serde_json::to_value(FileEntry::Session(header)).unwrap_or(Value::Null))
        .unwrap_or(Value::Null)
}

fn output_path_for(opts: &ExportOptions, source_path: &str) -> String {
    match opts.output_path.as_deref().filter(|p| !p.is_empty()) {
        Some(path) => normalize_path(path, None),
        None => {
            let file_name = std::path::Path::new(source_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let base = file_name.strip_suffix(".jsonl").unwrap_or(&file_name);
            format!("{APP_NAME}-session-{base}.html")
        }
    }
}

/// Export session to HTML using SessionManager and AgentState.
/// Used by TUI's /export command.
pub async fn export_session_to_html(
    sm: &SessionManager,
    state: Option<&AgentState>,
    options: Option<ExportSessionToHtmlOptions>,
) -> anyhow::Result<String> {
    let opts = options.map(ExportSessionToHtmlOptions::into_options).unwrap_or_default();

    let Some(session_file) = sm.get_session_file() else {
        anyhow::bail!("Cannot export in-memory session to HTML");
    };
    if !std::path::Path::new(&session_file).exists() {
        anyhow::bail!("Nothing to export yet - start a conversation first");
    }

    let entries = sm.get_entries();

    // Pre-render custom tools if a tool renderer is provided
    let rendered_tools = opts
        .tool_renderer
        .as_ref()
        .map(|renderer| pre_render_custom_tools(&entries, renderer.as_ref()))
        // Only include if we actually rendered something
        .filter(|rendered| !rendered.is_empty());

    let session_data = SessionData {
        header: header_value(sm),
        entries: serde_json::to_value(&entries)?,
        leaf_id: sm.get_leaf_id(),
        system_prompt: state.map(AgentState::system_prompt),
        tools: state.map(|state| {
            state
                .tools
                .iter()
                .map(|tool| SessionDataTools {
                    name: tool.name().to_string(),
                    description: tool.description().to_string(),
                    parameters: tool.parameters().clone(),
                })
                .collect()
        }),
        rendered_tools,
    };

    let html = generate_html(&session_data, opts.theme_name.as_deref())?;

    let output_path = output_path_for(&opts, &session_file);
    std::fs::write(&output_path, html)?;
    Ok(output_path)
}

pub type ExportFromFileOptions = ExportSessionToHtmlOptions;

/// Export session file to HTML (standalone, without AgentState).
/// Used by CLI for exporting arbitrary session files.
pub async fn export_from_file(
    input_path: &str,
    options: Option<ExportFromFileOptions>,
) -> anyhow::Result<String> {
    let opts = options.map(ExportSessionToHtmlOptions::into_options).unwrap_or_default();
    let resolved_input_path = resolve_path(input_path, None, None);

    if !std::path::Path::new(&resolved_input_path).exists() {
        anyhow::bail!("File not found: {resolved_input_path}");
    }

    let sm = SessionManager::open(&resolved_input_path, None, None)?;

    let session_data = SessionData {
        header: header_value(&sm),
        entries: serde_json::to_value(sm.get_entries())?,
        leaf_id: sm.get_leaf_id(),
        system_prompt: None,
        tools: None,
        rendered_tools: None,
    };

    let html = generate_html(&session_data, opts.theme_name.as_deref())?;

    let output_path = output_path_for(&opts, &resolved_input_path);
    std::fs::write(&output_path, html)?;
    Ok(output_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_export_colors() {
        let dark = derive_export_colors("#343541");
        assert_eq!(dark.page_bg, "rgb(36, 37, 46)");
        assert_eq!(dark.card_bg, "rgb(44, 45, 55)");
        assert_eq!(dark.info_bg, "rgb(72, 68, 65)");
        let light = derive_export_colors("rgb(240, 240, 240)");
        assert_eq!(light.page_bg, "rgb(230, 230, 230)");
        assert_eq!(light.card_bg, "rgb(240, 240, 240)");
        assert_eq!(light.info_bg, "rgb(250, 245, 220)");
        let fallback = derive_export_colors("blue");
        assert_eq!(fallback.page_bg, "rgb(24, 24, 30)");
    }

    #[test]
    fn replace_first_follows_js_substitution_patterns() {
        assert_eq!(replace_first("a{{X}}b{{X}}", "{{X}}", "$&"), "a{{X}}b{{X}}");
        assert_eq!(replace_first("a{{X}}b", "{{X}}", "[$$|$`|$'|$1|$<n>|$]"), "a[$|a|b|$1|$<n>|$]b");
        assert_eq!(replace_first("abc", "{{X}}", "$&"), "abc");
    }

    #[test]
    fn template_assets_are_embedded() {
        assert!(TEMPLATE_HTML.get().contains("{{SESSION_DATA}}"));
        assert!(TEMPLATE_CSS.get().contains("{{THEME_VARS}}"));
        assert!(TEMPLATE_JS.get().contains("parseSkillBlock"));
        assert!(!MARKED_JS.get().is_empty() && !HIGHLIGHT_JS.get().is_empty() && !MARKED_CJK_FRIENDLY_JS.get().is_empty());
    }

    /// Port of export-html-xss.test.ts / export-html-skill-block.test.ts / whitespace CSS checks:
    /// the embedded template keeps its sanitization and rendering hooks.
    #[test]
    fn template_keeps_sanitization_and_rendering_hooks() {
        let js = TEMPLATE_JS.get();
        let re = |pattern: &str| regex::Regex::new(pattern).unwrap();
        assert!(re(r"link\s*\(\s*token\s*\)").is_match(js));
        assert!(re(r"image\s*\(\s*token\s*\)").is_match(js));
        assert!(js.contains("sanitizeMarkdownUrl(token.href)"));
        assert!(js.contains("^(https?|mailto|tel|ftp)"));
        assert!(js.contains("replace(/[\\x00-\\x1f\\x7f]/g, '')"));
        assert!(!re(r"(?i)\^\\s\*\(javascript\|vbscript\|data\):").is_match(js));
        assert!(js.contains("escapeHtml(href)"));
        assert!(!js.contains("${img.mimeType}"));
        assert!(js.contains("escapeHtml(img.mimeType"));
        assert!(!js.contains(";base64,${img.data}\""));
        assert!(re(r#";base64,\$\{escapeHtml\(img\.data \|\| (?:''|"")\)\}""#).is_match(js));
        assert!(!js.contains("id=\"${entryId}\""));
        assert!(!js.contains("data-entry-id=\"${entryId}\""));
        assert!(js.contains("entry-${escapeHtml(entry.id)}"));
        assert!(js.contains("data-entry-id=\"${escapeHtml(entryId)}\""));
        assert!(js.contains("${escapeHtml(msg.toolName || 'tool')}"));
        assert!(js.contains("${escapeHtml(msg.role)}"));
        assert!(js.contains("${escapeHtml(entry.modelId)}"));
        assert!(js.contains("${escapeHtml(entry.thinkingLevel)}"));
        assert!(js.contains("${escapeHtml(entry.type)}"));
        assert!(js.contains("${escapeHtml(globalStats.models.join(', ') || 'unknown')}"));
        // Skill blocks.
        assert!(js.contains("parseSkillBlock"));
        assert!(js.contains("skillBlock.userMessage"));
        assert!(js.contains("skill-invocation"));
        assert!(js.contains("hasUserContent"));
        assert!(js.contains("safeMarkedParse(skillBlock.content)"));
        assert!(js.contains("tree-role-skill"));
        // Whitespace handling.
        let css = TEMPLATE_CSS.get();
        assert!(
            re(r"\.output-preview > div:not\(\.expand-hint\),\s*\.output-full > div:not\(\.expand-hint\) \{[\s\S]*?white-space:\s*pre-wrap;")
                .is_match(css)
        );
        assert!(re(r"\.ansi-line\s*\{[\s\S]*?white-space:\s*pre;").is_match(css));
    }
}
