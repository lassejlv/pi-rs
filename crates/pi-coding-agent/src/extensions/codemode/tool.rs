//! Port of `packages/coding-agent/src/extensions/codemode/tool.ts`.

use crate::core::extensions::types::{
    AllTools, ReachesTools, ToolDefinition, ToolExposure, ToolLoadout, ToolLoadoutChanges, ToolNamespace,
};
use crate::core::model_registry::ModelRegistry;
use crate::core::settings_schema::CodemodeMode;
use crate::core::tools::js_util::{js_trim, locale_compare, utf16_len};
use crate::core::tools::tool_definition_wrapper::wrap_tool_definition;
use crate::extensions::codemode::execute::execute_codemode;
use crate::extensions::codemode::renderer::codemode_renderers;
use indexmap::{IndexMap, IndexSet};
use pi_agent::types::{AgentTool, AgentToolRef};
use pi_ai::types::{ConstrainedSamplingConfig, GrammarFormat, JsonObject};
use pi_codemode::{
    CODEMODE_SOURCE_GRAMMAR, CodemodeToolDeclaration, render_tool_output_type, render_tool_sample,
    to_codemode_identifier,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::LazyLock;

pub const CODEMODE_TOOL_NAME: &str = "codemode";

/// Custom entry type holding one script's `store()` writes: {@link CodemodeStoreEntryData}.
pub const CODEMODE_STORE_ENTRY_TYPE: &str = "codemode-store";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeStoreEntryData {
    pub set: IndexMap<String, Value>,
    pub delete: Vec<String>,
}

/// The part of the model registry that scripts reach through `models`.
/// TS: Pick< ModelRegistry, "getModelsOfType" | "getAvailableOfType" | "getModelOfType" | "classify" | "generateImages" >
pub type CodemodeModelRuntime = Arc<ModelRegistry>;

#[derive(Clone, Default)]
pub struct CodemodeToolOptions {
    /// Namespace of a tool, for `searchTools()` ranking and its `namespace` filter.
    pub get_tool_namespace: Option<Arc<dyn Fn(String) -> Option<ToolNamespace> + Send + Sync>>,
    /// Prompt guidelines of every tool, by tool name, shown with declarations by `describeTool()` and `ALL_TOOLS`.
    pub get_tool_guidelines: Option<Arc<dyn Fn() -> IndexMap<String, Vec<String>> + Send + Sync>>,
    /// Expose the `models` namespace to scripts, backed by the session's model registry
    /// (`ctx.modelRegistry`). Without it, `models` is not declared.
    pub models: Option<bool>,
    /// Persists `store()` writes as a session custom entry. Without it, writes last only for the
    /// current script; `load()` still reads entries already on the branch.
    pub append_entry: Option<Arc<dyn Fn(String, CodemodeStoreEntryData) + Send + Sync>>,
    /// How the tool presents the loadout while active (the `codemode.mode` setting). Default: `on`.
    pub get_mode: Option<Arc<dyn Fn() -> CodemodeMode + Send + Sync>>,
    /// Token budget for tool declarations in the description. Default: {@link DEFAULT_CODEMODE_INLINE_BUDGET}.
    pub get_inline_budget: Option<Arc<dyn Fn() -> Option<f64> + Send + Sync>>,
}

pub static CODEMODE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["code"],
        "properties": { "code": { "type": "string", "description": "Raw JavaScript source." } }
    })
});

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeToolInput {
    /// Raw JavaScript source.
    pub code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CodemodeNestedCallStatus {
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "cancelled")]
    Cancelled,
}

impl CodemodeNestedCallStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeNestedCall {
    /// Tool call id of the nested call, `<codemode call id>/<n>`.
    pub id: String,
    pub name: String,
    /// Compact JSON of the arguments, truncated for display.
    pub args: String,
    pub status: CodemodeNestedCallStatus,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub duration_ms: Option<f64>,
    /// Error text, truncated for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Cost in USD of a `models.*` call that reported usage.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeToolDetails {
    pub calls: Vec<CodemodeNestedCall>,
    /// Temp file with the full text output, when the output was truncated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}


/// What the system prompt says about the tool (`codemodeToolSystemPromptContribution`).
pub const CODEMODE_TOOL_SNIPPET: &str = "Run JavaScript that calls other tools";
pub const CODEMODE_TOOL_GUIDELINES: [&str; 1] = [
    "Use codemode to batch independent tool calls (Promise.allSettled), chain them, or filter large output, instead of many separate calls.",
];

pub static CODEMODE_TOOL_SYSTEM_PROMPT_CONTRIBUTION: LazyLock<Value> =
    LazyLock::new(|| json!({ "snippet": CODEMODE_TOOL_SNIPPET, "guidelines": CODEMODE_TOOL_GUIDELINES }));

/// The reference for scripts: globals, tool results, `store()`, the `models` API, and limits.
pub static CODEMODE_DOCS_PATH: LazyLock<String> = LazyLock::new(|| {
    std::path::Path::new(&crate::config::get_docs_path()).join("codemode.md").to_string_lossy().into_owned()
});

const DESCRIPTION_INTRO: &str = "Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.
- `await tools.<name>({ ...args })` resolves to a string, or an object if the tool's declaration says so, and rejects with an Error on failure. A tool whose declaration returns an object resolves on failure too when it returned that object; check its fields. Calls still running when the script ends are cancelled.
- Optional first line: `// @options: {\"max_output_tokens\": 10000, \"timeout_ms\": 60000}`";

/// One line per global. The details live in {@link CODEMODE_DOCS_PATH}.
fn describe_globals(models: bool) -> String {
    let mut lines = vec![
        "Globals:".to_string(),
        "- `text(value)`, `image(dataUrlOrImageBlock)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script. With several text items, each starts with a `==> text N/M <==` line, and `console` lines follow the other output in one `<console_output>` block. `image()` also saves the image to a temp file and the result names its path.".to_string(),
        "- `store(key, value)` and `load(key)` keep JSON values across codemode calls.".to_string(),
        "- `ALL_TOOLS`, `await searchTools(query, { limit?, namespace? })`, `await describeTool(name)`, `await describeNamespace(name)`: find unlisted tools, such as MCP tools.".to_string(),
    ];
    if models {
        lines.push(format!("- `models`: classifiers and image generation. Read {} first.", *CODEMODE_DOCS_PATH));
    }
    lines.join("\n")
}

/// Default for {@link CodemodeDescriptionOptions.inlineBudget}, in estimated tokens.
pub const DEFAULT_CODEMODE_INLINE_BUDGET: i64 = crate::core::settings_defaults::codemode::INLINE_BUDGET as i64;
/// Characters per token when estimating the cost of a tool section.
const CHARS_PER_TOKEN: f64 = 4.0;

/// What a script sees of a tool (`Omit<CodemodeTool, "execute">`).
pub type ToCodemodeDeclarationResult = CodemodeToolDeclaration;

fn text_output_schema() -> Value {
    json!({ "type": "string" })
}

/// What a script sees of a tool: its description followed by its prompt guidelines, which the system
/// prompt only has for declared tools. Tools without an output schema resolve to their text output.
pub fn to_codemode_declaration(tool: &dyn AgentTool, guidelines: Option<&[String]>) -> ToCodemodeDeclarationResult {
    let bullets: Vec<String> = guidelines
        .unwrap_or_default()
        .iter()
        .filter(|guideline| !js_trim(guideline).is_empty())
        .map(|guideline| format!("- {}", js_trim(guideline)))
        .collect();
    let description = if bullets.is_empty() {
        tool.description().to_string()
    } else {
        format!("{}\n\n{}", js_trim(tool.description()), bullets.join("\n"))
    };
    CodemodeToolDeclaration {
        name: tool.name().to_string(),
        description: Some(description),
        input_schema: Some(tool.parameters().clone()),
        output_schema: Some(tool.output_schema().cloned().unwrap_or_else(text_output_schema)),
        spread: false,
        signature: None,
    }
}

/// Tools a script may call: every given tool except the codemode tool itself.
pub fn get_codemode_callable_tools(tools: &[AgentToolRef]) -> Vec<AgentToolRef> {
    tools.iter().filter(|tool| tool.name() != CODEMODE_TOOL_NAME).cloned().collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeDescriptionOptions {
    /// Declare the `models` namespace; only for tools created with model access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<bool>,
    /// Namespace of each tool, by tool name. Tools of one namespace are listed under one heading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespaces: Option<IndexMap<String, ToolNamespace>>,
    /// Tools that are callable but never listed with their declaration (`deferred` exposure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<IndexSet<String>>,
    /// Prompt guidelines of each tool, by tool name, listed after its description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidelines: Option<IndexMap<String, Vec<String>>>,
    /// Estimated tokens (characters / 4) the tool sections may use. Tools that do not fit are left
    /// out, like deferred tools. Unset lists every tool that is not deferred.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "pi_ai::serde_util::opt_js_f64")]
    pub inline_budget: Option<f64>,
}

/// `### \`id\` (\`raw name\`)` followed by the tool's description and declaration.
fn render_tool_section(declaration: &CodemodeToolDeclaration) -> String {
    let id = to_codemode_identifier(&declaration.name);
    let heading =
        if id == declaration.name { format!("### `{id}`") } else { format!("### `{id}` (`{}`)", declaration.name) };
    format!("{heading}\n{}", js_trim(&render_tool_sample(declaration, None)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub name: String,
    pub section: String,
    #[serde(with = "pi_ai::serde_util::js_f64")]
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogGroup {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<ToolNamespace>,
    pub entries: Vec<CatalogEntry>,
}

/// Pick the tool sections that fit the budget, like OpenCode's catalog: in each round every group
/// (tools without a namespace first, then namespaces by name) places its cheapest remaining tool; a
/// group whose next tool does not fit drops out while the others continue. Every namespace is
/// represented before any namespace is complete.
fn select_catalog(groups: &[CatalogGroup], budget: Option<f64>) -> HashSet<String> {
    let Some(budget) = budget else {
        return groups.iter().flat_map(|group| group.entries.iter().map(|entry| entry.name.clone())).collect();
    };
    let mut active: Vec<VecDeque<&CatalogEntry>> = groups
        .iter()
        .map(|group| {
            let mut entries: Vec<&CatalogEntry> = group.entries.iter().collect();
            entries.sort_by(|a, b| a.cost.partial_cmp(&b.cost).unwrap_or(std::cmp::Ordering::Equal));
            VecDeque::from(entries)
        })
        .filter(|queue| !queue.is_empty())
        .collect();
    let mut shown = HashSet::new();
    let mut remaining = budget;
    while !active.is_empty() {
        active.retain_mut(|queue| {
            let next = queue[0];
            if next.cost > remaining {
                return false;
            }
            remaining -= next.cost;
            shown.insert(next.name.clone());
            queue.pop_front();
            !queue.is_empty()
        });
    }
    shown
}

/// Model-facing description: the helper list, guidance for finding tools that are not listed, the
/// `models` API, and one section per listed tool, grouped by namespace. Deferred tools are never
/// listed and do not affect the description at all, so it stays the same while tools are registered
/// late (for example by MCP servers that connect). Tool sections are limited to `inlineBudget`.
pub fn create_codemode_description(tools: &[AgentToolRef], options: Option<&CodemodeDescriptionOptions>) -> String {
    let default_options = CodemodeDescriptionOptions::default();
    let options = options.unwrap_or(&default_options);
    let declarations: Vec<CodemodeToolDeclaration> = get_codemode_callable_tools(tools)
        .iter()
        .filter(|tool| !options.deferred.as_ref().is_some_and(|deferred| deferred.contains(tool.name())))
        .map(|tool| {
            to_codemode_declaration(
                tool.as_ref(),
                options.guidelines.as_ref().and_then(|g| g.get(tool.name())).map(Vec::as_slice),
            )
        })
        .collect();
    let mut groups: IndexMap<String, CatalogGroup> =
        IndexMap::from([(String::new(), CatalogGroup { namespace: None, entries: Vec::new() })]);
    for declaration in &declarations {
        let namespace = options.namespaces.as_ref().and_then(|n| n.get(&declaration.name)).cloned();
        let key = namespace.as_ref().map(|n| format!("ns:{}", n.name)).unwrap_or_default();
        let group = groups.entry(key).or_insert_with(|| CatalogGroup { namespace, entries: Vec::new() });
        let section = render_tool_section(declaration);
        let cost = (utf16_len(&section) as f64 / CHARS_PER_TOKEN).ceil();
        group.entries.push(CatalogEntry { name: declaration.name.clone(), section, cost });
    }
    let mut ordered: Vec<CatalogGroup> = groups.into_values().collect();
    ordered.sort_by(|a, b| match (&a.namespace, &b.namespace) {
        (None, _) => std::cmp::Ordering::Less,
        (_, None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => locale_compare(&a.name, &b.name),
    });
    let shown = select_catalog(&ordered, options.inline_budget);

    let mut sections = vec![DESCRIPTION_INTRO.to_string(), describe_globals(options.models == Some(true))];
    if declarations.is_empty() {
        return sections.join("\n\n");
    }

    let mut tool_sections = vec!["Nested tools:".to_string()];
    for group in &ordered {
        let visible: Vec<&CatalogEntry> = group.entries.iter().filter(|entry| shown.contains(&entry.name)).collect();
        if let Some(namespace) = &group.namespace {
            // Only tools that did not fit the budget are counted as not listed here.
            let listing = if visible.len() == group.entries.len() {
                ""
            } else if visible.is_empty() {
                " (tools not listed)"
            } else {
                " (some tools not listed)"
            };
            let description = namespace.description.as_deref().map(js_trim).filter(|d| !d.is_empty());
            tool_sections.push(format!(
                "## {}{listing}{}",
                namespace.name,
                description.map(|d| format!("\n{d}")).unwrap_or_default()
            ));
        }
        for entry in visible {
            tool_sections.push(entry.section.clone());
        }
    }
    sections.push(tool_sections.join("\n\n"));
    sections.join("\n\n")
}

/// The field names of an object schema, `{ output, exit_code, full_output_path? }`, with the fields of
/// object-typed fields `depth` levels deep: `{ content, structuredContent?: { results } }`. `None`
/// for other schemas.
fn describe_fields(schema: &Value, depth: usize) -> Option<String> {
    let schema = schema.as_object()?;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return None;
    }
    let properties = schema.get("properties")?.as_object()?;
    let required: HashSet<&str> = match schema.get("required") {
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => HashSet::new(),
    };
    let fields: Vec<String> = properties
        .iter()
        .map(|(name, field)| {
            let nested = if depth > 0 { describe_fields(field, depth - 1) } else { None };
            format!(
                "{name}{}{}",
                if required.contains(name.as_str()) { "" } else { "?" },
                nested.map(|n| format!(": {n}")).unwrap_or_default()
            )
        })
        .collect();
    Some(format!("{{ {} }}", fields.join(", ")))
}

static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// What a script call resolves to, in one line: `a string`, the field names of an object (see
/// {@link describeFields}), or the rendered type for anything else.
fn describe_output(schema: Option<&Value>) -> String {
    let rendered = render_tool_output_type(schema);
    if rendered == "string" {
        return "a string".to_string();
    }
    let fields = schema.and_then(|schema| describe_fields(schema, 1));
    format!("`{}`", fields.unwrap_or_else(|| WHITESPACE.replace_all(&rendered, " ").into_owned()))
}

/// A declared tool's description followed by how scripts call it and what the call resolves to. The
/// arguments are the tool's declared parameters, so they are not repeated.
fn describe_script_call(tool: &dyn AgentTool) -> String {
    format!(
        "{}\n\nCodemode: `tools.{}(args)` resolves to {}.",
        js_trim(tool.description()),
        to_codemode_identifier(tool.name()),
        describe_output(to_codemode_declaration(tool, None).output_schema.as_ref())
    )
}

fn current_mode(options: &CodemodeToolOptions) -> CodemodeMode {
    options.get_mode.as_ref().map(|get| get()).unwrap_or(CodemodeMode::On)
}

/// How the codemode tool presents tools that are both declared and callable from scripts:
/// - `on`: their descriptions say how scripts call them, and the codemode description
///   lists only the callable tools without `direct` exposure.
/// - `only`: the codemode description lists every callable tool, and requests leave out the
///   declarations of active `direct` tools.
///
/// Listed tools carry their prompt guidelines, which the system prompt only has for declared tools.
///
/// Listing by exposure, not by the active set, keeps the codemode description unchanged when
/// `tool_search` loads a tool, so loads do not redeclare codemode.
fn prepare_codemode_loadout(loadout: &dyn ToolLoadout, options: &CodemodeToolOptions) -> ToolLoadoutChanges {
    let mode = current_mode(options);
    let is_direct = |tool: &AgentToolRef| loadout.get_exposure(tool.name()) == ToolExposure::Direct;
    let callable = get_codemode_callable_tools(&loadout.callable());
    let callable_names: HashSet<String> = callable.iter().map(|tool| tool.name().to_string()).collect();
    let declared = loadout.declared();
    let mut descriptions: IndexMap<String, String> = IndexMap::new();
    if mode == CodemodeMode::On {
        for tool in &declared {
            if callable_names.contains(tool.name()) {
                descriptions.insert(tool.name().to_string(), describe_script_call(tool.as_ref()));
            }
        }
    }
    let listed: Vec<AgentToolRef> = if mode == CodemodeMode::Only {
        callable.clone()
    } else {
        callable.iter().filter(|tool| !is_direct(tool)).cloned().collect()
    };
    let namespaces: IndexMap<String, ToolNamespace> = listed
        .iter()
        .filter_map(|tool| loadout.get_namespace(tool.name()).map(|ns| (tool.name().to_string(), ns)))
        .collect();
    let guidelines: IndexMap<String, Vec<String>> = listed
        .iter()
        .map(|tool| (tool.name().to_string(), loadout.get_prompt_guidelines(tool.name())))
        .collect();
    let deferred: IndexSet<String> = listed
        .iter()
        .filter(|tool| loadout.get_exposure(tool.name()) == ToolExposure::Deferred)
        .map(|tool| tool.name().to_string())
        .collect();
    let inline_budget = options
        .get_inline_budget
        .as_ref()
        .and_then(|get| get())
        .unwrap_or(DEFAULT_CODEMODE_INLINE_BUDGET as f64);
    descriptions.insert(
        CODEMODE_TOOL_NAME.to_string(),
        create_codemode_description(
            &listed,
            Some(&CodemodeDescriptionOptions {
                models: Some(options.models == Some(true)),
                namespaces: Some(namespaces),
                deferred: Some(deferred),
                guidelines: Some(guidelines),
                inline_budget: Some(inline_budget),
            }),
        ),
    );
    let declared_names: HashSet<String> = declared.iter().map(|tool| tool.name().to_string()).collect();
    let hidden_declarations = if mode == CodemodeMode::Only {
        callable
            .iter()
            .filter(|tool| is_direct(tool) && declared_names.contains(tool.name()))
            .map(|tool| tool.name().to_string())
            .collect()
    } else {
        Vec::new()
    };
    ToolLoadoutChanges { descriptions: Some(descriptions), hidden_declarations: Some(hidden_declarations) }
}

/// Script globals that list or look up tools and namespaces, which the script may then call by any name.
static DISCOVERY_GLOBALS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(searchTools|describeNamespace|describeTool|ALL_TOOLS)\b").unwrap());
static WORDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9_]+").unwrap());

/// The tools a script may call (`ToolDefinition.reachesTools`): every tool when it uses a discovery
/// global, else the words of its source, so `tools.mcp__docs__search` and
/// `tools["mcp__docs__" + name]` both name the `mcp__docs` namespace.
fn get_script_reach(code: Option<&Value>) -> ReachesTools {
    let Some(Value::String(code)) = code else {
        return ReachesTools::Words(Vec::new());
    };
    if DISCOVERY_GLOBALS.is_match(code) {
        return ReachesTools::All(AllTools::All);
    }
    ReachesTools::Words(WORDS.find_iter(code).map(|m| m.as_str().to_string()).collect())
}

pub fn create_codemode_tool_definition(options: Option<&CodemodeToolOptions>) -> ToolDefinition {
    let options = options.cloned().unwrap_or_default();
    let models = options.models == Some(true);
    let execute_options = options.clone();
    let mut definition = ToolDefinition::new(
        CODEMODE_TOOL_NAME,
        CODEMODE_TOOL_NAME,
        // Replaced with the declarations of the callable tools when the tool is activated.
        create_codemode_description(
            &[],
            Some(&CodemodeDescriptionOptions { models: Some(models), ..Default::default() }),
        ),
        CODEMODE_SCHEMA.clone(),
        move |tool_call_id, params, signal, on_update, ctx| {
            let options = execute_options.clone();
            async move {
                let input: CodemodeToolInput = serde_json::from_value(params)
                    .map_err(|error| anyhow::anyhow!("Invalid codemode input: {error}"))?;
                execute_codemode(&tool_call_id, &input, signal, on_update, ctx.as_ref(), Some(&options)).await
            }
        },
    );
    // TS builds the schema with TypeBox.
    definition.parameters_typebox = true;
    definition.prompt_snippet = Some(CODEMODE_TOOL_SNIPPET.to_string());
    definition.prompt_guidelines = Some(CODEMODE_TOOL_GUIDELINES.iter().map(|g| g.to_string()).collect());
    // Scripts must not start other scripts.
    definition.exposure = Some(ToolExposure::ModelOnly);
    let loadout_options = options.clone();
    definition.prepare_loadout =
        Some(Arc::new(move |loadout: &dyn ToolLoadout| Some(prepare_codemode_loadout(loadout, &loadout_options))));
    definition.reaches_tools = Some(Arc::new(|input: &JsonObject| get_script_reach(input.get("code"))));
    // Capable models write the script as raw text instead of a JSON-escaped string.
    definition.constrained_sampling = Some(ConstrainedSamplingConfig::Grammar {
        variants: [(GrammarFormat::OpenaiLark, CODEMODE_SOURCE_GRAMMAR.to_string())].into_iter().collect(),
    });
    let renderers = codemode_renderers();
    definition.render_call = renderers.render_call;
    definition.render_result = renderers.render_result;
    definition
}

/// Create the codemode tool as an AgentTool. The description lists the given tools; the script can
/// call whatever tools the agent loop provides at execution time.
pub fn create_codemode_tool(tools: Option<&[AgentToolRef]>, options: Option<&CodemodeToolOptions>) -> AgentToolRef {
    let mut definition = create_codemode_tool_definition(options);
    definition.description = create_codemode_description(
        tools.unwrap_or_default(),
        Some(&CodemodeDescriptionOptions {
            models: Some(options.is_some_and(|o| o.models == Some(true))),
            ..Default::default()
        }),
    );
    wrap_tool_definition(&definition, None)
}
