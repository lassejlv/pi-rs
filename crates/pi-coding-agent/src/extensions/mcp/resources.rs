//! Port of `packages/coding-agent/src/extensions/mcp/resources.ts`.
//!
//! MCP resources, through the tools Codex and opencode use: `list_mcp_resources`,
//! `list_mcp_resource_templates`, and `read_mcp_resource`. They take a `server` argument and cover
//! every connected server with resources, so models trained on those tools use them unchanged.
//!
//! Listings are JSON, as in Codex: `{ server?, resources: [{ server, ...resource }], nextCursor? }`.
//! With a `server`, one page is listed and `cursor` continues it; without, every page of every server.
//! MCP App resources (`ui://` URIs and `profile=mcp-app` HTML) are left out, since they are user
//! interfaces for hosts that render them, and so are icons. Read resources become text and images for
//! the model; binary resources are saved to temp files. Scripts get the JSON payloads.

use crate::core::extensions::types::ToolAnnotations;
use crate::core::extensions::types::ToolDefinition;
use crate::core::mcp_servers::McpExposure;
pub use crate::core::mcp_servers::{
    LIST_MCP_RESOURCE_TEMPLATES_TOOL, LIST_MCP_RESOURCES_TOOL, READ_MCP_RESOURCE_TOOL,
};
use crate::extensions::mcp::tools::{
    McpToolDetails, limit_mcp_content, to_model_content, to_tool_exposure,
};
use futures::future::BoxFuture;
use pi_agent::types::AgentToolResult;
use pi_ai::types::UserContentBlock;
use pi_mcp::client::McpRequestOptions;
use pi_mcp::protocol::content::{ContentBlock, EmbeddedResourceContent};
use pi_mcp::protocol::types::ListResourceTemplatesResult;
use pi_mcp::protocol::types::ListResourcesResult;
use pi_mcp::protocol::types::ReadResourceResult;
use pi_mcp::protocol::types::Resource;
use pi_mcp::protocol::types::ResourceTemplate;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::{Arc, LazyLock};
use tokio_util::sync::CancellationToken;

/// A connected server that offers resources.
#[async_trait::async_trait]
pub trait McpResourceServer: Send + Sync {
    fn name(&self) -> String;
    fn timeout_ms(&self) -> i64;
    async fn resources_page(
        &self,
        cursor: Option<&str>,
        options: McpRequestOptions,
    ) -> anyhow::Result<ListResourcesResult>;
    async fn resource_templates_page(
        &self,
        cursor: Option<&str>,
        options: McpRequestOptions,
    ) -> anyhow::Result<ListResourceTemplatesResult>;
    async fn all_resources(&self, options: McpRequestOptions) -> anyhow::Result<Vec<Resource>>;
    async fn all_resource_templates(
        &self,
        options: McpRequestOptions,
    ) -> anyhow::Result<Vec<ResourceTemplate>>;
    async fn read_resource(
        &self,
        uri: &str,
        options: McpRequestOptions,
    ) -> anyhow::Result<ReadResourceResult>;
}

static MCP_APP_MIME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i);\s*profile\s*=\s*"?mcp-app"?"#).expect("valid regex"));

/// MCP App user interfaces, which only hosts that render them can use.
fn is_mcp_app_resource(item: &Map<String, Value>) -> bool {
    let uri = item
        .get("uri")
        .and_then(Value::as_str)
        .or_else(|| item.get("uriTemplate").and_then(Value::as_str))
        .unwrap_or("");
    let mime_type = item.get("mimeType").and_then(Value::as_str).unwrap_or("");
    uri.starts_with("ui://") || MCP_APP_MIME.is_match(mime_type)
}

/// A listed resource or template without `_meta` and icons, tagged with its server.
fn listed(server: &str, mut item: Map<String, Value>) -> Value {
    item.shift_remove("_meta");
    item.shift_remove("icons");
    let mut tagged = Map::new();
    tagged.insert("server".into(), Value::String(server.to_string()));
    tagged.extend(item);
    Value::Object(tagged)
}

fn string_property(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn list_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": string_property("MCP server name. Omit to list every server with resources."),
            "cursor": string_property(
                "Opaque cursor from a previous call with the same server; omit for the first page."
            ),
        },
        "additionalProperties": false,
    })
}

fn read_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": string_property(
                "MCP server name exactly as configured. Must match the 'server' field returned by list_mcp_resources."
            ),
            "uri": string_property(
                "Resource URI to read. Must be one of the URIs returned by list_mcp_resources."
            ),
        },
        "required": ["server", "uri"],
        "additionalProperties": false,
    })
}

fn listing_errors() -> Value {
    json!({
        "type": "array",
        "description": "Servers that could not be listed",
        "items": {
            "type": "object",
            "properties": { "server": { "type": "string" }, "error": { "type": "string" } },
            "required": ["server", "error"],
        },
    })
}

fn list_output_schema() -> Value {
    let optional_string = json!({ "type": "string" });
    json!({
        "type": "object",
        "properties": {
            "server": optional_string,
            "resources": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "server": { "type": "string" },
                        "uri": { "type": "string" },
                        "name": { "type": "string" },
                        "title": optional_string,
                        "description": optional_string,
                        "mimeType": optional_string,
                        "size": { "type": "number" },
                    },
                    "required": ["server", "uri", "name"],
                },
            },
            "nextCursor": optional_string,
            "errors": listing_errors(),
        },
        "required": ["resources"],
    })
}

fn list_templates_output_schema() -> Value {
    let optional_string = json!({ "type": "string" });
    json!({
        "type": "object",
        "properties": {
            "server": optional_string,
            "resourceTemplates": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "server": { "type": "string" },
                        "uriTemplate": { "type": "string", "description": "RFC 6570 URI template" },
                        "name": { "type": "string" },
                        "title": optional_string,
                        "description": optional_string,
                        "mimeType": optional_string,
                    },
                    "required": ["server", "uriTemplate", "name"],
                },
            },
            "nextCursor": optional_string,
            "errors": listing_errors(),
        },
        "required": ["resourceTemplates"],
    })
}

fn read_output_schema() -> Value {
    let optional_string = json!({ "type": "string" });
    json!({
        "type": "object",
        "properties": {
            "server": { "type": "string" },
            "uri": { "type": "string" },
            "contents": {
                "type": "array",
                "items": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {
                                "uri": { "type": "string" },
                                "mimeType": optional_string,
                                "text": { "type": "string" },
                            },
                            "required": ["uri", "text"],
                        },
                        {
                            "type": "object",
                            "properties": {
                                "uri": { "type": "string" },
                                "mimeType": optional_string,
                                "blob": { "type": "string", "description": "base64" },
                            },
                            "required": ["uri", "blob"],
                        },
                    ],
                },
            },
        },
        "required": ["server", "uri", "contents"],
    })
}

fn string_argument(params: &Value, key: &str) -> anyhow::Result<Option<String>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            let trimmed = value.trim();
            Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
        }
        Some(_) => anyhow::bail!("{key} must be a string"),
    }
}

async fn json_result(
    tool: &str,
    server: Option<String>,
    payload: Value,
) -> anyhow::Result<AgentToolResult> {
    let text = serde_json::to_string(&payload)?;
    let limited = limit_mcp_content(&[UserContentBlock::text(text)], None).await?;
    let details = McpToolDetails {
        server: server.unwrap_or_default(),
        tool: tool.to_string(),
        full_output_path: limited.full_output_path,
    };
    Ok(AgentToolResult {
        content: limited.content,
        details: Some(details.to_value()),
        structured_content: Some(payload),
        ..Default::default()
    })
}

#[derive(Clone)]
pub struct CreateMcpResourceToolDefinitionsOptions {
    pub exposure: McpExposure,
    pub servers: Arc<dyn Fn() -> Vec<Arc<dyn McpResourceServer>> + Send + Sync>,
}

type ServersFn = Arc<dyn Fn() -> Vec<Arc<dyn McpResourceServer>> + Send + Sync>;

fn find_server(servers: &ServersFn, name: &str) -> anyhow::Result<Arc<dyn McpResourceServer>> {
    let servers = servers();
    if let Some(server) = servers.iter().find(|candidate| candidate.name() == name) {
        return Ok(server.clone());
    }
    let available = servers
        .iter()
        .map(|candidate| candidate.name())
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if available.is_empty() {
        String::new()
    } else {
        format!(". Servers with resources: {available}")
    };
    anyhow::bail!("MCP server \"{name}\" has no resources{suffix}")
}

fn request_options(signal: &Option<CancellationToken>, timeout_ms: i64) -> McpRequestOptions {
    McpRequestOptions {
        signal: signal.clone().map(pi_mcp::AbortSignal::from),
        timeout_ms: Some(timeout_ms.max(0) as u64),
        on_progress: None,
    }
}

type PageFn = Arc<
    dyn Fn(
            Arc<dyn McpResourceServer>,
            Option<String>,
            McpRequestOptions,
        ) -> BoxFuture<'static, anyhow::Result<(Vec<Value>, Option<String>)>>
        + Send
        + Sync,
>;
type AllFn = Arc<
    dyn Fn(Arc<dyn McpResourceServer>, McpRequestOptions) -> BoxFuture<'static, anyhow::Result<Vec<Value>>>
        + Send
        + Sync,
>;

fn as_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(object) => object,
        _ => Map::new(),
    }
}

/// One page of one server, or every page of every server.
async fn list(
    servers: &ServersFn,
    params: &Value,
    signal: Option<CancellationToken>,
    key: &str,
    page: PageFn,
    all: AllFn,
) -> anyhow::Result<Value> {
    let server_name = string_argument(params, "server")?;
    let cursor = string_argument(params, "cursor")?;
    if let Some(server_name) = server_name {
        let server = find_server(servers, &server_name)?;
        let name = server.name();
        let options = request_options(&signal, server.timeout_ms());
        let (items, next_cursor) = page(server, cursor, options).await?;
        let listed_items: Vec<Value> = items
            .into_iter()
            .map(as_object)
            .filter(|item| !is_mcp_app_resource(item))
            .map(|item| listed(&name, item))
            .collect();
        let mut payload = Map::new();
        payload.insert("server".into(), Value::String(name));
        payload.insert(key.into(), Value::Array(listed_items));
        if let Some(next_cursor) = next_cursor {
            payload.insert("nextCursor".into(), Value::String(next_cursor));
        }
        return Ok(Value::Object(payload));
    }
    if cursor.is_some() {
        anyhow::bail!("cursor can only be used when a server is specified");
    }
    let mut sorted = servers();
    sorted.sort_by(|a, b| super::locale_compare(&a.name(), &b.name()));
    let results = futures::future::join_all(sorted.iter().map(|server| {
        let options = request_options(&signal, server.timeout_ms());
        all(server.clone(), options)
    }))
    .await;
    let mut items = Vec::new();
    let mut errors = Vec::new();
    for (server, result) in sorted.iter().zip(results) {
        let name = server.name();
        match result {
            Ok(values) => items.extend(
                values
                    .into_iter()
                    .map(as_object)
                    .filter(|item| !is_mcp_app_resource(item))
                    .map(|item| listed(&name, item)),
            ),
            Err(error) => errors.push(json!({ "server": name, "error": error.to_string() })),
        }
    }
    let mut payload = Map::new();
    payload.insert(key.into(), Value::Array(items));
    if !errors.is_empty() {
        payload.insert("errors".into(), Value::Array(errors));
    }
    Ok(Value::Object(payload))
}

fn to_values<T: serde::Serialize>(items: Vec<T>) -> Vec<Value> {
    items
        .into_iter()
        .map(|item| serde_json::to_value(item).unwrap_or(Value::Null))
        .collect()
}

/// The three resource tools. `servers` returns the servers whose resources they reach, at call time.
pub fn create_mcp_resource_tool_definitions(
    options: &CreateMcpResourceToolDefinitionsOptions,
) -> anyhow::Result<Vec<ToolDefinition>> {
    let read_only = ToolAnnotations {
        read_only_hint: Some(true),
        ..Default::default()
    };
    let exposure = to_tool_exposure(&options.exposure);

    let servers = options.servers.clone();
    let mut list_resources = ToolDefinition::new(
        LIST_MCP_RESOURCES_TOOL,
        LIST_MCP_RESOURCES_TOOL,
        "Lists resources provided by MCP servers. Resources allow servers to share data that provides context to language models, such as files, database schemas, or application-specific information. Prefer resources over web search when possible.",
        list_parameters(),
        move |_id, params, signal, _on_update, _ctx| {
            let servers = servers.clone();
            async move {
                let page: PageFn = Arc::new(|server, cursor, options| {
                    Box::pin(async move {
                        let result = server.resources_page(cursor.as_deref(), options).await?;
                        Ok((to_values(result.resources), result.next_cursor))
                    })
                });
                let all: AllFn = Arc::new(|server, options| {
                    Box::pin(async move { Ok(to_values(server.all_resources(options).await?)) })
                });
                let payload =
                    list(&servers, &params, signal, "resources", page, all).await?;
                json_result(
                    LIST_MCP_RESOURCES_TOOL,
                    string_argument(&params, "server")?,
                    payload,
                )
                .await
            }
        },
    );
    list_resources.output_schema = Some(list_output_schema());
    list_resources.exposure = Some(exposure);
    list_resources.annotations = Some(read_only.clone());

    let servers = options.servers.clone();
    let mut list_templates = ToolDefinition::new(
        LIST_MCP_RESOURCE_TEMPLATES_TOOL,
        LIST_MCP_RESOURCE_TEMPLATES_TOOL,
        "Lists resource templates provided by MCP servers. Parameterized resource templates allow servers to share data that takes parameters and provides context to language models, such as files, database schemas, or application-specific information. Prefer resource templates over web search when possible.",
        list_parameters(),
        move |_id, params, signal, _on_update, _ctx| {
            let servers = servers.clone();
            async move {
                let page: PageFn = Arc::new(|server, cursor, options| {
                    Box::pin(async move {
                        let result = server
                            .resource_templates_page(cursor.as_deref(), options)
                            .await?;
                        Ok((to_values(result.resource_templates), result.next_cursor))
                    })
                });
                let all: AllFn = Arc::new(|server, options| {
                    Box::pin(async move {
                        Ok(to_values(server.all_resource_templates(options).await?))
                    })
                });
                let payload =
                    list(&servers, &params, signal, "resourceTemplates", page, all).await?;
                json_result(
                    LIST_MCP_RESOURCE_TEMPLATES_TOOL,
                    string_argument(&params, "server")?,
                    payload,
                )
                .await
            }
        },
    );
    list_templates.output_schema = Some(list_templates_output_schema());
    list_templates.exposure = Some(exposure);
    list_templates.annotations = Some(read_only.clone());

    let servers = options.servers.clone();
    let mut read_resource = ToolDefinition::new(
        READ_MCP_RESOURCE_TOOL,
        READ_MCP_RESOURCE_TOOL,
        "Read a specific resource from an MCP server given the server name and resource URI.",
        read_parameters(),
        move |_id, params, signal, _on_update, _ctx| {
            let servers = servers.clone();
            async move {
                let server_name = string_argument(&params, "server")?;
                let uri = string_argument(&params, "uri")?;
                let Some(server_name) = server_name else {
                    anyhow::bail!("server must be provided");
                };
                let Some(uri) = uri else {
                    anyhow::bail!("uri must be provided");
                };
                let server = find_server(&servers, &server_name)?;
                let name = server.name();
                let result = server
                    .read_resource(&uri, request_options(&signal, server.timeout_ms()))
                    .await?;
                // Several contents (for example a directory) are labeled with their URIs.
                let several = result.contents.len() > 1;
                let mut blocks = Vec::new();
                for contents in &result.contents {
                    if several {
                        blocks.push(ContentBlock::text(format!("{}:", contents.uri())));
                    }
                    blocks.push(ContentBlock::Resource(EmbeddedResourceContent {
                        resource: contents.clone(),
                        annotations: None,
                        meta: None,
                    }));
                }
                let mut converted = to_model_content(&name, &blocks, None).await?;
                if converted.is_empty() {
                    converted.push(UserContentBlock::text(format!("Resource {uri} is empty.")));
                }
                let limited = limit_mcp_content(&converted, None).await?;
                let contents: Vec<Value> = result
                    .contents
                    .iter()
                    .map(|contents| {
                        let mut value = serde_json::to_value(contents).unwrap_or(Value::Null);
                        if let Value::Object(object) = &mut value {
                            object.shift_remove("_meta");
                        }
                        value
                    })
                    .collect();
                let details = McpToolDetails {
                    server: name.clone(),
                    tool: READ_MCP_RESOURCE_TOOL.to_string(),
                    full_output_path: limited.full_output_path,
                };
                Ok(AgentToolResult {
                    content: limited.content,
                    details: Some(details.to_value()),
                    structured_content: Some(json!({
                        "server": name,
                        "uri": uri,
                        "contents": contents,
                    })),
                    ..Default::default()
                })
            }
        },
    );
    read_resource.output_schema = Some(read_output_schema());
    read_resource.exposure = Some(exposure);
    read_resource.annotations = Some(read_only);

    Ok(vec![list_resources, list_templates, read_resource])
}
