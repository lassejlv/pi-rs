//! Port of `packages/coding-agent/src/extensions/tool-search/tool.ts`.
//!
//! Tool discovery: a BM25 ranker over tool metadata, shared by `searchTools()` in codemode scripts
//! and the optional `tool_search` tool.
//!
//! `tool_search` searches tools that are not declared to the model (`codemode` and `deferred`
//! exposure) and loads the matches, so they are declared for the next model call. Loading goes
//! through the active tool set, so it is recorded in the transcript like any other tool change and
//! survives `/tree`, resume, and fork on that branch.

use crate::core::extensions::types::{
    AllTools, ExtensionAPIRef, ReachesTools, ToolDefinition, ToolExposure, ToolInfo, ToolNamespace,
};
use crate::core::tools::js_util::js_trim;
use pi_agent::types::AgentToolResult;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::LazyLock;

pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

pub const DEFAULT_TOOL_SEARCH_LIMIT: i64 = 8;

/// A tool as the ranker sees it: its name and the text built by {@link createToolSearchDocument}.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSearchDocument {
    pub name: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSearchMatch {
    pub name: String,
    #[serde(with = "pi_ai::serde_util::js_f64")]
    pub score: f64,
}

/// Ranks tools for a query. BM25 today; a hybrid ranker with embeddings can replace it.
pub trait ToolRanker: Send + Sync {
    fn rank(&self, query: &str, documents: &[ToolSearchDocument], limit: i64) -> Vec<ToolSearchMatch>;
}

const STOP_WORDS: [&str; 21] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on", "or", "that",
    "the", "this", "to", "with",
];

static CAMEL_LOWER_UPPER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z0-9])([A-Z])").unwrap());
static CAMEL_ACRONYM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([A-Z]+)([A-Z][a-z])").unwrap());
static NON_ALPHANUMERIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").unwrap());
static PLURAL_ES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(ches|shes|sses|xes|zes)$").unwrap());

/// Naive singular form, so `issues` matches `issue` and `searches` matches `search`.
fn stem(term: &str) -> String {
    // Terms are ASCII (`[a-z0-9]+`), so byte lengths equal JS lengths.
    if term.len() > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..term.len() - 3]);
    }
    if term.len() > 4 && PLURAL_ES.is_match(term) {
        return term[..term.len() - 2].to_string();
    }
    if term.len() > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..term.len() - 1].to_string();
    }
    term.to_string()
}

/// Lowercase terms, split at camelCase boundaries and non-alphanumerics, without stop words.
pub fn tokenize(text: &str) -> Vec<String> {
    let spaced = CAMEL_LOWER_UPPER.replace_all(text, "$1 $2");
    let spaced = CAMEL_ACRONYM.replace_all(&spaced, "$1 $2");
    let lower = spaced.to_lowercase();
    NON_ALPHANUMERIC
        .split(&lower)
        .filter(|term| !term.is_empty() && !STOP_WORDS.contains(term))
        .map(stem)
        .collect()
}

fn is_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

/// Schema descriptions and property names, recursively.
fn schema_text(schema: &Value, parts: &mut Vec<String>) {
    let Some(schema) = is_object(schema) else { return };
    if let Some(Value::String(description)) = schema.get("description") {
        parts.push(description.clone());
    }
    if let Some(Value::Object(properties)) = schema.get("properties") {
        for (name, property) in properties {
            parts.push(name.clone());
            schema_text(property, parts);
        }
    }
    if let Some(items) = schema.get("items") {
        schema_text(items, parts);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(variants)) = schema.get(key) {
            for variant in variants {
                schema_text(variant, parts);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateToolSearchDocumentTool {
    /// Tool name (used in LLM tool calls)
    pub name: String,
    /// Description for LLM
    pub description: String,
    /// Parameter schema (TypeBox)
    pub parameters: Value,
}

impl From<&ToolInfo> for CreateToolSearchDocumentTool {
    fn from(tool: &ToolInfo) -> Self {
        Self { name: tool.name.clone(), description: tool.description.clone(), parameters: tool.parameters.clone() }
    }
}

impl From<&dyn pi_agent::types::AgentTool> for CreateToolSearchDocumentTool {
    fn from(tool: &dyn pi_agent::types::AgentTool) -> Self {
        Self {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters: tool.parameters().clone(),
        }
    }
}

/// Search text of a tool: the name, the name with `_`
/// as spaces, the description, schema descriptions and property names, and the namespace with its
/// description and instructions.
pub fn create_tool_search_document(
    tool: &CreateToolSearchDocumentTool,
    namespace: Option<&ToolNamespace>,
) -> ToolSearchDocument {
    let mut parts = vec![tool.name.clone(), tool.name.replace('_', " "), tool.description.clone()];
    schema_text(&tool.parameters, &mut parts);
    if let Some(namespace) = namespace {
        parts.push(namespace.name.clone());
        parts.push(namespace.description.clone().unwrap_or_default());
        parts.push(namespace.instructions.clone().unwrap_or_default());
    }
    ToolSearchDocument {
        name: tool.name.clone(),
        text: parts.into_iter().filter(|part| !js_trim(part).is_empty()).collect::<Vec<_>>().join(" "),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bm25RankerOptions {
    #[serde(default, skip_serializing_if = "Option::is_none", with = "pi_ai::serde_util::opt_js_f64")]
    pub k1: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "pi_ai::serde_util::opt_js_f64")]
    pub b: Option<f64>,
}

/// Okapi BM25 with the usual parameters. Ties keep document order.
/// TS: `implements ToolRanker`
#[derive(Debug, Clone)]
pub struct Bm25Ranker {
    k1: f64,
    b: f64,
}

impl Default for Bm25Ranker {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Bm25Ranker {
    pub fn new(options: Option<&Bm25RankerOptions>) -> Self {
        Self {
            k1: options.and_then(|o| o.k1).unwrap_or(1.2),
            b: options.and_then(|o| o.b).unwrap_or(0.75),
        }
    }
}

impl ToolRanker for Bm25Ranker {
    fn rank(&self, query: &str, documents: &[ToolSearchDocument], limit: i64) -> Vec<ToolSearchMatch> {
        let mut seen = HashSet::new();
        let query_terms: Vec<String> = tokenize(query).into_iter().filter(|term| seen.insert(term.clone())).collect();
        if query_terms.is_empty() || documents.is_empty() || limit <= 0 {
            return Vec::new();
        }
        let term_counts: Vec<HashMap<String, f64>> = documents
            .iter()
            .map(|document| {
                let mut counts = HashMap::new();
                for term in tokenize(&document.text) {
                    *counts.entry(term).or_insert(0.0) += 1.0;
                }
                counts
            })
            .collect();
        let lengths: Vec<f64> = term_counts.iter().map(|counts| counts.values().sum()).collect();
        let average = lengths.iter().sum::<f64>() / documents.len() as f64;
        // `|| 1`: 0 and NaN fall back to 1.
        let average_length = if average == 0.0 || average.is_nan() { 1.0 } else { average };
        let total = documents.len() as f64;
        let idf: HashMap<&str, f64> = query_terms
            .iter()
            .map(|term| {
                let frequency = term_counts.iter().filter(|counts| counts.contains_key(term)).count() as f64;
                (term.as_str(), (1.0 + (total - frequency + 0.5) / (frequency + 0.5)).ln())
            })
            .collect();
        let mut matches: Vec<ToolSearchMatch> = Vec::new();
        for (index, document) in documents.iter().enumerate() {
            let mut score = 0.0;
            for term in &query_terms {
                let Some(&count) = term_counts[index].get(term) else { continue };
                if count == 0.0 {
                    continue;
                }
                let norm = self.k1 * (1.0 - self.b + (self.b * lengths[index]) / average_length);
                score += idf.get(term.as_str()).copied().unwrap_or(0.0) * ((count * (self.k1 + 1.0)) / (count + norm));
            }
            if score > 0.0 {
                matches.push(ToolSearchMatch { name: document.name.clone(), score });
            }
        }
        // Stable sort: ties keep document order.
        matches.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        matches.truncate(limit as usize);
        matches
    }
}

pub static TOOL_SEARCH_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["query"],
        "properties": {
            "query": { "type": "string", "description": "Search query for deferred tools." },
            "limit": {
                "type": "number",
                "description": format!("Maximum number of tools to return. Defaults to {DEFAULT_TOOL_SEARCH_LIMIT}.")
            }
        }
    })
});

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSearchInput {
    /// Search query for deferred tools.
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "pi_ai::serde_util::opt_js_f64")]
    pub limit: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSearchResultTool {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSearchToolDetails {
    /// Tools loaded by this call.
    pub loaded: Vec<String>,
}

#[derive(Clone)]
pub struct ToolSearchToolOptionsTools {
    /// Get all configured tools with parameter schema, prompt guidelines, exposure, and source metadata.
    pub get_all_tools: Arc<dyn Fn() -> Vec<ToolInfo> + Send + Sync>,
    /// Get the names of the active tools, which are the tools declared to the model.
    pub get_active_tools: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    /// Set the active tools by name. Unknown and `hidden` tools are ignored. Tools with `codemode` or
    /// `deferred` exposure stay callable from codemode scripts whether active or not.
    pub set_active_tools: Arc<dyn Fn(Vec<String>) + Send + Sync>,
}

impl ToolSearchToolOptionsTools {
    /// The tools of an extension's `pi` object (TS passes the `ExtensionAPI` itself).
    pub fn from_api(pi: ExtensionAPIRef) -> Self {
        let all = pi.clone();
        let active = pi.clone();
        Self {
            get_all_tools: Arc::new(move || all.get_all_tools().unwrap_or_default()),
            get_active_tools: Arc::new(move || active.get_active_tools().unwrap_or_default()),
            set_active_tools: Arc::new(move |names| {
                let _ = pi.set_active_tools(names);
            }),
        }
    }
}

#[derive(Clone, Default)]
pub struct ToolSearchToolOptions {
    /// The session's tools. `tool_search` searches the tools that are not declared to the model and
    /// activates the matches. Without it, the tool finds nothing. An `ExtensionAPI` fits.
    pub tools: Option<ToolSearchToolOptionsTools>,
}

/// Whether `tool_search` can load a tool with this exposure.
fn is_searchable(exposure: ToolExposure) -> bool {
    matches!(exposure, ToolExposure::Codemode | ToolExposure::Deferred)
}

/// Rank the searchable tools that are not active yet and activate the matches, so the next model
/// call declares them. Activation is recorded in the transcript like any tool change.
fn search_and_load(tools: &ToolSearchToolOptionsTools, query: &str, limit: i64) -> Vec<ToolSearchResultTool> {
    let active = (tools.get_active_tools)();
    let candidates: Vec<ToolInfo> = (tools.get_all_tools)()
        .into_iter()
        .filter(|tool| is_searchable(tool.exposure) && !active.contains(&tool.name))
        .collect();
    let documents: Vec<ToolSearchDocument> = candidates
        .iter()
        .map(|tool| create_tool_search_document(&tool.into(), tool.namespace.as_ref()))
        .collect();
    let matches = Bm25Ranker::new(None).rank(query, &documents, limit);
    if !matches.is_empty() {
        let mut names = active.clone();
        names.extend(matches.iter().map(|m| m.name.clone()));
        (tools.set_active_tools)(names);
    }
    matches
        .into_iter()
        .map(|m| ToolSearchResultTool {
            description: candidates
                .iter()
                .find(|tool| tool.name == m.name)
                .map(|tool| tool.description.clone())
                .unwrap_or_default(),
            name: m.name,
        })
        .collect()
}

/// The `tool_search` description. It does not list the searchable tools or their namespaces, so it
/// stays the same while tools are registered, for example when MCP servers connect.
pub static TOOL_SEARCH_DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nSome of the tools, such as tools of MCP servers, may not have been provided to you upfront, and you should use this tool (`{TOOL_SEARCH_TOOL_NAME}`) to search for the required tools. For MCP tool discovery, always use `{TOOL_SEARCH_TOOL_NAME}`."
    )
});

/// First line of a description, trimmed (`description.trim().split(/\r?\n/)[0]`).
fn first_line(description: &str) -> &str {
    let trimmed = js_trim(description);
    let line = trimmed.split('\n').next().unwrap_or("");
    line.strip_suffix('\r').unwrap_or(line)
}

fn run_tool_search(options: &ToolSearchToolOptions, params: &Value) -> anyhow::Result<AgentToolResult> {
    let query = match params.get("query") {
        Some(Value::String(query)) => query.clone(),
        _ => anyhow::bail!("query must not be empty"),
    };
    if js_trim(&query).is_empty() {
        anyhow::bail!("query must not be empty");
    }
    let max = match params.get("limit") {
        None | Some(Value::Null) => DEFAULT_TOOL_SEARCH_LIMIT as f64,
        Some(value) => value.as_f64().unwrap_or(f64::NAN),
    };
    if !(max.is_finite() && max.fract() == 0.0) || max <= 0.0 {
        anyhow::bail!("limit must be a positive integer");
    }
    let limit = if max > i64::MAX as f64 { i64::MAX } else { max as i64 };
    let tools = match &options.tools {
        Some(tools) => search_and_load(tools, &query, limit),
        None => Vec::new(),
    };
    let text = if tools.is_empty() {
        "No matching tools found.".to_string()
    } else {
        format!(
            "Loaded {} tool{}. They are available from your next call:\n{}",
            tools.len(),
            if tools.len() == 1 { "" } else { "s" },
            tools
                .iter()
                .map(|tool| format!("- {}: {}", tool.name, first_line(&tool.description)))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let details = ToolSearchToolDetails { loaded: tools.into_iter().map(|tool| tool.name).collect() };
    Ok(AgentToolResult::text(text, Some(serde_json::to_value(details)?)))
}

pub fn create_tool_search_tool_definition(options: Option<&ToolSearchToolOptions>) -> anyhow::Result<ToolDefinition> {
    let options = options.cloned().unwrap_or_default();
    let mut definition = ToolDefinition::new(
        TOOL_SEARCH_TOOL_NAME,
        TOOL_SEARCH_TOOL_NAME,
        TOOL_SEARCH_DESCRIPTION.clone(),
        TOOL_SEARCH_SCHEMA.clone(),
        move |_tool_call_id, params, _signal, _on_update, _ctx| {
            let result = run_tool_search(&options, &params);
            async move { result }
        },
    );
    // TS builds the schema with TypeBox.
    definition.parameters_typebox = true;
    definition.prompt_snippet = Some("Search for tools that are not loaded yet and load the matches".into());
    // Searching is not something scripts need; it changes what the model sees.
    definition.exposure = Some(ToolExposure::ModelOnly);
    // A query can match any tool that is not declared yet.
    definition.reaches_tools = Some(Arc::new(|_input| ReachesTools::All(AllTools::All)));
    Ok(definition)
}
