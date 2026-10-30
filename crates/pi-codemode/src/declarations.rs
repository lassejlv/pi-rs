//! TypeScript declarations for model-facing tool descriptions.
//! The UTF-8 API delegates to the same lossless renderer used by durable state.
pub mod json;
use crate::types::{CodemodeJsonSchema, ToolDeclaration};
pub use json::*;
use pi_json::strict::{JsonString, Value};

/// Largest rendered input type, in UTF-16 code units, before it becomes `unknown`.
pub const DEFAULT_INPUT_SCHEMA_MAX_CHARS: usize = 16_000;

/// Script-visible tools and globals to render.
pub struct RenderDeclarationsOptions<'a, T: ToolDeclaration = crate::types::CodemodeTool, G: ToolDeclaration = crate::types::CodemodeTool> {
    pub tools: &'a [T],
    pub globals: &'a [G],
}
fn declaration(tool: &impl ToolDeclaration) -> JsonToolDeclaration {
    JsonToolDeclaration {
        name: tool.name().into(),
        description: tool.description().map(Into::into),
        input_schema: tool.input_schema().cloned().map(Value::from),
        output_schema: tool.output_schema().cloned().map(Value::from),
        signature: tool.signature().map(Into::into),
    }
}
fn utf8(text: JsonString) -> String {
    text.to_string_checked().expect("UTF-8 declaration inputs produce UTF-8 output")
}
/// Render declarations for tools and globals, preserving source formatting and ordering.
pub fn render_declarations<T: ToolDeclaration, G: ToolDeclaration>(options: RenderDeclarationsOptions<'_, T, G>) -> String {
    utf8(render_json_declarations(
        &options.tools.iter().map(declaration).collect::<Vec<_>>(),
        &options.globals.iter().map(declaration).collect::<Vec<_>>(),
    ))
}
/// Render a tool's script-visible member signature with a bounded input type.
pub fn render_tool_signature(tool: &impl ToolDeclaration, input_max_chars: Option<usize>) -> String {
    utf8(render_json_tool_signature(&declaration(tool), input_max_chars))
}
/// Render a trimmed description and its fenced tool declaration.
pub fn render_tool_sample(tool: &impl ToolDeclaration, input_max_chars: Option<usize>) -> String {
    utf8(render_json_tool_sample(&declaration(tool), input_max_chars))
}
/// Render the schema type, or `unknown` when no output schema is declared.
pub fn render_tool_output_type(schema: Option<&CodemodeJsonSchema>) -> String {
    utf8(render_json_tool_output_type(schema.cloned().map(Value::from).as_ref()))
}
/// Render a schema with source-compatible references, comments, tags and UTF-16 budgets.
pub fn schema_to_type(schema: &CodemodeJsonSchema, max_chars: Option<usize>) -> String {
    utf8(schema_to_json_type(&Value::from(schema.clone()), max_chars))
}
